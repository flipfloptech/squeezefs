//! Fibers: the unit a lane schedules (charter §9; `docs/porting/exec.md`
//! §3.3). A fiber is a preallocated slot in its lane's table (no per-spawn
//! allocation — 0002 ledger vs the reference's `Arc<LaneTask>` per spawn and
//! `Arc<FusedWaker>` per poll) with a lazily mapped stack, the delivery word
//! (`task_state.zig`), the cancel word, and the result/context bytes the
//! `std.Io` task protocol copies through.
//!
//! Stacks are `mmap`ed with a GUARD PAGE below the usable region
//! (`PROT_NONE`): a frame that outgrows its stack faults loud instead of
//! overwriting the neighbouring mapping — std's `Io.Uring` reserves 60 MiB
//! per fiber from a plain allocator with no guard (0002 ledger). In debug and
//! test builds the usable region is filled with a canary at map time and
//! scanned at recycle: `exec_fiber_stack_hwm_bytes` records the deepest
//! frame ever parked and the `exec_fiber_stack_over_half` tripwire fires
//! before an overflow ever can.
//!
//! The entry trampoline is std's shape (`Io/Uring.zig` `AsyncClosure.entry`):
//! a naked stub moves the `Switch` pointer the first `contextSwitch` hands
//! over into the first argument register and tail-calls `call`, which
//! recovers the fiber from `switch.new` (`&fiber.ctx`), runs the start
//! function, and finishes by switching to the lane's idle context forever.
//!
//! Linux only (mmap flags, the lane's ring).

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const assert = std.debug.assert;
const derive = @import("../core/derive.zig");
const futex = @import("../sync/futex.zig");
const task_state = @import("task_state.zig");
const timer_heap = @import("timer_heap.zig");
const stats = @import("stats.zig");
const lane_mod = @import("lane.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("exec/fiber is Linux only (charter §1.2); gate the import on builtin.os.tag");
    if (!Io.fiber.supported) @compileError("exec/fiber needs Io.fiber.contextSwitch for this architecture");
}

pub const Lane = lane_mod.Lane;
pub const LaneClass = stats.LaneClass;
pub const TaskState = task_state.TaskState;

/// std's task-protocol ceilings (`Io/Uring.zig` L252–258): a result is a
/// small error union or handle, a context an args tuple; the vtable asserts
/// them at spawn exactly as `Io.Uring.concurrent` does.
pub const max_result_align: std.mem.Alignment = .@"16";
pub const max_result_size: usize = 512;
pub const max_context_align: std.mem.Alignment = .@"16";
pub const max_context_size: usize = 1024;

/// The stack class floor: std's `Io.Uring` measured ">128 KiB being used by
/// the self-hosted backend to panic" and "glibc complain about 256 KiB"
/// (`Io/Uring.zig` L56–58) — twice the larger measured figure, so the
/// `std.fmt` panic path alone can never reach the guard.
pub const stack_floor_bytes: usize = 512 * derive.KiB;

/// Per-class stack size, page-aligned (a derivation, tie-tested below).
pub fn stackSizeFor(class: LaneClass, page_size: usize) usize {
    const want: usize = switch (class) {
        // Handler-class fibers keep a `kv_node` 64 KiB node image, the bset
        // frame staging and a reply header on the frame: 2× the floor so
        // the half-stack tripwire still has the whole floor as headroom.
        .fuse, .fuse_ur, .meta, .lease => 2 * stack_floor_bytes,
        // The journal lane's conveyor passes fold a whole batch
        // (`META_COMMIT_BATCH_TXS` ≥ 64 entries) on frame; `main` runs the
        // CLI's deepest paths (format, fsck): 8× the floor.
        .journal, .main => 8 * stack_floor_bytes,
    };
    return @intCast(derive.alignUp(want, page_size));
}

/// Canary fill and high-water scan: debug and test builds only (a scan of
/// a 1 MiB stack per recycle is not a product-path cost).
pub const canary_enabled = builtin.mode == .debug or builtin.is_test;
const canary_byte: u8 = 0xA5;

/// What a fiber reports to its lane when it switches out.
pub const Message = enum(u8) {
    /// Parked (futex, timer, CQE, awaiting): `end_poll_pending`.
    parked,
    /// `yield_now`: requeue at the tail.
    yielded,
    /// The start function returned: `end_poll_complete`.
    finished,
};

/// What a parked fiber is waiting for — the cancel word's dispatch
/// (`exec.md` §3.8): what `requestCancel` must do to interrupt it.
pub const Awaiting = enum(u3) {
    nothing,
    /// A futex wait, a sleep or a timed wait: a plain wake resumes it and it
    /// observes the request itself.
    wakeable,
    /// An SQE on its lane ring: only the OWNING lane may cancel it
    /// (`ASYNC_CANCEL` under `SINGLE_ISSUER`); the fiber resumes at the CQE.
    ring_op,
    /// `Group.await`: the request propagates to the members.
    group,
    /// `Future.await`: not a cancellation point; the request is observed at
    /// the next one.
    future,
    /// A blocking-pool job: never interrupted; the request waits for it.
    blocking,
};

/// ONE atomic word per fiber: the cancel request, what it awaits, and the
/// group it belongs to — packed together so a group-cancel walker's CAS
/// cannot land on a slot re-spawned for another group (the ABA std's
/// `Io.Threaded` avoids the same way with its `Thread.status` word).
pub const CancelWord = packed struct(u64) {
    requested: bool = false,
    awaiting: Awaiting = .nothing,
    reserved: u12 = 0,
    /// `*Io.Group` bits; 0 = no group. 48 pointer bits is the user-space VA
    /// ceiling of every target (`sync/arc_swap.zig`).
    group: u48 = 0,

    pub fn fromGroup(g: ?*Io.Group) CancelWord {
        const addr: u64 = if (g) |p| @intFromPtr(p) else 0;
        assert(addr >> 48 == 0);
        return .{ .group = @intCast(addr) };
    }

    pub fn groupPtr(self: CancelWord) ?*Io.Group {
        if (self.group == 0) return null;
        return @ptrFromInt(@as(usize, self.group));
    }

    pub fn toInt(self: CancelWord) u64 {
        return @bitCast(self);
    }
    pub fn fromInt(v: u64) CancelWord {
        return @bitCast(v);
    }
};

/// Cancel protection (`Io.CancelProtection`) plus the acknowledged latch:
/// after a cancellation point returned `error.Canceled` the request is spent
/// until `recancel` (std's `Io.Uring` `CancelProtection` shape).
pub const Protection = packed struct {
    user: Io.CancelProtection = .unblocked,
    acknowledged: bool = false,

    pub const unblocked: Protection = .{};
    pub const blocked: Protection = .{ .user = .blocked };

    pub fn check(self: Protection) Io.CancelProtection {
        return if (self.user == .blocked or self.acknowledged) .blocked else .unblocked;
    }
};

pub const Kind = enum(u8) { future, group };

pub const Start = union {
    future: *const fn (context: *const anyopaque, result: *anyopaque) void,
    group: *const fn (context: *const anyopaque) void,
};

/// A mapped stack: `[guard page][usable region]`, `len` covers both.
pub const Stack = struct {
    base: [*]align(std.heap.page_size_min) u8,
    len: usize,
    page: usize,

    pub const MapError = error{StackMapFailed};

    pub fn map(stack_size: usize, page: usize) MapError!Stack {
        assert(derive.isAligned(stack_size, page));
        const len = stack_size + page;
        // NORESERVE: the reserve is virtual until touched (the lane's whole
        // population is mapped at spawn); STACK: the kernel's hint for
        // stack-shaped mappings (THP policy).
        const rc = linux.mmap(null, len, .{ .READ = true, .WRITE = true }, .{ .TYPE = .PRIVATE, .ANONYMOUS = true, .NORESERVE = true, .STACK = true }, -1, 0);
        if (linux.errno(rc) != .SUCCESS) return error.StackMapFailed;
        const base: [*]align(std.heap.page_size_min) u8 = @ptrFromInt(rc);
        // The guard: the lowest page is unmapped for every access.
        if (linux.errno(linux.mprotect(base, page, .{})) != .SUCCESS) {
            _ = linux.munmap(base, len);
            return error.StackMapFailed;
        }
        const s: Stack = .{ .base = base, .len = len, .page = page };
        if (canary_enabled) @memset(s.usable(), canary_byte);
        stats.fiber_stack_bytes.add(len);
        return s;
    }

    pub fn unmap(self: Stack) void {
        stats.fiber_stack_bytes.sub(self.len);
        _ = linux.munmap(self.base, self.len);
    }

    /// The writable region above the guard.
    pub fn usable(self: Stack) []u8 {
        return self.base[self.page..self.len];
    }

    /// One past the highest usable byte: 16-aligned (page-aligned) — the
    /// initial stack pointer.
    pub fn top(self: Stack) usize {
        return @intFromPtr(self.base) + self.len;
    }

    /// Bytes of the usable region below the top that lost their canary: the
    /// deepest frame since the last fill. Scans from the low end (the
    /// untouched part) 32 bytes at a time: a 1 MiB stack is ~32 k compares,
    /// which keeps the per-recycle cost of the debug/test build at ~10 µs.
    pub fn highWater(self: Stack) usize {
        const u = self.usable();
        const V = @Vector(32, u8);
        const pattern: V = @splat(canary_byte);
        var i: usize = 0;
        while (i + 32 <= u.len) : (i += 32) {
            const chunk: V = u[i..][0..32].*;
            if (@reduce(.Or, chunk != pattern)) break;
        }
        while (i < u.len) : (i += 1) if (u[i] != canary_byte) return u.len - i;
        return 0;
    }

    /// Re-fill the touched region after a high-water read (the next spawn
    /// starts from a clean canary).
    pub fn refillCanary(self: Stack, used: usize) void {
        const u = self.usable();
        @memset(u[u.len - used ..], canary_byte);
    }
};

pub const Fiber = struct {
    ctx: Io.fiber.Context,
    state: TaskState,
    /// `CancelWord` bits.
    cancel: std.atomic.Value(u64),
    protection: Protection,
    msg: Message,
    kind: Kind,
    start: Start,
    /// The awaiter of this future: null (none yet), a fiber, `raw_awaiter`
    /// (a non-lane thread parked on `raw_word`) or `finished` (the lane
    /// published the result; whoever reads this recycles the fiber).
    awaiter: std.atomic.Value(?*Fiber),
    raw_word: ?*futex.Word,
    /// The group this fiber is awaiting (`Awaiting.group`), for propagation.
    awaited_group: ?*Io.Group,
    /// The result slot of a ring op (`Awaiting.ring_op`); `op_pending` is
    /// set by the fiber before it parks and cleared by its lane at the CQE.
    cqe_res: i32,
    cqe_flags: u32,
    op_pending: bool,
    /// `futex_table.zig`'s intrusive waiter link.
    futex_ptr: ?*const u32,
    futex_next: ?*Fiber,
    futex_prev: ?*Fiber,
    futex_linked: bool,
    /// `timer_heap.zig`'s indexed slot.
    timer_slot: u32,
    timer_deadline_ns: u64,
    /// The lane's ready FIFO link (lane thread only).
    ready_next: ?*Fiber,
    /// `fdwatch.zig`: the fd this fiber awaits readability on, and whether a
    /// later awaiter displaced it.
    watched_fd: linux.fd_t,
    watch_replaced: bool,
    lane: *Lane,
    /// Index in `lane.fibers`.
    slot: u32,
    stack: ?Stack,
    result: [max_result_size]u8 align(16),
    context: [max_context_size]u8 align(16),

    /// Sentinels for `awaiter` (never valid fiber addresses: below any mapping).
    pub const finished: *Fiber = @ptrFromInt(@alignOf(Fiber));
    pub const raw_awaiter: *Fiber = @ptrFromInt(2 * @alignOf(Fiber));

    /// A slot's resting state (table init and recycle).
    pub fn reset(f: *Fiber, lane: *Lane, slot: u32) void {
        f.* = .{
            .ctx = undefined,
            .state = .newScheduled(),
            .cancel = .init(0),
            .protection = .unblocked,
            .msg = .parked,
            .kind = .future,
            .start = undefined,
            .awaiter = .init(null),
            .raw_word = null,
            .awaited_group = null,
            .cqe_res = 0,
            .cqe_flags = 0,
            .op_pending = false,
            .futex_ptr = null,
            .futex_next = null,
            .futex_prev = null,
            .futex_linked = false,
            .timer_slot = timer_heap.no_slot,
            .timer_deadline_ns = 0,
            .ready_next = null,
            .watched_fd = -1,
            .watch_replaced = false,
            .lane = lane,
            .slot = slot,
            .stack = f.stack,
            .result = undefined,
            .context = undefined,
        };
    }

    /// Arm a slot for a spawn: copy the context, set the start, the delivery
    /// word (SCHEDULED: spawn IS the first wake), the cancel word (requested
    /// at once when the group is already cancelled), the protection, and the
    /// entry context on a mapped stack.
    pub fn arm(f: *Fiber, kind: Kind, start: Start, context: []const u8, group: ?*Io.Group, group_canceled: bool, protection: Protection, stack_size: usize, page: usize) Stack.MapError!void {
        assert(context.len <= max_context_size);
        if (f.stack == null) f.stack = try Stack.map(stack_size, page);
        f.state = .newScheduled();
        var cw = CancelWord.fromGroup(group);
        cw.requested = group_canceled;
        f.cancel.store(cw.toInt(), .monotonic);
        f.protection = protection;
        f.kind = kind;
        f.start = start;
        f.awaiter.store(null, .monotonic);
        f.raw_word = null;
        f.awaited_group = null;
        f.futex_linked = false;
        f.op_pending = false;
        f.timer_slot = timer_heap.no_slot;
        f.ready_next = null;
        f.watched_fd = -1;
        f.watch_replaced = false;
        @memcpy(f.context[0..context.len], context);
        const top = f.stack.?.top();
        f.ctx = switch (builtin.cpu.arch) {
            .aarch64, .riscv64 => .{ .sp = top, .fp = 0, .pc = @intFromPtr(&entry) },
            // The ABI expects `rsp + 8` 16-aligned at entry (as after a `call`);
            // the slot at `rsp` is the first frame's return address: 0, so a
            // stack walk ends here (see `entry`).
            .x86_64 => blk: {
                const ra: *usize = @ptrFromInt(top - 8);
                ra.* = 0;
                break :blk .{ .rsp = top - 8, .rbp = 0, .rip = @intFromPtr(&entry) };
            },
            else => unreachable,
        };
    }

    pub fn cancelWord(f: *const Fiber) CancelWord {
        // acquire pairs with `requestCancel`'s acq_rel CAS so the awaited
        // state a canceller published is visible.
        return CancelWord.fromInt(f.cancel.load(.acquire));
    }

    /// Move `awaiting` from `old` to `new` (asserted); returns the
    /// `requested` bit as of the move. acq_rel pairs with `requestCancel`.
    pub fn changeAwaiting(f: *Fiber, old: Awaiting, new: Awaiting) bool {
        var cur = CancelWord.fromInt(f.cancel.load(.acquire));
        while (true) {
            assert(cur.awaiting == old);
            var next = cur;
            next.awaiting = new;
            if (f.cancel.cmpxchgWeak(cur.toInt(), next.toInt(), .acq_rel, .acquire)) |actual| {
                cur = CancelWord.fromInt(actual);
                continue;
            }
            return cur.requested;
        }
    }

    /// Set `requested` if the fiber still belongs to `group` (null = any);
    /// returns the word as it was when the bit was set, or null when nothing
    /// was done (already requested, or a different group).
    pub fn requestCancelWord(f: *Fiber, group: ?*Io.Group) ?CancelWord {
        var cur = CancelWord.fromInt(f.cancel.load(.acquire));
        while (true) {
            if (cur.requested) return null;
            if (group) |g| if (cur.groupPtr() != g) return null;
            var next = cur;
            next.requested = true;
            if (f.cancel.cmpxchgWeak(cur.toInt(), next.toInt(), .acq_rel, .acquire)) |actual| {
                cur = CancelWord.fromInt(actual);
                continue;
            }
            return cur;
        }
    }

    /// A pure cancellation point on the running fiber.
    pub fn checkCancel(f: *Fiber) Io.Cancelable!void {
        if (f.protection.check() == .blocked) return;
        if (f.cancelWord().requested) {
            f.protection.acknowledged = true;
            return error.Canceled;
        }
    }

    pub fn resultBytes(f: *Fiber, len: usize) []u8 {
        assert(len <= max_result_size);
        return f.result[0..len];
    }

    // ---- the switch ----

    /// Switch to the lane's idle context with `msg`; returns when the lane
    /// switches back in.
    ///
    /// `noinline` (and the same on `switchIn`): `Io.fiber.contextSwitch`'s
    /// inline asm declares every allocatable register clobbered EXCEPT the
    /// AArch64 platform register x18, which Linux leaves allocatable and
    /// caller-saved. Inlined into an optimized caller (the LTO link runs
    /// the O2 pipeline over every module), the caller may keep a live value
    /// in x18 across the asm and read garbage after the resume — the lane's
    /// `f` after `switchIn`, a sleeper's `f` after `park` (the daemon's
    /// first 5 s watchdog wake crashed at `f.msg = msg` with a code address
    /// in `f`). A call boundary is the ABI's own fence: no caller value is
    /// live in a caller-saved register across it, and nothing in these two
    /// bodies is live past the switch.
    noinline fn switchToLane(f: *Fiber, msg: Message) void {
        f.msg = msg;
        var sw: Io.fiber.Switch = .{ .old = &f.ctx, .new = &f.lane.idle_ctx };
        _ = Io.fiber.contextSwitch(&sw);
    }

    /// Park: the lane runs `end_poll_pending`; a wake (any thread) brings the
    /// fiber back here.
    pub fn park(f: *Fiber) void {
        f.switchToLane(.parked);
    }

    /// `yield_now`: back to the tail of the lane's ready queue.
    pub fn yieldNow(f: *Fiber) void {
        f.switchToLane(.yielded);
    }

    /// The lane side: switch into `f`; returns when it switches out
    /// (`f.msg` carries why). `noinline`: see `switchToLane`.
    pub noinline fn switchIn(f: *Fiber) void {
        var sw: Io.fiber.Switch = .{ .old = &f.lane.idle_ctx, .new = &f.ctx };
        _ = Io.fiber.contextSwitch(&sw);
    }

    /// The link register is ZEROED before the first frame: the switch
    /// arrives with the lane thread's return address in it, and a stack
    /// walk (a panic trace, the test allocator's capture) that followed it
    /// would apply the lane frames' unwind rules to the fiber's stack and
    /// read garbage. A zero return address ends every walker (`fp = 0`
    /// ends the frame-pointer chain the same way).
    fn entry() callconv(.naked) void {
        switch (builtin.cpu.arch) {
            .aarch64 => asm volatile (
                \\ mov x0, x1
                \\ mov x30, xzr
                \\ b %[call]
                :
                : [call] "X" (&call),
            ),
            .riscv64 => asm volatile (
                \\ mv a0, a1
                \\ mv ra, zero
                \\ tail %[call]@plt
                :
                : [call] "X" (&call),
            ),
            .x86_64 => asm volatile (
                \\ movq %%rsi, %%rdi
                \\ jmp %[call:P]
                :
                : [call] "X" (&call),
            ),
            else => unreachable,
        }
    }

    /// The fiber's first frame. `incoming_stack_alignment = 8`: the entry
    /// stub arrives with whatever the initial context carried, so the
    /// prologue realigns.
    fn call(contexts: *const Io.fiber.Switch) callconv(.withStackAlign(.c, 8)) noreturn {
        const f: *Fiber = @alignCast(@fieldParentPtr("ctx", contexts.new));
        switch (f.kind) {
            .future => f.start.future(&f.context, &f.result),
            .group => f.start.group(&f.context),
        }
        f.state.endPollComplete();
        f.switchToLane(.finished);
        unreachable; // a finished fiber is never switched into again
    }

    /// Recycle-time stack accounting (debug/test): the high-water mark and
    /// the half-stack tripwire.
    pub fn recordHighWater(f: *Fiber) void {
        if (!canary_enabled) return;
        const s = f.stack orelse return;
        const used = s.highWater();
        if (used > stats.fiber_stack_hwm_bytes.load()) stats.fiber_stack_hwm_bytes.set(used);
        if (used > s.usable().len / 2) stats.fiber_stack_half.fire("a fiber frame used more than half its stack");
        s.refillCanary(used);
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;

test "stack size derivation: page-aligned multiples of the 512 KiB class floor" {
    try testing.expectEqual(@as(usize, 512 * derive.KiB), stack_floor_bytes);
    try testing.expectEqual(2 * stack_floor_bytes, stackSizeFor(.fuse, 4096));
    try testing.expectEqual(2 * stack_floor_bytes, stackSizeFor(.meta, 4096));
    try testing.expectEqual(8 * stack_floor_bytes, stackSizeFor(.journal, 4096));
    try testing.expectEqual(8 * stack_floor_bytes, stackSizeFor(.main, 65536));
    try testing.expect(derive.isAligned(stackSizeFor(.fuse, 16384), 16384));
    try testing.expect(stackSizeFor(.fuse, 4096) >= 2 * 256 * derive.KiB);
}

test "CancelWord packs the request, the awaited state and the group pointer; sentinels are distinct" {
    var g: Io.Group = .init;
    var w = CancelWord.fromGroup(&g);
    try testing.expectEqual(@as(?*Io.Group, &g), w.groupPtr());
    try testing.expect(!w.requested);
    w.requested = true;
    w.awaiting = .ring_op;
    const back = CancelWord.fromInt(w.toInt());
    try testing.expect(back.requested);
    try testing.expectEqual(Awaiting.ring_op, back.awaiting);
    try testing.expectEqual(@as(?*Io.Group, &g), back.groupPtr());
    try testing.expectEqual(@as(?*Io.Group, null), CancelWord.fromGroup(null).groupPtr());
    try testing.expect(Fiber.finished != Fiber.raw_awaiter);
    try testing.expectEqual(Io.CancelProtection.blocked, Protection.blocked.check());
    try testing.expectEqual(Io.CancelProtection.blocked, (Protection{ .acknowledged = true }).check());
    try testing.expectEqual(Io.CancelProtection.unblocked, Protection.unblocked.check());
}

test "Stack.map: a guard page below a canary-filled usable region; the high-water mark follows the deepest touch; unmap returns the bytes" {
    const page = std.heap.pageSize();
    const before = stats.fiber_stack_bytes.load();
    const s = try Stack.map(stackSizeFor(.meta, page), page);
    try testing.expectEqual(before + s.len, stats.fiber_stack_bytes.load());
    try testing.expectEqual(@as(usize, 0), s.highWater());
    try testing.expect(derive.isAligned(s.top(), 16));
    const u = s.usable();
    u[u.len - 1] = 0;
    try testing.expectEqual(@as(usize, 1), s.highWater());
    u[u.len - 10_000] = 0;
    try testing.expectEqual(@as(usize, 10_000), s.highWater());
    s.refillCanary(10_000);
    try testing.expectEqual(@as(usize, 0), s.highWater());
    s.unmap();
    try testing.expectEqual(before, stats.fiber_stack_bytes.load());
}

test "the guard page faults: a write below the usable region kills a forked child with SIGSEGV" {
    const page = std.heap.pageSize();
    const pid_rc = linux.fork();
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(pid_rc));
    if (pid_rc == 0) {
        // The child: the test runner's segfault handler would turn the fault
        // into a traced abort; restore the default so the signal is the verdict.
        std.posix.sigaction(.SEGV, &.{ .handler = .{ .handler = std.posix.SIG.DFL }, .mask = std.posix.sigemptyset(), .flags = 0 }, null);
        // Map a stack and touch its guard.
        const s = Stack.map(stackSizeFor(.meta, page), page) catch linux.exit(3);
        const guard: *volatile u8 = @ptrCast(s.base);
        guard.* = 1;
        linux.exit(4); // reached only if the guard did not fault
    }
    const pid: linux.pid_t = @intCast(pid_rc);
    var status: i32 = 0;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.waitpid(pid, &status, 0)));
    try testing.expect(linux.W.IFSIGNALED(@bitCast(status)));
    try testing.expectEqual(linux.SIG.SEGV, linux.W.TERMSIG(@bitCast(status)));
}
