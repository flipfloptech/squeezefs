//! The coherence limiter for KD-11's residual windows (spec `docs/porting/
//! il_daemon.md` §3.3). PORTABLE: the ladder over an injected notify hook.
//!
//! An interception mount forces kernel write-through, so daemon state holds
//! every acked write from any transport — closed STRUCTURALLY. What is left
//! is W1: a ring write made bytes the KERNEL's page cache may still hold
//! stale for a buffered kernel READER. The daemon bounds it with
//! `FUSE_NOTIFY_INVAL_INODE`: a WHOLE invalidation on bind, on the FIRST
//! ring write of every `(ino, window)` (1 s), and on the last unbind of an
//! ino; a write inside the window that GREW the file past the announced
//! high-water fires ATTRS-ONLY (`off < 0`: attributes and ACLs, no page) —
//! POSIX-8: an `lseek(SEEK_END)` the shim routes to the kernel must learn
//! the size before appending over live data; everything else is
//! SUPPRESSED (`ipc_inval_suppressed`). The hook is a SYNCHRONOUS enqueue
//! into the session's sideband queue in the caller's context (the retired
//! spawn-per-fire was the inject-queue venue again); fire-and-forget by
//! contract — a ring completion may overtake the kernel-side invalidation.
//! W2 (cross-process `MAP_SHARED` readers) and W3(b) (cross-process mmap
//! writers) are declared unsupported; W4 inherits W1.
//!
//! The per-ino state is ONE striped word `{hwm_pages: u48, last_fire_s:
//! u16}` keyed `splitmix64(ino)` at the op-guard width (decision 0002 row
//! for the reference's two unreclaimed `scc::HashMap`s per written ino):
//! a collision costs one extra notify or one suppressed attrs-only the
//! kernel's attr TTL (default 1 s) bounds, never correctness; memory is a
//! constant. Coarse seconds suffice for a 1 s window; the high-water is
//! kept in 4 KiB pages (48 bits of pages covers every file the format can
//! name). The 16-bit second wraps every 18.2 h: a write landing exactly a
//! multiple of that after the last fire reads as inside the window — one
//! suppressed Whole the attr TTL bounds, never a stale page past it.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Atomic = std.atomic.Value;

const dlm = @import("../../meta/dlm.zig");
const knobs = @import("../../core/knobs.zig");
const stats = @import("stats.zig").stats;

/// The high-water's grain.
pub const HWM_PAGE: u64 = 4096;
/// `SQUEEZEFS_IPC_INVAL_WINDOW_MS`'s shipped default: one whole-inode
/// invalidation per written ino per second — the kernel's own default
/// attr TTL, so a buffered reader's view is never older than its TTL.
pub const WINDOW_MS_DEFAULT: u64 = 1000;

/// The notify hook (`Session.notifyInvalInode`): `off < 0` = attrs only,
/// `(0, 0)` = every page. A full queue is counted, never awaited.
pub const Hook = struct {
    ctx: ?*anyopaque,
    call: *const fn (ctx: ?*anyopaque, ino: u64, off: i64, len: i64) error{QueueFull}!void,
};

pub const Verdict = enum { whole, attrs_only, suppressed };

const Word = packed struct(u64) {
    hwm_pages: u48,
    last_fire_s: u16,
};

pub const Invalidator = struct {
    words: []Atomic(u64),
    mask: u64,
    hook: Hook,
    window_s: u16,

    pub fn init(gpa: Allocator, stripes: usize, hook: Hook, window_ms: u64) Allocator.Error!Invalidator {
        std.debug.assert(std.math.isPowerOfTwo(stripes));
        const words = try gpa.alloc(Atomic(u64), stripes);
        @memset(words, .init(0));
        // Coarse seconds, at least one (a sub-second window is "every write").
        return .{ .words = words, .mask = stripes - 1, .hook = hook, .window_s = @intCast(@max(1, (window_ms + 999) / 1000)) };
    }

    pub fn deinit(self: *Invalidator, gpa: Allocator) void {
        gpa.free(self.words);
        self.* = undefined;
    }

    pub fn windowMsFromEnv(env: *const std.process.Environ.Map) u64 {
        return knobs.intKnob("SQUEEZEFS_IPC_INVAL_WINDOW_MS", u64, env);
    }

    fn word(self: *Invalidator, ino: u64) *Atomic(u64) {
        return &self.words[@intCast(dlm.splitmix64(ino) & self.mask)];
    }

    fn fire(self: *Invalidator, ino: u64, off: i64, len: i64) void {
        self.hook.call(self.hook.ctx, ino, off, len) catch {
            stats.ipc_inval_queue_full.inc();
            return;
        };
        if (off < 0) stats.ipc_inval_attrs_only.inc() else stats.ipc_inval_notifies.inc();
    }

    /// A BIND of `ino` at `size`: the high-water resets to the announced
    /// size and the whole inode invalidates (the kernel may hold pages of a
    /// file the ring is about to write).
    pub fn onBind(self: *Invalidator, ino: u64, size: u64, now_s: u64) void {
        const w: Word = .{ .hwm_pages = @intCast(@min((size + HWM_PAGE - 1) / HWM_PAGE, std.math.maxInt(u48))), .last_fire_s = @truncate(now_s) };
        self.word(ino).store(@bitCast(w), .monotonic);
        self.fire(ino, 0, 0);
    }

    /// The last binding of `ino` across every live session went: whole.
    pub fn onLastUnbind(self: *Invalidator, ino: u64, now_s: u64) void {
        var w: Word = @bitCast(self.word(ino).load(.monotonic));
        w.last_fire_s = @truncate(now_s);
        self.word(ino).store(@bitCast(w), .monotonic);
        self.fire(ino, 0, 0);
    }

    /// A ring write acked at `end`: the ladder (module doc). One load, one
    /// CAS; a lost CAS re-reads (another write of the stripe moved it).
    pub fn onWrite(self: *Invalidator, ino: u64, end: u64, now_s: u64) Verdict {
        const end_pages: u48 = @intCast(@min((end + HWM_PAGE - 1) / HWM_PAGE, std.math.maxInt(u48)));
        const now16: u16 = @truncate(now_s);
        const cell = self.word(ino);
        var cur = cell.load(.monotonic);
        while (true) {
            const w: Word = @bitCast(cur);
            const due = now16 -% w.last_fire_s >= self.window_s;
            const grew = end_pages > w.hwm_pages;
            if (!due and !grew) {
                stats.ipc_inval_suppressed.inc();
                return .suppressed;
            }
            const next: Word = .{ .hwm_pages = @max(w.hwm_pages, end_pages), .last_fire_s = if (due) now16 else w.last_fire_s };
            if (cell.cmpxchgWeak(cur, @bitCast(next), .monotonic, .monotonic)) |seen| {
                cur = seen;
                continue;
            }
            if (due) {
                self.fire(ino, 0, 0);
                return .whole;
            }
            self.fire(ino, -1, 0);
            return .attrs_only;
        }
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;

const Capture = struct {
    whole: u32 = 0,
    attrs: u32 = 0,
    full: bool = false,
    last_ino: u64 = 0,

    fn call(ctx: ?*anyopaque, ino: u64, off: i64, _: i64) error{QueueFull}!void {
        const self: *Capture = @ptrCast(@alignCast(ctx.?));
        if (self.full) return error.QueueFull;
        self.last_ino = ino;
        if (off < 0) self.attrs += 1 else self.whole += 1;
    }
};

test "the ladder: bind fires whole and seeds the high-water; the first write of a window fires whole; a growing write inside the window fires attrs-only; a non-growing one is suppressed; the next window fires whole again; the last unbind fires whole; a full queue is counted, never awaited" {
    var cap: Capture = .{};
    var inv = try Invalidator.init(testing.allocator, 64, .{ .ctx = &cap, .call = &Capture.call }, WINDOW_MS_DEFAULT);
    defer inv.deinit(testing.allocator);
    const sup0 = stats.ipc_inval_suppressed.load();
    const full0 = stats.ipc_inval_queue_full.load();
    inv.onBind(7, 8192, 100);
    try testing.expectEqual(@as(u32, 1), cap.whole);
    // Inside the window: a write below the high-water is suppressed.
    try testing.expectEqual(Verdict.suppressed, inv.onWrite(7, 4096, 100));
    try testing.expectEqual(sup0 + 1, stats.ipc_inval_suppressed.load());
    // A write that grows the file: attrs only, the high-water moves.
    try testing.expectEqual(Verdict.attrs_only, inv.onWrite(7, 20_000, 100));
    try testing.expectEqual(@as(u32, 1), cap.attrs);
    try testing.expectEqual(Verdict.suppressed, inv.onWrite(7, 20_000, 100));
    // The next second: whole again (and the high-water keeps the max).
    try testing.expectEqual(Verdict.whole, inv.onWrite(7, 100, 101));
    try testing.expectEqual(@as(u32, 2), cap.whole);
    try testing.expectEqual(Verdict.suppressed, inv.onWrite(7, 20_000, 101));
    // The u16 second wraps: a plain difference across the wrap.
    try testing.expectEqual(Verdict.whole, inv.onWrite(7, 100, 65536 + 102));
    inv.onLastUnbind(7, 200);
    try testing.expectEqual(@as(u32, 4), cap.whole);
    try testing.expectEqual(@as(u64, 7), cap.last_ino);
    // A full sideband queue: counted, the verdict stands, nothing waits.
    cap.full = true;
    try testing.expectEqual(Verdict.whole, inv.onWrite(7, 100, 300));
    try testing.expectEqual(full0 + 1, stats.ipc_inval_queue_full.load());
    try testing.expectEqual(@as(u32, 4), cap.whole);
    // The table is bounded: 64 words, whatever the ino population.
    try testing.expectEqual(@as(usize, 64), inv.words.len);
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expectEqual(WINDOW_MS_DEFAULT, Invalidator.windowMsFromEnv(&env));
}
