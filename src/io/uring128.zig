//! The SQE128/CQE32 io_uring binding (charter §12 "io_uring through Zig's
//! own liburing-equivalent bindings (SQE128/CQE32 …)"; decision 0003 §D
//! "SQE128"): the ring every FUSE-over-io_uring transport worker owns
//! (charter §4; porting spec fuse_uring_transport §1). Linux only.
//!
//! WHY a first-party binding: Zig 0.17's `std.os.linux.IoUring` mmaps the
//! SQE array as `sq_entries × 64` and indexes `[*]io_uring_sqe` at a 64 B
//! stride (`std/os/linux/IoUring.zig:1543–1553`). Under
//! `IORING_SETUP_SQE128` the kernel's SQE region is `sq_entries × 128` and
//! every SQE is 128 bytes with the 80 B command area at offset 48 — where
//! the FUSE uring command (`fuse_uring_cmd_req`, 24 B) travels. Pushing a
//! `URING_CMD` through std's `get_sqe` on an SQE128 ring would address the
//! wrong slot. `IORING_SETUP_CQE32` likewise doubles the CQE (two extra
//! u64: the `URING_CMD` result words).
//!
//! What this module is: the ring (`Ring128` — setup, the two mmaps at their
//! true strides, SQE take/flush, ONE enter shape with EXT_ARG timed waits
//! and eager GETEVENTS, CQE copy, registration, the op-support probe, the
//! raw register call a probe-resolved opcode composes), byte-exact SQE
//! builders (`prep*`), the `user_data` tag (`Tag`) and `ParkedRing128` —
//! `ring_park.Parked` over this ring, so the wake/park/dead protocol is the
//! SAME body the 64 B `ParkedRing` runs (decision 0002 row 12: never
//! copied).
//!
//! What it is not: FUSE. The REGISTER / COMMIT_AND_FETCH / RELEASE codec,
//! the kmbuf opcode ladder (37/38 on 6.19-sqz vs 38/39 on 7.1/7.2-sqz,
//! fuse_kmbuf_zc §2.1) and the slot table belong to the transport; this
//! module only guarantees the bytes land where the kernel reads them.

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const page_size_min = std.heap.page_size_min;

const log = @import("../core/log.zig");
const errors = @import("../core/errors.zig");
const ring_park = @import("ring_park.zig");

const root = @This();

comptime {
    if (builtin.os.tag != .linux) @compileError("uring128 is Linux only (charter §1.2); gate the import on builtin.os.tag");
    // `cmd_op` is the LOW u32 of the SQE's `off/addr2` union (uapi
    // io_uring.h `struct { __u32 cmd_op; __u32 __pad1; }`): byte +8 on the
    // little-endian targets this product builds for.
    if (builtin.cpu.arch.endian() != .little) @compileError("uring128 writes cmd_op as the low half of `off`; a big-endian target needs the union spelled out");
}

/// Fixed by `IORING_SETUP_SQE128` (uapi io_uring.h): the kernel's SQE stride.
pub const sqe_size = 128;
/// Fixed by `IORING_SETUP_CQE32`.
pub const cqe_size = 32;
/// The command area starts where the 64 B SQE's `addr3/__pad2` tail sits
/// (uapi: `__u8 cmd[0]` in that union) and runs to the end of the 128 B SQE.
pub const cmd_offset = 48;
pub const cmd_len = sqe_size - cmd_offset;
/// `IORING_MAX_ENTRIES` (io_uring/io_uring.c): the SQ ceiling; the CQ may
/// be twice it (`IORING_MAX_CQ_ENTRIES`), exactly the 2× law below.
pub const max_entries = 32768;

/// One 128 B submission queue entry. The first 64 bytes are std's
/// `io_uring_sqe` field for field (the `addr3`/`resv` tail is `cmd[0..16]`).
pub const Sqe128 = extern struct {
    opcode: linux.IORING_OP,
    /// `IOSQE_*`.
    flags: u8,
    ioprio: u16,
    /// The fd, or the fixed-file index under `IOSQE_FIXED_FILE`.
    fd: i32,
    /// `off` / `addr2`; for `URING_CMD` the low u32 is `cmd_op` and the
    /// high u32 (`__pad1`) must be zero.
    off: u64,
    /// `addr` (the iovec-array pointer of a FUSE REGISTER).
    addr: u64,
    len: u32,
    /// The per-op flags word: poll mask, fsync/cancel/timeout flags,
    /// `uring_cmd_flags`.
    rw_flags: u32,
    user_data: u64,
    /// The fixed-buffer slot (the kmbuf `fixed_buf_id` on a FUSE REGISTER).
    buf_index: u16,
    personality: u16,
    /// `splice_fd_in` / `file_index`.
    splice_fd_in: i32,
    /// The 80 B command area: `fuse_uring_cmd_req` at its head, zero past it.
    cmd: [cmd_len]u8,
};

comptime {
    std.debug.assert(@sizeOf(Sqe128) == sqe_size);
    std.debug.assert(@offsetOf(Sqe128, "flags") == 1);
    std.debug.assert(@offsetOf(Sqe128, "fd") == 4);
    std.debug.assert(@offsetOf(Sqe128, "off") == 8);
    std.debug.assert(@offsetOf(Sqe128, "addr") == 16);
    std.debug.assert(@offsetOf(Sqe128, "len") == 24);
    std.debug.assert(@offsetOf(Sqe128, "rw_flags") == 28);
    std.debug.assert(@offsetOf(Sqe128, "user_data") == 32);
    std.debug.assert(@offsetOf(Sqe128, "buf_index") == 40);
    std.debug.assert(@offsetOf(Sqe128, "splice_fd_in") == 44);
    std.debug.assert(@offsetOf(Sqe128, "cmd") == cmd_offset);
    std.debug.assert(@sizeOf(linux.io_uring_sqe) == 64);
    std.debug.assert(@offsetOf(linux.io_uring_sqe, "addr3") == cmd_offset);
}

/// One 32 B completion queue entry.
pub const Cqe32 = extern struct {
    user_data: u64,
    /// `-errno` when negative.
    res: i32,
    /// `IORING_CQE_F_*`; the kmbuf buffer id rides the high 16 bits under
    /// `IORING_CQE_F_BUFFER`.
    flags: u32,
    /// The two extra words of a 32 B CQE: a `URING_CMD`'s second result
    /// (`io_uring_cmd_done(…, res2)`); zero for every other op.
    big_cqe: [2]u64,

    pub fn err(self: Cqe32) linux.E {
        if (self.res > -4096 and self.res < 0) return @fromBackingInt(@as(u16, @intCast(-self.res)));
        return .SUCCESS;
    }
};

comptime {
    std.debug.assert(@sizeOf(Cqe32) == cqe_size);
    std.debug.assert(@offsetOf(Cqe32, "res") == 8);
    std.debug.assert(@offsetOf(Cqe32, "flags") == 12);
    std.debug.assert(@offsetOf(Cqe32, "big_cqe") == 16);
}

/// The ring. One owner thread builds, submits and reaps (the ladder's
/// `SINGLE_ISSUER` posture makes the kernel enforce it); `deinit` may run
/// on the joiner after the owner quiesced.
pub const Ring128 = struct {
    fd: linux.fd_t,
    /// The setup flags the kernel accepted — always carrying
    /// `SQE128 | CQE32 | CQSIZE` plus the ladder rung's posture.
    flags: u32,
    features: u32,
    sq: Sq,
    cq: Cq,

    pub const Sq = struct {
        head: *u32,
        tail: *u32,
        mask: u32,
        /// `IORING_SQ_*`: `CQ_OVERFLOW` and, under `TASKRUN_FLAG`, `TASKRUN`.
        flags: *u32,
        dropped: *u32,
        array: []u32,
        sqes: []Sqe128,
        ring_mmap: []align(page_size_min) u8,
        sqes_mmap: []align(page_size_min) u8,
        /// Taken-but-unflushed SQEs (liburing's `sqe_head`/`sqe_tail`): the
        /// release store on `tail` is paid once per flush, not once per take.
        sqe_head: u32 = 0,
        sqe_tail: u32 = 0,
    };

    pub const Cq = struct {
        head: *u32,
        tail: *u32,
        mask: u32,
        overflow: *u32,
        cqes: []Cqe32,
    };

    /// The verdict of one `io_uring_enter`. `timed_out` is authoritative
    /// only when the call submitted nothing: a submitting enter returns the
    /// submitted count and DISCARDS the wait's ETIME (io_uring.c keeps
    /// `ret2` only when `ret == 0`) — `ring_park`'s submit-first lesson.
    /// And `timed_out` is false whenever ANY CQE sits unreaped in the ring
    /// at expiry (`io_cqring_wait` returns `cq.head == cq.tail ? ret : 0`):
    /// a timed park with work already posted reads as an event.
    pub const Entered = struct { submitted: u32, timed_out: bool };

    /// `io_uring_setup(entries)` with `SQE128 | CQE32 | CQSIZE` and the
    /// rung's `flags`; `cq_entries = 2 × entries`. WHY 2×: a worst transport
    /// pass pushes `depth` commits + `depth` re-REGISTERs + the poll re-arm
    /// (fuse_uring_transport §1.1); the CQ must hold a full SQ of
    /// completions plus the deliveries that land while the worker reaps —
    /// `transport_cq_overflows` proves the geometry instead of assuming it.
    /// `entries` is a power of two ≤ `max_entries` (the caller derived it:
    /// `next_pow2(width·depth + 8).max(16)`).
    pub fn init(entries: u16, flags: u32) errors.Error!Ring128 {
        return setup(entries, @as(u32, entries) * 2, flags);
    }

    /// `init` with an EXPLICIT completion queue size and `IORING_SETUP_CLAMP`
    /// (decision 0003 §G "`Ring128` CQ"): the zcrx lane's derived CQ
    /// (`governor.cqEntriesFor`: `depth × per-command span completions +
    /// sq`) exceeds `2 × sq` by an order of magnitude, and an ask past the
    /// kernel's `IORING_MAX_CQ_ENTRIES` must clamp-then-round rather than
    /// refuse (ops_zcrx §1.2). `cq_entries` is a power of two ≥ `entries`;
    /// the kernel's echo (clamped) is what `cq.cqes.len` reports.
    pub fn initWithCq(entries: u16, cq_entries: u32, flags: u32) errors.Error!Ring128 {
        std.debug.assert(cq_entries >= entries and std.math.isPowerOfTwo(cq_entries));
        return setup(entries, cq_entries, flags | linux.IORING_SETUP_CLAMP);
    }

    fn setup(entries: u16, cq_entries: u32, flags: u32) errors.Error!Ring128 {
        std.debug.assert(entries != 0 and std.math.isPowerOfTwo(entries) and entries <= max_entries);
        var p = std.mem.zeroes(linux.io_uring_params);
        p.flags = flags | linux.IORING_SETUP_SQE128 | linux.IORING_SETUP_CQE32 | linux.IORING_SETUP_CQSIZE;
        p.cq_entries = cq_entries;
        const rc = linux.io_uring_setup(entries, &p);
        if (linux.errno(rc) != .SUCCESS) return errors.fromErrno(linux.errno(rc));
        const fd: linux.fd_t = @intCast(rc);
        errdefer _ = linux.close(fd);
        // One mmap carries both rings since 5.4; SQE128 is 5.19+, so a kernel
        // that accepted the flags has it — the check is the kernel's word.
        if (p.features & linux.IORING_FEAT_SINGLE_MMAP == 0) return error.NotSupported;
        // Powers of two in, the same powers of two out: nothing was rounded
        // (under CLAMP an oversize CQ ask comes back as the kernel's ceiling,
        // itself a power of two).
        std.debug.assert(p.sq_entries == entries);
        std.debug.assert(p.cq_entries == cq_entries or (flags & linux.IORING_SETUP_CLAMP != 0 and p.cq_entries < cq_entries));
        std.debug.assert(std.math.isPowerOfTwo(p.cq_entries));

        const ring_len = @max(
            p.sq_off.array + p.sq_entries * @sizeOf(u32),
            p.cq_off.cqes + p.cq_entries * @sizeOf(Cqe32),
        );
        const ring_mmap = try mapRing(fd, ring_len, linux.IORING_OFF_SQ_RING);
        errdefer unmap(ring_mmap);
        const sqes_mmap = try mapRing(fd, p.sq_entries * @sizeOf(Sqe128), linux.IORING_OFF_SQES);
        errdefer unmap(sqes_mmap);

        const array: [*]u32 = @ptrCast(@alignCast(&ring_mmap[p.sq_off.array]));
        const sqes: [*]Sqe128 = @ptrCast(@alignCast(sqes_mmap.ptr));
        const cqes: [*]Cqe32 = @ptrCast(@alignCast(&ring_mmap[p.cq_off.cqes]));
        std.debug.assert(p.sq_entries == @as(*u32, @ptrCast(@alignCast(&ring_mmap[p.sq_off.ring_entries]))).*);
        std.debug.assert(p.cq_entries == @as(*u32, @ptrCast(@alignCast(&ring_mmap[p.cq_off.ring_entries]))).*);
        return .{
            .fd = fd,
            .flags = p.flags,
            .features = p.features,
            .sq = .{
                .head = @ptrCast(@alignCast(&ring_mmap[p.sq_off.head])),
                .tail = @ptrCast(@alignCast(&ring_mmap[p.sq_off.tail])),
                .mask = @as(*u32, @ptrCast(@alignCast(&ring_mmap[p.sq_off.ring_mask]))).*,
                .flags = @ptrCast(@alignCast(&ring_mmap[p.sq_off.flags])),
                .dropped = @ptrCast(@alignCast(&ring_mmap[p.sq_off.dropped])),
                .array = array[0..p.sq_entries],
                .sqes = sqes[0..p.sq_entries],
                .ring_mmap = ring_mmap,
                .sqes_mmap = sqes_mmap,
            },
            .cq = .{
                .head = @ptrCast(@alignCast(&ring_mmap[p.cq_off.head])),
                .tail = @ptrCast(@alignCast(&ring_mmap[p.cq_off.tail])),
                .mask = @as(*u32, @ptrCast(@alignCast(&ring_mmap[p.cq_off.ring_mask]))).*,
                .overflow = @ptrCast(@alignCast(&ring_mmap[p.cq_off.overflow])),
                .cqes = cqes[0..p.cq_entries],
            },
        };
    }

    /// The mmaps depend on the fd: unmap first, close last.
    pub fn deinit(self: *Ring128) void {
        unmap(self.sq.sqes_mmap);
        unmap(self.sq.ring_mmap);
        _ = linux.close(self.fd);
        self.* = undefined;
    }

    /// A vacant SQE at the 128 B stride, or null when the SQ is full (the
    /// owner flushes with `submit` and retries once — `ring_park.getSqe`).
    pub fn getSqe(self: *Ring128) ?*Sqe128 {
        // Acquire pairs with the kernel's release of `head` after it copied
        // the SQEs out: a slot below `head` may be overwritten.
        const head = @atomicLoad(u32, self.sq.head, .acquire);
        const next = self.sq.sqe_tail +% 1;
        if (next -% head > self.sq.sqes.len) return null;
        const sqe = &self.sq.sqes[self.sq.sqe_tail & self.sq.mask];
        self.sq.sqe_tail = next;
        return sqe;
    }

    /// Publish every taken SQE to the kernel's ring (fills the index array,
    /// one release store on `tail`). Returns the SQEs the kernel has yet to
    /// consume — the `to_submit` of the enter that follows.
    pub fn flushSq(self: *Ring128) u32 {
        if (self.sq.sqe_head != self.sq.sqe_tail) {
            const to_submit = self.sq.sqe_tail -% self.sq.sqe_head;
            var tail = self.sq.tail.*;
            var i: u32 = 0;
            while (i < to_submit) : (i += 1) {
                self.sq.array[tail & self.sq.mask] = self.sq.sqe_head & self.sq.mask;
                tail +%= 1;
                self.sq.sqe_head +%= 1;
            }
            // Release pairs with the kernel's acquire of `tail`: every SQE
            // body and array slot written above is visible before the count.
            @atomicStore(u32, self.sq.tail, tail, .release);
        }
        return self.sqReady();
    }

    /// SQEs taken and not yet consumed by the kernel (flushed or not).
    pub fn sqReady(self: *Ring128) u32 {
        return self.sq.sqe_tail -% @atomicLoad(u32, self.sq.head, .acquire);
    }

    /// Vacant SQEs: how many pushes the pass may still make before a flush.
    pub fn sqSpaceLeft(self: *Ring128) u32 {
        return @as(u32, @intCast(self.sq.sqes.len)) - self.sqReady();
    }

    /// CQEs posted and not yet copied. On a `DEFER_TASKRUN` ring another
    /// context's completion is posted only inside this thread's GETEVENTS
    /// enter, so 0 here does not mean nothing finished — `sqFlags()`'s
    /// `IORING_SQ_TASKRUN` says there is work to run.
    pub fn cqReady(self: *Ring128) u32 {
        // Acquire pairs with the kernel's release of `tail` after writing the CQE.
        return @atomicLoad(u32, self.cq.tail, .acquire) -% self.cq.head.*;
    }

    /// `IORING_SQ_*` as the kernel last published them: the spin governor's
    /// one acquire load (`TASKRUN` under `TASKRUN_FLAG`; `CQ_OVERFLOW`).
    pub fn sqFlags(self: *Ring128) u32 {
        return @atomicLoad(u32, self.sq.flags, .acquire);
    }

    /// `io_uring_enter`. `arg` (an EXT_ARG timeout) sets
    /// `IORING_ENTER_EXT_ARG` itself. `ETIME` is a verdict, not an error:
    /// `Entered.timed_out` (see its doc for when it is trustworthy).
    pub fn enter(self: *Ring128, to_submit: u32, min_complete: u32, flags: u32, arg: ?*const linux.io_uring_getevents_arg) errors.Error!Entered {
        const argp: usize = if (arg) |a| @intFromPtr(a) else 0;
        const argsz: usize = if (arg != null) @sizeOf(linux.io_uring_getevents_arg) else 0;
        var enter_flags: u32 = flags | (if (arg != null) @as(u32, linux.IORING_ENTER_EXT_ARG) else 0);
        // SQPOLL (the transport's A/B lever): an idle poller must be woken
        // to see the flushed tail; the kernel raises NEED_WAKEUP in
        // `sq.flags` when it went to sleep.
        if (self.flags & linux.IORING_SETUP_SQPOLL != 0 and self.sqFlags() & linux.IORING_SQ_NEED_WAKEUP != 0) enter_flags |= linux.IORING_ENTER_SQ_WAKEUP;
        // The kernel reads `arg` and its timespec during this call only.
        const rc = linux.syscall6(.io_uring_enter, @as(usize, @intCast(self.fd)), to_submit, min_complete, enter_flags, argp, argsz);
        return switch (linux.errno(rc)) {
            .SUCCESS => .{ .submitted = @intCast(rc), .timed_out = false },
            .TIME => .{ .submitted = 0, .timed_out = true },
            else => |e| errors.fromErrno(e),
        };
    }

    /// Flush and submit, no wait. On a `DEFER_TASKRUN` ring this runs no
    /// task work: completions posted by other contexts stay invisible until
    /// a GETEVENTS enter (`submitAndWaitTimed(…, 0)` is the eager flush).
    pub fn submit(self: *Ring128) errors.Error!u32 {
        return (try self.enter(self.flushSq(), 0, 0, null)).submitted;
    }

    /// Flush, submit and wait for `wait_nr` CQEs (unbounded).
    pub fn submitAndWait(self: *Ring128, wait_nr: u32) errors.Error!u32 {
        return (try self.enter(self.flushSq(), wait_nr, linux.IORING_ENTER_GETEVENTS, null)).submitted;
    }

    /// THE pass enter (charter §4 "ONE `io_uring_enter` carrying every SQE
    /// and the wait"): flush, submit and GETEVENTS-wait for `min_complete`
    /// CQEs for at most `tick_ns`. `tick_ns == 0` is the eager non-blocking
    /// GETEVENTS flush a `DEFER_TASKRUN` ring needs mid-pass. `timed_out`
    /// is trustworthy only on a pass that submitted nothing — a pass that
    /// submitted and timed out reads as `submitted > 0` and re-loops, which
    /// is why `ring_park.park` splits the TIMED park into two enters.
    /// EXT_ARG is 5.11+, inside SQE128's 5.19 floor: no feature check.
    pub fn submitAndWaitTimed(self: *Ring128, min_complete: u32, tick_ns: u64) errors.Error!Entered {
        var ts: linux.kernel_timespec = .{ .sec = @intCast(tick_ns / std.time.ns_per_s), .nsec = @intCast(tick_ns % std.time.ns_per_s) };
        const arg: linux.io_uring_getevents_arg = .{ .sigmask = 0, .sigmask_sz = 0, .pad = 0, .ts = @intFromPtr(&ts) };
        return self.enter(self.flushSq(), min_complete, linux.IORING_ENTER_GETEVENTS, &arg);
    }

    /// Copy as many ready CQEs as fit; when none are ready, enter to wait
    /// for `wait_nr` (0 ⇒ only when the kernel flagged overflow or pending
    /// task work). Advances the CQ head once per batch.
    pub fn copyCqes(self: *Ring128, cqes: []Cqe32, wait_nr: u32) errors.Error!u32 {
        const count = self.copyReady(cqes);
        if (count > 0) return count;
        if (self.cqNeedsFlush() or wait_nr > 0) {
            _ = try self.enter(0, wait_nr, linux.IORING_ENTER_GETEVENTS, null);
            return self.copyReady(cqes);
        }
        return 0;
    }

    fn copyReady(self: *Ring128, cqes: []Cqe32) u32 {
        const ready = self.cqReady();
        const count: u32 = @intCast(@min(cqes.len, ready));
        const head = self.cq.head.* & self.cq.mask;
        const n: u32 = @intCast(@min(self.cq.cqes.len - head, count));
        @memcpy(cqes[0..n], self.cq.cqes[head..][0..n]);
        if (count > n) {
            const w = count - n;
            @memcpy(cqes[n..][0..w], self.cq.cqes[0..w]);
        }
        self.cqAdvance(count);
        return count;
    }

    fn cqAdvance(self: *Ring128, count: u32) void {
        // Release pairs with the kernel's acquire of `head`: the slots are
        // free for reuse only after the copies above completed.
        if (count > 0) @atomicStore(u32, self.cq.head, self.cq.head.* +% count, .release);
    }

    /// liburing's `cq_ring_needs_flush`: the kernel parked CQEs in its
    /// overflow list, or (`DEFER_TASKRUN` + `TASKRUN_FLAG`) holds deferred
    /// completions only an enter materialises.
    fn cqNeedsFlush(self: *Ring128) bool {
        return self.sqFlags() & (linux.IORING_SQ_CQ_OVERFLOW | linux.IORING_SQ_TASKRUN) != 0;
    }

    // ---- registration -----------------------------------------------------

    /// `io_uring_register` with the opcode as a number, so a PROBE-resolved
    /// opcode (the kmbuf pair, charter §12) composes without a binding
    /// change. Every typed registration below is this call.
    pub fn registerRaw(self: *Ring128, opcode: u32, arg: ?*const anyopaque, nr_args: u32) errors.Error!void {
        const rc = linux.io_uring_register(self.fd, @fromBackingInt(opcode), arg, nr_args);
        if (linux.errno(rc) != .SUCCESS) return errors.fromErrno(linux.errno(rc));
    }

    /// The fixed-file table (`[fuse_fd, wake_eventfd]` on a transport
    /// ring); -1 leaves a slot sparse for `registerFilesUpdate`.
    pub fn registerFiles(self: *Ring128, fds: []const linux.fd_t) errors.Error!void {
        try self.registerRaw(@backingInt(linux.IORING_REGISTER.REGISTER_FILES), @ptrCast(fds.ptr), @intCast(fds.len));
    }

    /// Replace table slots from `offset` (-1 clears one).
    pub fn registerFilesUpdate(self: *Ring128, offset: u32, fds: []const linux.fd_t) errors.Error!void {
        const update: linux.io_uring_rsrc_update = .{ .offset = offset, .resv = 0, .data = @intFromPtr(fds.ptr) };
        try self.registerRaw(@backingInt(linux.IORING_REGISTER.REGISTER_FILES_UPDATE), &update, @intCast(fds.len));
    }

    pub fn unregisterFiles(self: *Ring128) errors.Error!void {
        try self.registerRaw(@backingInt(linux.IORING_REGISTER.UNREGISTER_FILES), null, 0);
    }

    /// A fixed-buffer table of `n` EMPTY slots (`IORING_RSRC_REGISTER_SPARSE`,
    /// 5.19+): the zc shape — one slot per ring entry the kernel fills with a
    /// request's pages at delivery (fuse_kmbuf_zc §4.1), the headers buffer
    /// at index `depth` through `registerBuffersUpdate`.
    pub fn registerBuffersSparse(self: *Ring128, n: u32) errors.Error!void {
        const reg: linux.io_uring_rsrc_register = .{ .nr = n, .flags = linux.IORING_RSRC_REGISTER_SPARSE, .resv2 = 0, .data = 0, .tags = 0 };
        try self.registerRaw(@backingInt(linux.IORING_REGISTER.REGISTER_BUFFERS2), &reg, @sizeOf(linux.io_uring_rsrc_register));
    }

    /// Fill table slots from `offset` with `iovecs`. The pages are pinned
    /// against `RLIMIT_MEMLOCK` unless the process holds `CAP_IPC_LOCK`.
    pub fn registerBuffersUpdate(self: *Ring128, offset: u32, iovecs: []const std.posix.iovec) errors.Error!void {
        const update: linux.io_uring_rsrc_update2 = .{ .offset = offset, .resv = 0, .data = @intFromPtr(iovecs.ptr), .tags = 0, .nr = @intCast(iovecs.len), .resv2 = 0 };
        try self.registerRaw(@backingInt(linux.IORING_REGISTER.REGISTER_BUFFERS_UPDATE), &update, @sizeOf(linux.io_uring_rsrc_update2));
    }

    pub fn unregisterBuffers(self: *Ring128) errors.Error!void {
        try self.registerRaw(@backingInt(linux.IORING_REGISTER.UNREGISTER_BUFFERS), null, 0);
    }

    /// The ring's completion eventfd (a foreign poller's view of this ring).
    pub fn registerEventfd(self: *Ring128, efd: linux.fd_t) errors.Error!void {
        try self.registerRaw(@backingInt(linux.IORING_REGISTER.REGISTER_EVENTFD), &efd, 1);
    }

    pub fn unregisterEventfd(self: *Ring128) errors.Error!void {
        try self.registerRaw(@backingInt(linux.IORING_REGISTER.UNREGISTER_EVENTFD), null, 0);
    }

    /// Enable a ring built with `IORING_SETUP_R_DISABLED`: under
    /// `SINGLE_ISSUER` the CALLING thread becomes the ring's one submitter,
    /// which is how a ring built on one thread is handed to its worker.
    pub fn enableRings(self: *Ring128) errors.Error!void {
        try self.registerRaw(@backingInt(linux.IORING_REGISTER.REGISTER_ENABLE_RINGS), null, 0);
    }

    /// The kernel's op-support bitmap (charter §3: probe, never a version
    /// table): `probe.is_supported(.URING_CMD)`.
    pub fn getProbe(self: *Ring128) errors.Error!linux.io_uring_probe {
        var probe = std.mem.zeroInit(linux.io_uring_probe, .{});
        try self.registerRaw(@backingInt(linux.IORING_REGISTER.REGISTER_PROBE), &probe, probe.ops.len);
        return probe;
    }

    /// Register a buffer ring through a caller-resolved opcode: stock
    /// `REGISTER_PBUF_RING` (22) with `ring_addr` a user mapping, or the
    /// kmbuf pair's register opcode (a probe verdict — fuse_kmbuf_zc §2.1)
    /// with `BufReg.ring_addr`'s low u32 read as `buf_size`. The 40 B
    /// argument rides in a ZEROED 256 B scratch: a foreign occupant of a
    /// probed opcode that reads a wider struct (7.1's `io_uring_bpf` reads
    /// 72 B) sees zeros, never stack garbage.
    pub fn registerBufRing(self: *Ring128, opcode: u32, reg: BufReg) errors.Error!void {
        var scratch: [256]u8 = @splat(0);
        @memcpy(scratch[0..@sizeOf(BufReg)], std.mem.asBytes(&reg));
        try self.registerRaw(opcode, &scratch, 1);
    }

    /// Map a kernel-owned region of this ring at a magic mmap offset (the
    /// kmbuf buffer region at `IORING_OFF_KMBUF_RING | (bgid << 16)`):
    /// shared, populated, read-write. Released with `munmapRegion`.
    pub fn mmapRegion(self: *Ring128, offset: u64, len: usize) errors.Error![]align(page_size_min) u8 {
        return mapRing(self.fd, len, offset);
    }

    pub fn munmapRegion(region: []align(page_size_min) u8) void {
        unmap(region);
    }
};

/// `struct io_uring_buf_reg` (uapi, 40 B). Stock `REGISTER_PBUF_RING`
/// reads `ring_addr` whole; the sqz kmbuf tracks read its low u32 as
/// `buf_size` (page-aligned) — the union `{ u64 ring_addr; u32 buf_size; }`
/// of the kernel series, flattened.
pub const BufReg = extern struct {
    ring_addr: u64,
    /// A power of two below 65536.
    ring_entries: u32,
    bgid: u16,
    flags: u16,
    resv: [3]u64,
};

comptime {
    std.debug.assert(@sizeOf(BufReg) == 40);
    std.debug.assert(@offsetOf(BufReg, "ring_entries") == 8);
    std.debug.assert(@offsetOf(BufReg, "bgid") == 12);
    std.debug.assert(@offsetOf(BufReg, "flags") == 14);
}

fn mapRing(fd: linux.fd_t, len: usize, offset: u64) errors.Error![]align(page_size_min) u8 {
    const rc = linux.mmap(null, len, .{ .READ = true, .WRITE = true }, .{ .TYPE = .SHARED, .POPULATE = true }, fd, @intCast(offset));
    if (linux.errno(rc) != .SUCCESS) return errors.fromErrno(linux.errno(rc));
    const ptr: [*]align(page_size_min) u8 = @ptrFromInt(rc);
    return ptr[0..len];
}

fn unmap(region: []align(page_size_min) u8) void {
    _ = linux.munmap(region.ptr, region.len);
}

// ---------------------------------------------------------------------------
// SQE builders — byte-exact. Each zeroes the whole 128 B first so a slot's
// previous occupant never leaks into a field the new op leaves unset (the
// kernel refuses non-zero bytes it does not expect: `__pad1`, `cmd` tails).
// `user_data` is the caller's, set after the prep like std's builders.
// ---------------------------------------------------------------------------

/// The file an SQE names: a raw fd, or a fixed-file table index
/// (`IOSQE_FIXED_FILE`) — index 0 is the fuse fd on a transport ring.
pub const File = union(enum) { raw: linux.fd_t, fixed: u32 };

fn setFile(sqe: *Sqe128, file: File) void {
    switch (file) {
        .raw => |fd| sqe.fd = fd,
        .fixed => |index| {
            sqe.fd = @intCast(index);
            sqe.flags |= linux.IOSQE_FIXED_FILE;
        },
    }
}

fn prepRw(sqe: *Sqe128, op: linux.IORING_OP, file: File, addr: u64, len: u32, off: u64) void {
    sqe.* = std.mem.zeroes(Sqe128);
    sqe.opcode = op;
    setFile(sqe, file);
    sqe.addr = addr;
    sqe.len = len;
    sqe.off = off;
}

pub fn prepNop(sqe: *Sqe128) void {
    sqe.* = std.mem.zeroes(Sqe128);
    sqe.opcode = .NOP;
}

/// `FUTEX_WAITV` over `waiters` (Linux ≥ 6.7; `len` carries the count and
/// `fd`, `addr2`, `addr3`, the futex flags and the buffer index must be 0
/// — `io_futexv_prep` answers EINVAL otherwise): the CQE's `res` is the
/// index of the woken word, at once when a value already differs. The
/// kernel copies the array at submission, so it need only outlive the enter.
pub fn prepFutexWaitv(sqe: *Sqe128, waiters: []const linux.futex2_waitone) void {
    sqe.* = std.mem.zeroes(Sqe128);
    sqe.opcode = .FUTEX_WAITV;
    sqe.len = @intCast(waiters.len);
    sqe.addr = @intFromPtr(waiters.ptr);
}

pub fn prepRead(sqe: *Sqe128, file: File, buf: []u8, off: u64) void {
    prepRw(sqe, .READ, file, @intFromPtr(buf.ptr), @intCast(buf.len), off);
}

pub fn prepWrite(sqe: *Sqe128, file: File, buf: []const u8, off: u64) void {
    prepRw(sqe, .WRITE, file, @intFromPtr(buf.ptr), @intCast(buf.len), off);
}

/// `READ_FIXED` of `len` bytes at file offset `off` into registered slot
/// `buf_index` at `addr`: a user address inside the registered iovec, or —
/// for a kernel-registered zc slot (`imu->ubuf == 0`) — the BYTE OFFSET into
/// the slot (the ublk convention; fuse_kmbuf_zc §4.1).
pub fn prepReadFixed(sqe: *Sqe128, file: File, buf_index: u16, off: u64, len: u32, addr: u64) void {
    prepRw(sqe, .READ_FIXED, file, addr, len, off);
    sqe.buf_index = buf_index;
}

pub fn prepWriteFixed(sqe: *Sqe128, file: File, buf_index: u16, off: u64, len: u32, addr: u64) void {
    prepRw(sqe, .WRITE_FIXED, file, addr, len, off);
    sqe.buf_index = buf_index;
}

/// One-shot, level-triggered `POLL_ADD` (`mask` is `POLL.IN` etc.).
pub fn prepPollAdd(sqe: *Sqe128, file: File, mask: u32) void {
    prepRw(sqe, .POLL_ADD, file, 0, 0, 0);
    // The poll mask is defined little-endian on the wire (it grew from 16
    // to 32 bits; the kernel reads it with `READ_ONCE(sqe->poll32_events)`
    // after `swahw32` on big-endian).
    sqe.rw_flags = std.mem.nativeToLittle(u32, mask);
}

/// What an `ASYNC_CANCEL` targets: the request with this `user_data` (the
/// first match), every request with it (`user_data_all`: a bridge pushed
/// in pieces), or every request on a fixed-file index (a transport
/// bridge's slot ops).
pub const CancelTarget = union(enum) { user_data: u64, user_data_all: u64, fixed: u32 };

pub fn prepAsyncCancel(sqe: *Sqe128, target: CancelTarget) void {
    sqe.* = std.mem.zeroes(Sqe128);
    sqe.opcode = .ASYNC_CANCEL;
    sqe.fd = -1;
    switch (target) {
        .user_data => |ud| sqe.addr = ud,
        .user_data_all => |ud| {
            sqe.addr = ud;
            sqe.rw_flags = linux.IORING_ASYNC_CANCEL_ALL;
        },
        .fixed => |index| {
            sqe.fd = @intCast(index);
            sqe.rw_flags = linux.IORING_ASYNC_CANCEL_FD | linux.IORING_ASYNC_CANCEL_FD_FIXED;
        },
    }
}

pub fn prepFsync(sqe: *Sqe128, file: File, datasync: bool) void {
    sqe.* = std.mem.zeroes(Sqe128);
    sqe.opcode = .FSYNC;
    setFile(sqe, file);
    sqe.rw_flags = if (datasync) linux.IORING_FSYNC_DATASYNC else 0;
}

/// A `TIMEOUT` op (`count` completions or `ts`, whichever first; `flags`
/// are `IORING_TIMEOUT_*`). `ts` must outlive the op.
pub fn prepTimeout(sqe: *Sqe128, ts: *const linux.kernel_timespec, count: u32, flags: u32) void {
    prepRw(sqe, .TIMEOUT, .{ .raw = -1 }, @intFromPtr(ts), 1, count);
    sqe.rw_flags = flags;
}

/// `URING_CMD`: `cmd_op` in the low u32 at +8, the 80 command bytes at +48
/// (a FUSE REGISTER's iovec `addr`/`len` and its `buf_index` are the
/// caller's to set on the returned SQE — they are plain fields).
pub fn prepUringCmd(sqe: *Sqe128, file: File, cmd_op: u32, cmd: *const [cmd_len]u8) void {
    sqe.* = std.mem.zeroes(Sqe128);
    sqe.opcode = .URING_CMD;
    setFile(sqe, file);
    sqe.off = cmd_op;
    sqe.cmd = cmd.*;
}

/// One-shot `RECV` of up to `buf.len` bytes into `buf` (`msg_flags` in
/// `rw_flags`): the zcrx lane's venue leg — a granted area chunk stands in
/// for the provider pool (ops_zcrx §12.1.11).
pub fn prepRecv(sqe: *Sqe128, file: File, buf: []u8, msg_flags: u32) void {
    prepRw(sqe, .RECV, file, @intFromPtr(buf.ptr), @intCast(buf.len), 0);
    sqe.rw_flags = msg_flags;
}

/// `SEND` of `buf` (`msg_flags` in `rw_flags`; `MSG_NOSIGNAL` is the lane's
/// posture — a peer that closed must surface as a CQE, never a signal).
pub fn prepSend(sqe: *Sqe128, file: File, buf: []const u8, msg_flags: u32) void {
    prepRw(sqe, .SEND, file, @intFromPtr(buf.ptr), @intCast(buf.len), 0);
    sqe.rw_flags = msg_flags;
}

/// Multishot `RECV_ZC` on `file` bound to the registered ifq `zcrx_id`
/// (ops_zcrx §1.2: opcode 58, `ioprio = IORING_RECV_MULTISHOT`, the ifq
/// index in the `splice_fd_in`/`file_index` union slot at +44, `len = 0`
/// = unbounded). Payload lands in the area; the CQE's big word carries
/// `(area_token_hi << 48) | byte offset` (`zcrxOff`).
pub fn prepRecvZc(sqe: *Sqe128, file: File, zcrx_id: u32) void {
    prepRw(sqe, .RECV_ZC, file, 0, 0, 0);
    sqe.ioprio = linux.IORING_RECV_MULTISHOT;
    sqe.splice_fd_in = @bitCast(zcrx_id);
}

/// The zcrx big word of a 32 B CQE: `struct io_uring_zcrx_cqe { __u64 off;
/// __u64 __pad; }` at CQE offset 16 — `big_cqe[0]`.
pub fn zcrxOff(cqe: *const Cqe32) u64 {
    return cqe.big_cqe[0];
}

// ---------------------------------------------------------------------------
// user_data tags.
// ---------------------------------------------------------------------------

/// A slot addressed by value in 64 bits (charter §4: other threads name a
/// slot as `(qid, ent_idx, commit_id)`): `kind` is the op class, `a`/`b`/`c`
/// the transport's coordinates. The top `kind` (0xFF) is the park
/// protocol's: `ring_park.wake_ud` and `reserved_ud` decode there, so a
/// request tag never collides with the standing wake or its cancel.
pub const Tag = packed struct(u64) {
    c: u24,
    b: u16,
    a: u16,
    kind: u8,

    pub const reserved_kind: u8 = std.math.maxInt(u8);

    pub fn pack(self: Tag) u64 {
        std.debug.assert(self.kind != reserved_kind);
        return @bitCast(self);
    }

    pub fn unpack(ud: u64) Tag {
        return @bitCast(ud);
    }

    /// The protocol's own values (`>= ring_park.reserved_ud`): never a request.
    pub fn isReserved(ud: u64) bool {
        return ud >= ring_park.reserved_ud;
    }
};

comptime {
    std.debug.assert(@bitSizeOf(Tag) == 64);
    std.debug.assert(Tag.unpack(ring_park.wake_ud).kind == Tag.reserved_kind);
    std.debug.assert(Tag.unpack(ring_park.reserved_ud).kind == Tag.reserved_kind);
    // The largest request tag sits strictly below every reserved value.
    const top: Tag = .{ .kind = Tag.reserved_kind - 1, .a = std.math.maxInt(u16), .b = std.math.maxInt(u16), .c = std.math.maxInt(u24) };
    std.debug.assert(top.pack() < ring_park.reserved_ud);
}

// ---------------------------------------------------------------------------
// The posture ladder and the parked form.
// ---------------------------------------------------------------------------

pub const Rung = struct { flags: u32, name: []const u8 };

/// The transport worker's setup posture, probed top-down at ring build
/// (charter §4 "`SINGLE_ISSUER|DEFER_TASKRUN|TASKRUN_FLAG` where
/// buildable"; fuse_uring_transport §1.1): `SINGLE_ISSUER` is free (one
/// thread builds, submits and reaps); `DEFER_TASKRUN` (6.1+) runs
/// completion task-work inside this thread's own enter instead of IPIs;
/// `TASKRUN_FLAG` (5.19+) raises `IORING_SQ_TASKRUN` so a spinning worker
/// sees a completion with one acquire load; `COOP_TASKRUN` is the milder
/// no-IPI variant for a kernel without `DEFER_TASKRUN`. The last rung is a
/// plain ring; `SQE128 | CQE32 | CQSIZE` ride every rung inside
/// `Ring128.init`.
pub const taskrun_ladder = [_]Rung{
    .{ .flags = linux.IORING_SETUP_SINGLE_ISSUER | linux.IORING_SETUP_DEFER_TASKRUN | linux.IORING_SETUP_TASKRUN_FLAG, .name = "single_issuer+defer_taskrun+taskrun_flag" },
    .{ .flags = linux.IORING_SETUP_SINGLE_ISSUER | linux.IORING_SETUP_COOP_TASKRUN | linux.IORING_SETUP_TASKRUN_FLAG, .name = "single_issuer+coop_taskrun+taskrun_flag" },
    .{ .flags = linux.IORING_SETUP_SINGLE_ISSUER, .name = "single_issuer" },
    .{ .flags = 0, .name = "plain" },
};

/// `taskrun_ladder`'s flags alone: what `ParkedRing128.initProbing` walks.
pub const taskrun_ladder_flags: [taskrun_ladder.len]u32 = blk: {
    var flags: [taskrun_ladder.len]u32 = undefined;
    for (taskrun_ladder, 0..) |rung, i| flags[i] = rung.flags;
    break :blk flags;
};

/// `Ring128` behind `ring_park.Parked`'s face.
pub const ParkFace = struct {
    pub const Ring = Ring128;
    pub const Sqe = Sqe128;
    pub const Cqe = Cqe32;

    pub fn setup(entries: u16, flags: u32) errors.Error!Ring128 {
        return Ring128.init(entries, flags);
    }

    pub fn deinit(ring: *Ring128) void {
        ring.deinit();
    }

    pub fn getSqe(ring: *Ring128) ?*Sqe128 {
        return ring.getSqe();
    }

    pub fn submit(ring: *Ring128) errors.Error!u32 {
        return ring.submit();
    }

    pub fn submitAndWait(ring: *Ring128, wait_nr: u32) errors.Error!u32 {
        return ring.submitAndWait(wait_nr);
    }

    pub fn copyCqes(ring: *Ring128, cqes: []Cqe32, wait_nr: u32) errors.Error!u32 {
        return ring.copyCqes(cqes, wait_nr);
    }

    pub fn cqReady(ring: *Ring128) u32 {
        return ring.cqReady();
    }

    pub fn registerFiles(ring: *Ring128, fds: []const linux.fd_t) bool {
        ring.registerFiles(fds) catch return false;
        return true;
    }

    pub fn prepRead(sqe: *Sqe128, fd: linux.fd_t, buf: []u8, off: u64) void {
        root.prepRead(sqe, .{ .raw = fd }, buf, off);
    }

    pub fn prepCancel(sqe: *Sqe128, target: u64) void {
        root.prepAsyncCancel(sqe, .{ .user_data = target });
    }

    pub fn prepFdatasync(sqe: *Sqe128, fd: linux.fd_t) void {
        root.prepFsync(sqe, .{ .raw = fd }, true);
    }
};

/// The park/wake/dead protocol over the SQE128/CQE32 ring: what a
/// transport worker embeds (`ring: Ring128` is its SQE/CQE face; `park`,
/// `armWake`, `consumeWake`, `service`, `quiesce` are `ring_park`'s).
pub const ParkedRing128 = ring_park.Parked(ParkFace);

// ---------------------------------------------------------------------------
// Tests: the contract (venue: root on the 7.2.6-sqz kernel).
// ---------------------------------------------------------------------------

const testing = std.testing;

/// A scratch file `/build/tmp/uring128-<pid>-<stem>.dat`: ext4 under /build
/// (the venue's cache volume), never the virtiofs mount; pid-tagged, so two
/// test processes on the venue (a gate's shards, another agent's run) never
/// share one. The caller closes `fd` and unlinks `path`.
const Scratch = struct {
    fd: linux.fd_t,
    path: [:0]const u8,
    buf: [64]u8,

    fn open(self: *Scratch, stem: []const u8) !void {
        _ = linux.mkdir("/build/tmp", 0o755);
        self.path = try std.fmt.bufPrintSentinel(&self.buf, "/build/tmp/uring128-{d}-{s}.dat", .{ linux.getpid(), stem }, 0);
        const rc = linux.open(self.path.ptr, .{ .ACCMODE = .RDWR, .CREAT = true, .TRUNC = true, .CLOEXEC = true }, 0o644);
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
        self.fd = @intCast(rc);
    }

    fn close(self: *const Scratch) void {
        _ = linux.close(self.fd);
        _ = linux.unlink(self.path.ptr);
    }
};

fn newEventfd() !linux.fd_t {
    const rc = linux.eventfd(0, linux.EFD.CLOEXEC | linux.EFD.NONBLOCK);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
    return @intCast(rc);
}

fn writeEventfd(fd: linux.fd_t) void {
    const one: u64 = 1;
    _ = linux.write(fd, @ptrCast(&one), @sizeOf(u64));
}

fn readEventfd(fd: linux.fd_t) ?u64 {
    var count: u64 = 0;
    const rc = linux.read(fd, @ptrCast(&count), @sizeOf(u64));
    if (linux.errno(rc) != .SUCCESS) return null;
    return count;
}

fn negErrno(e: linux.E) i32 {
    return -@as(i32, @backingInt(e));
}

/// Reap exactly `n` CQEs (waiting), in arrival order.
fn reap(ring: *Ring128, out: []Cqe32) !void {
    var got: usize = 0;
    while (got < out.len) {
        const k = try ring.copyCqes(out[got..], 1);
        got += k;
    }
}

test "setup with SQE128|CQE32 succeeds; the CQ is twice the SQ; getSqe hands out 128 B-strided slots and a byte written in the cmd area lands at index × 128 + 48 in the raw mmap" {
    var ring = try Ring128.init(16, 0);
    defer ring.deinit();
    try testing.expect(ring.flags & linux.IORING_SETUP_SQE128 != 0);
    try testing.expect(ring.flags & linux.IORING_SETUP_CQE32 != 0);
    try testing.expectEqual(@as(usize, 16), ring.sq.sqes.len);
    try testing.expectEqual(@as(usize, 32), ring.cq.cqes.len);
    try testing.expectEqual(@as(usize, 16 * sqe_size), ring.sq.sqes_mmap.len);
    try testing.expectEqual(@as(u32, 16), ring.sqSpaceLeft());

    const s0 = ring.getSqe().?;
    const s1 = ring.getSqe().?;
    try testing.expectEqual(@as(usize, sqe_size), @intFromPtr(s1) - @intFromPtr(s0));
    try testing.expectEqual(@as(u32, 14), ring.sqSpaceLeft());

    prepNop(s0);
    for (&s0.cmd, 0..) |*b, i| b.* = @truncate(0xC0 + i);
    const raw: [*]const u8 = @ptrCast(ring.sq.sqes.ptr);
    for (0..cmd_len) |i| try testing.expectEqual(@as(u8, @truncate(0xC0 + i)), raw[0 * sqe_size + cmd_offset + i]);
    try testing.expectEqual(@backingInt(linux.IORING_OP.NOP), raw[0]);

    // A FUSE-shaped URING_CMD on fixed file 0: cmd_op at +8, qid at +64,
    // init.flags at +66, init.queue_depth at +68 (fuse_uring_transport §1.5).
    var cmd: [cmd_len]u8 = @splat(0);
    std.mem.writeInt(u16, cmd[16..18], 0x1234, .little);
    std.mem.writeInt(u16, cmd[18..20], 0x0005, .little);
    std.mem.writeInt(u16, cmd[20..22], 0x0020, .little);
    prepUringCmd(s1, .{ .fixed = 0 }, 2, &cmd);
    s1.user_data = 0xABCD;
    const r1 = raw[sqe_size .. 2 * sqe_size];
    try testing.expectEqual(@backingInt(linux.IORING_OP.URING_CMD), r1[0]);
    try testing.expect(r1[1] & linux.IOSQE_FIXED_FILE != 0);
    try testing.expectEqual(@as(i32, 0), std.mem.readInt(i32, r1[4..8], .little));
    try testing.expectEqual(@as(u32, 2), std.mem.readInt(u32, r1[8..12], .little));
    try testing.expectEqual(@as(u32, 0), std.mem.readInt(u32, r1[12..16], .little));
    try testing.expectEqual(@as(u64, 0xABCD), std.mem.readInt(u64, r1[32..40], .little));
    try testing.expectEqual(@as(u16, 0x1234), std.mem.readInt(u16, r1[64..66], .little));
    try testing.expectEqual(@as(u16, 0x0005), std.mem.readInt(u16, r1[66..68], .little));
    try testing.expectEqual(@as(u16, 0x0020), std.mem.readInt(u16, r1[68..70], .little));
    try testing.expectEqualSlices(u8, &cmd, r1[cmd_offset..]);
    // Nothing was flushed: the ring tears down with two taken, unsubmitted slots.
}

test "initWithCq: an explicit CQ of 64 × the SQ is honoured; an ask past the kernel maximum clamps to it (CLAMP) instead of refusing; the lane's socket builders land opcode, ioprio and the ifq word byte-exact" {
    var ring = try Ring128.initWithCq(16, 1024, 0);
    defer ring.deinit();
    try testing.expectEqual(@as(usize, 16), ring.sq.sqes.len);
    try testing.expectEqual(@as(usize, 1024), ring.cq.cqes.len);
    try testing.expect(ring.flags & linux.IORING_SETUP_CLAMP != 0);
    // 2 × IORING_MAX_ENTRIES is the ceiling (io_uring.h:160-161); an ask of
    // 4× it comes back clamped, a power of two.
    var big = try Ring128.initWithCq(16, 4 * 2 * max_entries, 0);
    defer big.deinit();
    try testing.expectEqual(@as(usize, 2 * max_entries), big.cq.cqes.len);

    const raw: [*]const u8 = @ptrCast(ring.sq.sqes.ptr);
    var buf: [64]u8 = undefined;
    const s0 = ring.getSqe().?;
    prepRecv(s0, .{ .raw = 7 }, &buf, linux.MSG.NOSIGNAL);
    try testing.expectEqual(@backingInt(linux.IORING_OP.RECV), raw[0]);
    try testing.expectEqual(@as(i32, 7), std.mem.readInt(i32, raw[4..8], .little));
    try testing.expectEqual(@as(u64, @intFromPtr(&buf)), std.mem.readInt(u64, raw[16..24], .little));
    try testing.expectEqual(@as(u32, 64), std.mem.readInt(u32, raw[24..28], .little));
    try testing.expectEqual(@as(u32, linux.MSG.NOSIGNAL), std.mem.readInt(u32, raw[28..32], .little));
    const s1 = ring.getSqe().?;
    prepRecvZc(s1, .{ .raw = 9 }, 0x1234_5678);
    const r1 = raw[sqe_size .. 2 * sqe_size];
    try testing.expectEqual(@backingInt(linux.IORING_OP.RECV_ZC), r1[0]);
    try testing.expectEqual(@as(u16, linux.IORING_RECV_MULTISHOT), std.mem.readInt(u16, r1[2..4], .little));
    try testing.expectEqual(@as(u32, 0), std.mem.readInt(u32, r1[24..28], .little));
    try testing.expectEqual(@as(u32, 0x1234_5678), std.mem.readInt(u32, r1[44..48], .little));
    const s2 = ring.getSqe().?;
    prepSend(s2, .{ .raw = 9 }, buf[0..24], linux.MSG.NOSIGNAL);
    const r2 = raw[2 * sqe_size .. 3 * sqe_size];
    try testing.expectEqual(@backingInt(linux.IORING_OP.SEND), r2[0]);
    try testing.expectEqual(@as(u32, 24), std.mem.readInt(u32, r2[24..28], .little));
    var cqe = std.mem.zeroes(Cqe32);
    cqe.big_cqe[0] = (0xAB << 48) | 4096;
    try testing.expectEqual(@as(u64, (0xAB << 48) | 4096), zcrxOff(&cqe));
}

test "1024 NOPs ride one enter and complete in order through 32 B CQEs whose extra words are zero" {
    var ring = try Ring128.init(1024, 0);
    defer ring.deinit();
    for (0..1024) |i| {
        const sqe = ring.getSqe().?;
        prepNop(sqe);
        sqe.user_data = i;
    }
    try testing.expect(ring.getSqe() == null);
    try testing.expectEqual(@as(u32, 0), ring.sqSpaceLeft());
    try testing.expectEqual(@as(u32, 1024), try ring.submit());
    try testing.expectEqual(@as(u32, 1024), ring.cqReady());
    try testing.expectEqual(@as(u32, 1024), ring.sqSpaceLeft());
    // The 2× CQ held every completion; the kernel dropped no SQE.
    try testing.expectEqual(@as(u32, 0), ring.cq.overflow.*);
    try testing.expectEqual(@as(u32, 0), ring.sq.dropped.*);
    var cqes: [512]Cqe32 = undefined;
    var expect_ud: u64 = 0;
    while (expect_ud < 1024) {
        const n = try ring.copyCqes(&cqes, 0);
        try testing.expect(n > 0);
        for (cqes[0..n]) |cqe| {
            try testing.expectEqual(expect_ud, cqe.user_data);
            try testing.expectEqual(@as(i32, 0), cqe.res);
            try testing.expectEqual(@as(u32, 0), cqe.flags);
            try testing.expectEqual([2]u64{ 0, 0 }, cqe.big_cqe);
            try testing.expectEqual(linux.E.SUCCESS, cqe.err());
            expect_ud += 1;
        }
    }
    try testing.expectEqual(@as(u32, 0), try ring.copyCqes(&cqes, 0));
}

test "a sparse fixed-buffer table: an empty slot is -EFAULT, a filled slot round-trips WRITE_FIXED (fixed file) and READ_FIXED (raw fd) through an ext4 file" {
    var ring = try Ring128.init(16, 0);
    defer ring.deinit();
    var scratch: Scratch = undefined;
    try scratch.open("fixed");
    defer scratch.close();
    const fd = scratch.fd;
    try ring.registerFiles(&.{fd});

    const buf = try testing.allocator.alignedAlloc(u8, .fromByteUnits(4096), 8192);
    defer testing.allocator.free(buf);
    for (buf[0..4096], 0..) |*b, i| b.* = @intCast((i * 7) & 0xFF);
    @memset(buf[4096..], 0);

    try ring.registerBuffersSparse(4);
    defer ring.unregisterBuffers() catch unreachable;
    const iov: std.posix.iovec = .{ .base = buf.ptr, .len = buf.len };
    try ring.registerBuffersUpdate(2, &.{iov});

    var cqes: [1]Cqe32 = undefined;
    // Slot 3 was never filled: the table is sparse and the kernel says so.
    const empty = ring.getSqe().?;
    prepReadFixed(empty, .{ .raw = fd }, 3, 0, 4096, @intFromPtr(buf.ptr));
    empty.user_data = 3;
    try testing.expectEqual(@as(u32, 1), try ring.submitAndWait(1));
    try reap(&ring, &cqes);
    try testing.expectEqual(@as(u64, 3), cqes[0].user_data);
    try testing.expectEqual(negErrno(.FAULT), cqes[0].res);

    const w = ring.getSqe().?;
    prepWriteFixed(w, .{ .fixed = 0 }, 2, 0, 4096, @intFromPtr(buf.ptr));
    w.user_data = 1;
    try testing.expectEqual(@as(u32, 1), try ring.submitAndWait(1));
    try reap(&ring, &cqes);
    try testing.expectEqual(@as(u64, 1), cqes[0].user_data);
    try testing.expectEqual(@as(i32, 4096), cqes[0].res);

    const r = ring.getSqe().?;
    prepReadFixed(r, .{ .raw = fd }, 2, 0, 4096, @intFromPtr(buf.ptr) + 4096);
    r.user_data = 2;
    try testing.expectEqual(@as(u32, 1), try ring.submitAndWait(1));
    try reap(&ring, &cqes);
    try testing.expectEqual(@as(u64, 2), cqes[0].user_data);
    try testing.expectEqual(@as(i32, 4096), cqes[0].res);
    try testing.expectEqualSlices(u8, buf[0..4096], buf[4096..]);
    try ring.unregisterFiles();
}

test "PollAdd on an eventfd completes with POLLIN once written; AsyncCancel of an armed PollAdd posts -ECANCELED on it and 0 on itself — by user_data and by fixed-file target" {
    var ring = try Ring128.init(16, 0);
    defer ring.deinit();
    const efd = try newEventfd();
    defer _ = linux.close(efd);
    try ring.registerFiles(&.{efd});

    const p = ring.getSqe().?;
    prepPollAdd(p, .{ .raw = efd }, linux.POLL.IN);
    p.user_data = 5;
    try testing.expectEqual(@as(u32, 1), try ring.submit());
    var cqes: [2]Cqe32 = undefined;
    try testing.expectEqual(@as(u32, 0), try ring.copyCqes(&cqes, 0));
    const Writer = struct {
        fn run(fd: linux.fd_t) void {
            writeEventfd(fd);
        }
    };
    const t = try std.Thread.spawn(.{}, Writer.run, .{efd});
    try reap(&ring, cqes[0..1]);
    t.join();
    try testing.expectEqual(@as(u64, 5), cqes[0].user_data);
    try testing.expect(cqes[0].res > 0 and @as(u32, @intCast(cqes[0].res)) & linux.POLL.IN != 0);
    try testing.expectEqual(@as(u64, 1), readEventfd(efd).?);

    // Cancel by user_data.
    const p2 = ring.getSqe().?;
    prepPollAdd(p2, .{ .raw = efd }, linux.POLL.IN);
    p2.user_data = 6;
    try testing.expectEqual(@as(u32, 1), try ring.submit());
    const c2 = ring.getSqe().?;
    prepAsyncCancel(c2, .{ .user_data = 6 });
    c2.user_data = 7;
    try testing.expectEqual(@as(u32, 1), try ring.submit());
    try reap(&ring, &cqes);
    try expectCancelPair(&cqes, 6, 7);

    // Cancel by fixed-file target: the poll on fixed index 0 (the eventfd).
    const p3 = ring.getSqe().?;
    prepPollAdd(p3, .{ .fixed = 0 }, linux.POLL.IN);
    p3.user_data = 8;
    try testing.expectEqual(@as(u32, 1), try ring.submit());
    const c3 = ring.getSqe().?;
    prepAsyncCancel(c3, .{ .fixed = 0 });
    c3.user_data = 9;
    try testing.expectEqual(@as(u32, 1), try ring.submit());
    try reap(&ring, &cqes);
    try expectCancelPair(&cqes, 8, 9);
}

fn expectCancelPair(cqes: []const Cqe32, poll_ud: u64, cancel_ud: u64) !void {
    var saw_poll = false;
    var saw_cancel = false;
    for (cqes) |cqe| {
        if (cqe.user_data == poll_ud) {
            try testing.expectEqual(negErrno(.CANCELED), cqe.res);
            saw_poll = true;
        } else if (cqe.user_data == cancel_ud) {
            try testing.expectEqual(@as(i32, 0), cqe.res);
            saw_cancel = true;
        } else return error.UnexpectedCqe;
    }
    try testing.expect(saw_poll and saw_cancel);
}

test "the submit-first lesson: an enter that submits discards its EXT_ARG wait's ETIME; the same wait with to_submit = 0 reports it; an unreaped CQE masks ETIME too; tick 0 is the eager non-blocking GETEVENTS flush" {
    var ring = try Ring128.init(16, 0);
    defer ring.deinit();
    var ts: linux.kernel_timespec = .{ .sec = 0, .nsec = 2 * std.time.ns_per_ms };
    const arg: linux.io_uring_getevents_arg = .{ .sigmask = 0, .sigmask_sz = 0, .pad = 0, .ts = @intFromPtr(&ts) };
    // A NOP that posts NO CQE (`IOSQE_CQE_SKIP_SUCCESS`, 5.17+): the wait
    // for one CQE times out with the CQ empty — and the call still reports
    // the submitted count, never ETIME.
    const silent = ring.getSqe().?;
    prepNop(silent);
    silent.flags |= linux.IOSQE_CQE_SKIP_SUCCESS;
    const combined = try ring.enter(ring.flushSq(), 1, linux.IORING_ENTER_GETEVENTS, &arg);
    try testing.expectEqual(@as(u32, 1), combined.submitted);
    try testing.expect(!combined.timed_out);
    // The same wait submitting nothing: ETIME survives.
    const alone = try ring.enter(0, 1, linux.IORING_ENTER_GETEVENTS, &arg);
    try testing.expect(alone.timed_out);
    try testing.expectEqual(@as(u32, 0), alone.submitted);

    // The eager flush: nothing pending ⇒ timed out at once; a NOP pending ⇒
    // submitted 1 and its CQE is already there.
    const idle = try ring.submitAndWaitTimed(1, 0);
    try testing.expect(idle.timed_out);
    const n = ring.getSqe().?;
    prepNop(n);
    n.user_data = 2;
    const busy = try ring.submitAndWaitTimed(1, 0);
    try testing.expectEqual(@as(u32, 1), busy.submitted);

    // With that CQE still unreaped, a wait for TWO runs its 2 ms and returns
    // 0, not ETIME: `io_cqring_wait` reports success whenever ANY CQE sits
    // in the ring (`cq.head == cq.tail ? ret : 0`). A timed park with
    // unreaped work therefore reads `.event`, which is the right verdict.
    const masked = try ring.enter(0, 2, linux.IORING_ENTER_GETEVENTS, &arg);
    try testing.expect(!masked.timed_out);
    var cqes: [2]Cqe32 = undefined;
    try testing.expectEqual(@as(u32, 1), try ring.copyCqes(&cqes, 0));
    try testing.expectEqual(@as(u64, 2), cqes[0].user_data);
    // Reaped: the same wait now times out.
    const empty = try ring.enter(0, 1, linux.IORING_ENTER_GETEVENTS, &arg);
    try testing.expect(empty.timed_out);
}

test "getProbe: URING_CMD and every op this binding builds are supported" {
    var ring = try Ring128.init(16, 0);
    defer ring.deinit();
    const probe = try ring.getProbe();
    for ([_]linux.IORING_OP{ .NOP, .READ, .WRITE, .READ_FIXED, .WRITE_FIXED, .POLL_ADD, .ASYNC_CANCEL, .FSYNC, .TIMEOUT, .URING_CMD }) |op| {
        try testing.expect(probe.is_supported(op));
    }
}

test "a URING_CMD on an impossible fd completes -EBADF, on a regular file -EOPNOTSUPP (raw and through fixed file 0): the cmd-area SQE is parsed as a command without a FUSE fd" {
    var ring = try Ring128.init(16, 0);
    defer ring.deinit();
    var scratch: Scratch = undefined;
    try scratch.open("cmd");
    defer scratch.close();
    const fd = scratch.fd;
    try ring.registerFiles(&.{fd});

    var cmd: [cmd_len]u8 = undefined;
    for (&cmd, 0..) |*b, i| b.* = @intCast(i);
    const bad = ring.getSqe().?;
    prepUringCmd(bad, .{ .raw = 1 << 20 }, 3, &cmd);
    bad.user_data = 1;
    const plain = ring.getSqe().?;
    prepUringCmd(plain, .{ .raw = fd }, 3, &cmd);
    plain.user_data = 2;
    const fixed = ring.getSqe().?;
    prepUringCmd(fixed, .{ .fixed = 0 }, 3, &cmd);
    fixed.user_data = 3;
    try testing.expectEqual(@as(u32, 3), try ring.submitAndWait(3));
    var cqes: [3]Cqe32 = undefined;
    try reap(&ring, &cqes);
    var seen: [4]bool = @splat(false);
    for (cqes) |cqe| {
        const want: i32 = switch (cqe.user_data) {
            1 => negErrno(.BADF),
            2, 3 => negErrno(.OPNOTSUPP),
            else => return error.UnexpectedCqe,
        };
        try testing.expectEqual(want, cqe.res);
        seen[@intCast(cqe.user_data)] = true;
    }
    try testing.expect(seen[1] and seen[2] and seen[3]);
}

test "registerEventfd: a completion signals the eventfd; after unregisterEventfd it stays silent" {
    var ring = try Ring128.init(16, 0);
    defer ring.deinit();
    const efd = try newEventfd();
    defer _ = linux.close(efd);
    try ring.registerEventfd(efd);
    const n = ring.getSqe().?;
    prepNop(n);
    try testing.expectEqual(@as(u32, 1), try ring.submitAndWait(1));
    try testing.expect(readEventfd(efd).? >= 1);
    try ring.unregisterEventfd();
    const n2 = ring.getSqe().?;
    prepNop(n2);
    try testing.expectEqual(@as(u32, 1), try ring.submitAndWait(1));
    try testing.expect(readEventfd(efd) == null);
}

test "registerFilesUpdate fills a sparse slot; a READ through it completes; unregisterFiles makes a fixed-file op -EBADF" {
    var ring = try Ring128.init(16, 0);
    defer ring.deinit();
    const efd = try newEventfd();
    defer _ = linux.close(efd);
    try ring.registerFiles(&.{ -1, -1 });
    try ring.registerFilesUpdate(1, &.{efd});
    writeEventfd(efd);
    var count: u64 = 0;
    const r = ring.getSqe().?;
    prepRead(r, .{ .fixed = 1 }, std.mem.asBytes(&count), 0);
    r.user_data = 1;
    try testing.expectEqual(@as(u32, 1), try ring.submitAndWait(1));
    var cqes: [1]Cqe32 = undefined;
    try reap(&ring, &cqes);
    try testing.expectEqual(@as(i32, 8), cqes[0].res);
    try testing.expectEqual(@as(u64, 1), count);

    try ring.unregisterFiles();
    const r2 = ring.getSqe().?;
    prepRead(r2, .{ .fixed = 1 }, std.mem.asBytes(&count), 0);
    r2.user_data = 2;
    try testing.expectEqual(@as(u32, 1), try ring.submitAndWait(1));
    try reap(&ring, &cqes);
    try testing.expectEqual(negErrno(.BADF), cqes[0].res);
}

test "a ring built R_DISABLED on one thread is enabled by the worker that will issue on it (SINGLE_ISSUER follows enableRings)" {
    var ring = try Ring128.init(16, linux.IORING_SETUP_SINGLE_ISSUER | linux.IORING_SETUP_R_DISABLED);
    defer ring.deinit();
    const Worker = struct {
        fn run(r: *Ring128, result: *anyerror!void) void {
            result.* = issue(r);
        }
        fn issue(r: *Ring128) !void {
            try r.enableRings();
            const n = r.getSqe().?;
            prepNop(n);
            n.user_data = 11;
            try testing.expectEqual(@as(u32, 1), try r.submitAndWait(1));
            var cqes: [1]Cqe32 = undefined;
            try reap(r, &cqes);
            try testing.expectEqual(@as(u64, 11), cqes[0].user_data);
        }
    };
    var result: anyerror!void = {};
    const t = try std.Thread.spawn(.{}, Worker.run, .{ &ring, &result });
    t.join();
    try result;
}

test "a TIMEOUT op completes -ETIME after its span" {
    var ring = try Ring128.init(16, 0);
    defer ring.deinit();
    const ts: linux.kernel_timespec = .{ .sec = 0, .nsec = 2 * std.time.ns_per_ms };
    const sqe = ring.getSqe().?;
    prepTimeout(sqe, &ts, 0, 0);
    sqe.user_data = 21;
    try testing.expectEqual(@as(u32, 1), try ring.submitAndWait(1));
    var cqes: [1]Cqe32 = undefined;
    try reap(&ring, &cqes);
    try testing.expectEqual(@as(u64, 21), cqes[0].user_data);
    try testing.expectEqual(negErrno(.TIME), cqes[0].res);
}

test "registerBufRing through the raw opcode: stock PBUF_RING registers once and refuses the same bgid again with EEXIST" {
    var ring = try Ring128.init(16, 0);
    defer ring.deinit();
    const len = 8 * @sizeOf(linux.io_uring_buf);
    const rc = linux.mmap(null, len, .{ .READ = true, .WRITE = true }, .{ .TYPE = .PRIVATE, .ANONYMOUS = true }, -1, 0);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
    defer _ = linux.munmap(@ptrFromInt(rc), len);
    const reg: BufReg = .{ .ring_addr = rc, .ring_entries = 8, .bgid = 9, .flags = 0, .resv = @splat(0) };
    const pbuf_ring = @backingInt(linux.IORING_REGISTER.REGISTER_PBUF_RING);
    try ring.registerBufRing(pbuf_ring, reg);
    try testing.expectError(error.PathAlreadyExists, ring.registerBufRing(pbuf_ring, reg));
}

test "the kmbuf rung on an sqz kernel: the register opcode takes a page buf_size, repeats EEXIST, and the region maps at 0x88000000 | (bgid << 16); a stock kernel refuses every rung" {
    var ring = try Ring128.init(8, 0);
    defer ring.deinit();
    const page: u32 = @intCast(std.heap.pageSize());
    const reg: BufReg = .{ .ring_addr = page, .ring_entries = 8, .bgid = 7, .flags = 0, .resv = @splat(0) };
    // fuse_kmbuf_zc §2.1: 37 on 6.19-sqz, 38 on 7.1/7.2-sqz. A stock
    // kernel's 37/38 are BPF_FILTER/LAST (EINVAL); 6.19's 38 is the kmbuf
    // UNREGISTER (ENOENT): refusals, never a false positive.
    const rung: u32 = for ([_]u32{ 37, 38 }) |opcode| {
        if (ring.registerBufRing(opcode, reg)) |_| break opcode else |e| switch (e) {
            error.InvalidArgument, error.FileNotFound => continue,
            else => return e,
        }
    } else {
        return testkit.skip(@src(), .no_kernel_feature, "no kmbuf register opcode on this kernel (stock stratum)");
    };
    log.debug("uring128: kmbuf register opcode {d} confirmed", .{rung});
    try testing.expectError(error.PathAlreadyExists, ring.registerBufRing(rung, reg));
    const region = try ring.mmapRegion(0x88000000 | (@as(u64, 7) << 16), 8 * page);
    defer Ring128.munmapRegion(region);
    try testing.expectEqual(@as(usize, 8 * page), region.len);
    region[0] = 0x5A;
    try testing.expectEqual(@as(u8, 0x5A), region[0]);
}

test "Tag: pack/unpack round-trips every field; the top kind is the protocol's and a full request tag sits below reserved_ud" {
    const t: Tag = .{ .kind = 3, .a = 0xBEEF, .b = 0x0102, .c = 0xABCDEF };
    const ud = t.pack();
    const back = Tag.unpack(ud);
    try testing.expectEqual(@as(u8, 3), back.kind);
    try testing.expectEqual(@as(u16, 0xBEEF), back.a);
    try testing.expectEqual(@as(u16, 0x0102), back.b);
    try testing.expectEqual(@as(u24, 0xABCDEF), back.c);
    try testing.expectEqual(ud >> 56, @as(u64, 3));
    try testing.expect(!Tag.isReserved(ud));
    try testing.expect(Tag.isReserved(ring_park.wake_ud));
    try testing.expect(Tag.isReserved(ring_park.reserved_ud));
    try testing.expect(!Tag.isReserved(ring_park.reserved_ud - 1));
}

test "the wake protocol on the 128 ring: 8 publishers between two passes pay ONE eventfd write; the park ends on it; drain → disarm → scan re-opens the coalescer" {
    var pr = try ParkedRing128.init(testing.allocator, 16, 0);
    defer pr.deinit();
    try testing.expect(pr.armWake());
    try testing.expectEqual(ring_park.Verdict.tick, pr.park(2 * std.time.ns_per_ms));

    var paid = std.atomic.Value(u32).init(0);
    const Publisher = struct {
        fn run(w: ring_park.Wake, counter: *std.atomic.Value(u32)) void {
            if (w.wake()) _ = counter.fetchAdd(1, .seq_cst);
        }
    };
    var threads: [8]std.Thread = undefined;
    for (&threads) |*t| t.* = try std.Thread.spawn(.{}, Publisher.run, .{ pr.wakeHandle(), &paid });
    try testing.expectEqual(ring_park.Verdict.event, pr.park(10 * std.time.ns_per_s));
    for (threads) |t| t.join();
    try testing.expectEqual(@as(u32, 1), paid.load(.seq_cst));

    var cqes: [4]Cqe32 = undefined;
    try testing.expectEqual(@as(u32, 1), try pr.ring.copyCqes(&cqes, 0));
    try testing.expect(pr.consumeWake(cqes[0]));
    try testing.expectEqual(@as(i32, 8), cqes[0].res);
    // The READ drained the counter: exactly one write landed.
    try testing.expectEqual(@as(u64, 1), pr.wake_buf.*);
    try testing.expect(pr.service());
    try testing.expect(!pr.service());
    // Re-opened: the next producer pays again, the one after elides.
    try testing.expect(pr.wake());
    try testing.expect(!pr.wake());
    try testing.expect(pr.armWake());
    try testing.expectEqual(ring_park.Verdict.event, pr.park(10 * std.time.ns_per_s));
    try testing.expectEqual(@as(u32, 1), try pr.ring.copyCqes(&cqes, 0));
    try testing.expect(pr.consumeWake(cqes[0]));
    try testing.expect(pr.service());
}

test "an owner's op completes through the park beside the standing READ; the fdatasync barrier rides fixed file 0; deinit cancels the armed READ" {
    var pr = try ParkedRing128.init(testing.allocator, 16, 0);
    defer pr.deinit();
    var scratch: Scratch = undefined;
    try scratch.open("park");
    defer scratch.close();
    const fd = scratch.fd;
    try testing.expect(pr.registerFile(fd));
    try testing.expect(pr.armWake());

    var block: [4096]u8 = @splat(0xA5);
    const w = pr.getSqe().?;
    prepWrite(w, .{ .fixed = 0 }, &block, 0);
    w.user_data = 7;
    try testing.expectEqual(ring_park.Verdict.event, pr.park(10 * std.time.ns_per_s));
    var cqes: [4]Cqe32 = undefined;
    try testing.expectEqual(@as(u32, 1), try pr.ring.copyCqes(&cqes, 1));
    try testing.expect(!pr.consumeWake(cqes[0]));
    try testing.expectEqual(@as(u64, 7), cqes[0].user_data);
    try testing.expectEqual(@as(i32, 4096), cqes[0].res);

    _ = pr.prepFdatasync(0, true, ring_park.reserved_ud - 1).?;
    try testing.expectEqual(ring_park.Verdict.event, pr.park(10 * std.time.ns_per_s));
    try testing.expectEqual(@as(u32, 1), try pr.ring.copyCqes(&cqes, 1));
    try testing.expectEqual(ring_park.reserved_ud - 1, cqes[0].user_data);
    try testing.expectEqual(@as(i32, 0), cqes[0].res);
    try testing.expect(pr.wake_armed);
}

test "the posture ladder on this kernel resolves at rung 0 (single_issuer+defer_taskrun+taskrun_flag); a deferred completion raises IORING_SQ_TASKRUN before any enter and the park materialises it" {
    var rung: usize = undefined;
    var pr = try ParkedRing128.initProbing(testing.allocator, 16, &taskrun_ladder_flags, &rung);
    defer pr.deinit();
    log.debug("uring128: taskrun ladder rung {d} ({s})", .{ rung, taskrun_ladder[rung].name });
    try testing.expectEqual(@as(usize, 0), rung);
    try testing.expect(pr.ring.flags & linux.IORING_SETUP_DEFER_TASKRUN != 0);
    try testing.expect(pr.ring.flags & linux.IORING_SETUP_TASKRUN_FLAG != 0);

    try testing.expect(pr.armWake());
    const n = pr.getSqe().?;
    prepNop(n);
    n.user_data = 1;
    try testing.expectEqual(ring_park.Verdict.event, pr.park(10 * std.time.ns_per_s));
    var cqes: [4]Cqe32 = undefined;
    try testing.expectEqual(@as(u32, 1), try pr.ring.copyCqes(&cqes, 0));
    try testing.expectEqual(@as(u64, 1), cqes[0].user_data);
    try testing.expect(pr.wake_armed);

    // A foreign-context wake: the eventfd write runs the poll wakeup in the
    // writer's context, which queues the READ's completion as local task
    // work and raises the flag — visible to the spin governor's one load.
    const Writer = struct {
        fn run(w: ring_park.Wake) void {
            w.force();
        }
    };
    const t = try std.Thread.spawn(.{}, Writer.run, .{pr.wakeHandle()});
    t.join();
    try testing.expect(pr.ring.sqFlags() & linux.IORING_SQ_TASKRUN != 0);
    try testing.expectEqual(@as(u32, 1), try pr.ring.copyCqes(&cqes, 1));
    try testing.expect(pr.consumeWake(cqes[0]));
    try testing.expectEqual(@as(i32, 8), cqes[0].res);
}

test "initProbing on the 128 ring: a refused rung falls through (DEFER_TASKRUN alone is EINVAL), exhaustion is the one loud error" {
    const ladder = [_]u32{ linux.IORING_SETUP_DEFER_TASKRUN, 0 };
    var rung: usize = undefined;
    var pr = try ParkedRing128.initProbing(testing.allocator, 16, &ladder, &rung);
    defer pr.deinit();
    try testing.expectEqual(@as(usize, 1), rung);
    try testing.expect(pr.armWake());
    try testing.expectEqual(ring_park.Verdict.tick, pr.park(2 * std.time.ns_per_ms));
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.RingSetupFailed, ParkedRing128.initProbing(testing.allocator, 16, ladder[0..1], &rung));
    try log.testing.expectAllFired();
}

test "a dead 128 ring parks on the eventfd alone: the tick fires, a wake arrives and is consumed, the coalescer still gates producers" {
    var pr = try ParkedRing128.init(testing.allocator, 16, 0);
    defer pr.deinit();
    pr.markDead();
    try testing.expect(pr.isDead());
    try testing.expect(!pr.armWake());
    try testing.expectEqual(ring_park.Verdict.tick, pr.park(2 * std.time.ns_per_ms));
    pr.forceWake();
    try testing.expectEqual(ring_park.Verdict.event, pr.park(10 * std.time.ns_per_s));
    try testing.expect(pr.service());
    try testing.expect(pr.wake());
    try testing.expect(!pr.wake());
    try testing.expectEqual(ring_park.Verdict.event, pr.park(null));
    try testing.expect(pr.service());
}

test "quiesce on the ring's thread, deinit on the joiner's: the standing READ is cancelled and reaped before the SINGLE_ISSUER ring changes hands" {
    var pr: ParkedRing128 = undefined;
    const Owner = struct {
        fn run(p: *ParkedRing128) void {
            p.* = ParkedRing128.init(testing.allocator, 16, linux.IORING_SETUP_SINGLE_ISSUER) catch @panic("ring setup failed");
            std.debug.assert(p.armWake());
            p.forceWake();
            p.quiesce();
            std.debug.assert(!p.wake_armed);
        }
    };
    const t = try std.Thread.spawn(.{}, Owner.run, .{&pr});
    t.join();
    pr.forceWake();
    var cqes: [4]Cqe32 = undefined;
    try testing.expectEqual(@as(u32, 0), try pr.ring.copyCqes(&cqes, 0));
    pr.deinit();
}
