//! The coherence limiter for KD-11's residual windows (spec `docs/porting/
//! il_daemon.md` §3.3; decision 0003 §CE). PORTABLE: the ladder over an
//! injected notify hook and the mount's ring table (`ring_inodes.zig`).
//!
//! An interception mount forces kernel write-through, so daemon state holds
//! every acked write from any transport — closed STRUCTURALLY. What is left
//! is the kernel's own view of a file the RING changed: its cached
//! attributes (size, times) and pages (W1: a buffered kernel reader; the
//! kernel drops no page on an mtime it sees move — `FUSE_AUTO_INVAL_DATA`
//! is never advertised, decision 0002 row 446). The mount is ISOLATED for
//! the TTL law (§AB.2, §CE): every class is `T_owner`, so coherence is
//! driven by invalidation, never by expiry — the limiter tells the kernel
//! every ring change with `FUSE_NOTIFY_INVAL_INODE`:
//!   * the FIRST ring write of a binding, and the first of every window
//!     (1 s) after it: WHOLE (attributes and every page) — the LEADING edge;
//!   * a write that GREW the file past the announced high-water: the size,
//!     ATTRS-ONLY (`off < 0`), AWAITED before the ack — POSIX-8: the
//!     `fstat` or `lseek(SEEK_END)` a kernel-path reader issues after
//!     `write(2)` returned sees the new length (the shim battery's row 4b);
//!   * every other write inside the window is SUPPRESSED and marks its
//!     binding's row DIRTY (the clean → dirty edge raises its session's
//!     sweep flag): the window's TRAILING sweep (`IpcHost.sweepDirty`, on
//!     the ctl poller once per window), the binding's UNBIND and its
//!     session's teardown each fire WHOLE for a dirty row — so a buffered
//!     reader's pages and times are never older than two windows;
//!   * a write whose binding is already gone (its session torn down with
//!     the write in flight) fires WHOLE itself — the Dekker pair with the
//!     unbind (`ring_inodes_model.zig`);
//!   * a ring read that parked an access time: ATTRS-ONLY.
//! A BIND fires nothing: until the ring changes the file, the kernel's view
//! is the daemon's. No fire is ever dropped: a full shared queue
//! (`ipc_inval_queue_full`) falls back to the attrs-only reserved cells —
//! the kernel's attributes die at once and its pages at the next fire. The
//! whole notify stays fire-and-forget (it may sleep a READ round trip in
//! the kernel's `invalidate_inode_pages2_range`, so a device write awaited
//! on this path is banned): a ring completion may overtake it — never the
//! size. W2 (cross-process `MAP_SHARED` readers) and W3(b) (cross-process
//! mmap writers) are declared unsupported; W4 inherits W1.
//!
//! The per-ino word is ONE striped `ring_inodes.Word` keyed
//! `splitmix64(ino)` at the op-guard width (decision 0002 row for the
//! reference's two unreclaimed `scc::HashMap`s per written ino): a
//! collision costs one suppressed leading fire (the trailing sweep covers
//! it) or one spurious size announcement; memory is a constant. The
//! dirty mark is the binding row's own (`Session.Binding.dirty`), never
//! shared. The high-water is kept in BYTES (a grow inside one page is a
//! grow). The 13-bit second wraps every 2.3 h: a write landing exactly a
//! multiple of that after the last fire reads as inside the window —
//! suppressed and dirty, so swept.

const std = @import("std");
const Atomic = std.atomic.Value;

const knobs = @import("../../core/knobs.zig");
const ring_inodes = @import("../ring_inodes.zig");
const stats = @import("stats.zig").stats;

const Word = ring_inodes.Word;

/// `SQUEEZEFS_IPC_INVAL_WINDOW_MS`'s shipped default: one leading and at
/// most one trailing whole-inode invalidation per written binding per
/// second — the charter's attr class, the staleness the reference's 1 s TTL
/// allowed a kernel reader.
pub const WINDOW_MS_DEFAULT: u64 = 1000;

/// The window's ceiling in seconds: half the word's 13-bit second, so the
/// wrapping difference stays an age.
pub const WINDOW_S_MAX: u13 = std.math.maxInt(u13) / 2;

/// The notify hook (`Session.notifyInvalInode` and its attrs-only kin).
pub const Hook = struct {
    ctx: ?*anyopaque,
    /// From the shared cells, fire-and-forget: `off < 0` = attrs only,
    /// `(0, 0)` = every page; `QueueFull` when no cell is free.
    call: *const fn (ctx: ?*anyopaque, ino: u64, off: i64, len: i64) error{QueueFull}!void,
    /// Attrs-only, returning once the kernel took it
    /// (`Session.notifyAttrsAwaited`).
    sync: *const fn (ctx: ?*anyopaque, ino: u64) void,
    /// Attrs-only from the reserved cells, waiting for one — never dropped
    /// (`Session.notifyAttrs`).
    attrs: *const fn (ctx: ?*anyopaque, ino: u64) void,
};

pub const Verdict = enum { whole, attrs_only, suppressed };

/// A ring write's binding row as its write reads and marks it: the
/// session's teardown, the row's liveness and dirty mark, the session's
/// sweep flag. SeqCst everywhere: the row's mark is in memory before the
/// write reads the binding (the Dekker pair with `fireIfDirty` after an
/// unbind or a teardown stored it dead), and the flag is raised after the
/// mark (the sweep's order).
pub const Row = struct {
    torn_down: *const Atomic(bool),
    live: *const Atomic(bool),
    dirty: *Atomic(bool),
    sweep: *Atomic(bool),

    pub fn alive(self: Row) bool {
        return !self.torn_down.load(.seq_cst) and self.live.load(.seq_cst);
    }
};

pub const Invalidator = struct {
    table: *const ring_inodes.Table,
    hook: Hook,
    window_s: u13,

    pub fn init(table: *const ring_inodes.Table, hook: Hook, window_ms: u64) Invalidator {
        // Coarse seconds, at least one (a sub-second window is "every write").
        const s = @max(1, (window_ms + 999) / 1000);
        return .{ .table = table, .hook = hook, .window_s = @intCast(@min(s, WINDOW_S_MAX)) };
    }

    pub fn windowMsFromEnv(env: *const std.process.Environ.Map) u64 {
        return knobs.intKnob("SQUEEZEFS_IPC_INVAL_WINDOW_MS", u64, env);
    }

    /// The trailing sweep's period (`IpcHost.pollLoop`).
    pub fn windowNs(self: *const Invalidator) u64 {
        return @as(u64, self.window_s) * std.time.ns_per_s;
    }

    /// WHOLE, never dropped: a full shared queue falls back to the reserved
    /// attrs-only cells.
    fn fireWhole(self: *const Invalidator, ino: u64) void {
        self.hook.call(self.hook.ctx, ino, 0, 0) catch {
            stats.ipc_inval_queue_full.inc();
            self.hook.attrs(self.hook.ctx, ino);
            stats.ipc_inval_attrs_only.inc();
            return;
        };
        stats.ipc_inval_notifies.inc();
    }

    /// A BIND of `ino` at the daemon's `size`, BEFORE it is answered (no
    /// ring write of the binding precedes it): the high-water is the
    /// announced size and the binding's first write is due. Nothing fires:
    /// the ring changed nothing yet.
    pub fn onBind(self: *const Invalidator, ino: u64, size: u64, now_s: u64) void {
        const cell = self.table.word(ino);
        var cur = cell.load(.seq_cst);
        while (true) {
            var w: Word = @bitCast(cur);
            w.hwm = @intCast(@min(size, std.math.maxInt(u48)));
            w.last_fire_s = @as(u13, @truncate(now_s)) -% self.window_s;
            cur = cell.cmpxchgWeak(cur, @bitCast(w), .seq_cst, .seq_cst) orelse return;
        }
    }

    /// A dirty row's whole fire, once: an UNBIND or a teardown (the row or
    /// its session stored dead first) or the trailing sweep. True = fired.
    pub fn fireIfDirty(self: *const Invalidator, ino: u64, dirty: *Atomic(bool)) bool {
        if (!dirty.swap(false, .seq_cst)) return false;
        self.fireWhole(ino);
        return true;
    }

    /// A ring write ended at `end`, after its RAM apply and BEFORE its ack:
    /// the ladder (module doc). `gen` is the stripe's size-set generation
    /// read before the write's admission (`Table.genOf`; null = a direct
    /// arm's note — in-size writes by the lease's law, raising nothing):
    /// a size set since then leaves the high-water where the kernel's
    /// truncate put it. A grow returns only once the kernel took the size.
    pub fn onWrite(self: *const Invalidator, ino: u64, end: u64, now_s: u64, gen: ?u3, row: Row) Verdict {
        const end48: u48 = @intCast(@min(end, std.math.maxInt(u48)));
        const now13: u13 = @truncate(now_s);
        const cell = self.table.word(ino);
        var cur = cell.load(.seq_cst);
        var due = false;
        var grew = false;
        while (true) {
            const w: Word = @bitCast(cur);
            due = now13 -% w.last_fire_s >= self.window_s;
            // Past the word's reach every write is a grow.
            grew = end48 > w.hwm or end > std.math.maxInt(u48);
            var next = w;
            if (gen) |g| if (g == w.gen) {
                next.hwm = @max(w.hwm, end48);
            };
            if (due) next.last_fire_s = now13;
            if (@as(u64, @bitCast(next)) == cur) break;
            cur = cell.cmpxchgWeak(cur, @bitCast(next), .seq_cst, .seq_cst) orelse break;
        }
        if (grew) {
            self.hook.sync(self.hook.ctx, ino);
            stats.ipc_inval_attrs_only.inc();
        }
        if (due) {
            self.fireWhole(ino);
            return .whole;
        }
        // The row's mark, then (on its clean → dirty edge) the sweep flag;
        // then the binding: one already gone fires its own.
        if (!row.dirty.swap(true, .seq_cst)) row.sweep.store(true, .seq_cst);
        if (!row.alive()) {
            // An unbind that swapped the mark off fired for it already.
            if (self.fireIfDirty(ino, row.dirty)) stats.ipc_inval_dead_binding_fires.inc();
            return .whole;
        }
        if (grew) return .attrs_only;
        stats.ipc_inval_suppressed.inc();
        return .suppressed;
    }

    /// A ring read parked an access time the kernel's cached attributes do
    /// not show (`note_read` answered true): attrs-only, fire-and-forget,
    /// never dropped.
    pub fn onAtime(self: *const Invalidator, ino: u64) void {
        stats.ipc_inval_atime.inc();
        self.hook.call(self.hook.ctx, ino, -1, 0) catch {
            stats.ipc_inval_queue_full.inc();
            self.hook.attrs(self.hook.ctx, ino);
        };
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;

const Capture = struct {
    whole: u32 = 0,
    attrs: u32 = 0,
    awaited: u32 = 0,
    reserved: u32 = 0,
    full: bool = false,
    last_ino: u64 = 0,

    fn call(ctx: ?*anyopaque, ino: u64, off: i64, _: i64) error{QueueFull}!void {
        const self: *Capture = @ptrCast(@alignCast(ctx.?));
        if (self.full) return error.QueueFull;
        self.last_ino = ino;
        if (off < 0) self.attrs += 1 else self.whole += 1;
    }
    fn sync(ctx: ?*anyopaque, ino: u64) void {
        const self: *Capture = @ptrCast(@alignCast(ctx.?));
        self.last_ino = ino;
        self.awaited += 1;
    }
    fn reservedAttrs(ctx: ?*anyopaque, ino: u64) void {
        const self: *Capture = @ptrCast(@alignCast(ctx.?));
        self.last_ino = ino;
        self.reserved += 1;
    }
    fn hook(self: *Capture) Hook {
        return .{ .ctx = self, .call = &call, .sync = &sync, .attrs = &reservedAttrs };
    }
};

const TestRow = struct {
    torn_down: Atomic(bool) = .init(false),
    live: Atomic(bool) = .init(true),
    dirty: Atomic(bool) = .init(false),
    sweep: Atomic(bool) = .init(false),
    fn row(self: *TestRow) Row {
        return .{ .torn_down = &self.torn_down, .live = &self.live, .dirty = &self.dirty, .sweep = &self.sweep };
    }
};

test "the ladder (decision 0003 §CE): a bind fires nothing; its first write is due (whole); a grow is announced awaited; a write inside the window is suppressed and marks its row dirty (the clean → dirty edge raises the sweep flag); the sweep, a dirty unbind each fire whole once; a clean unbind fires nothing; the next window fires whole again" {
    var cap: Capture = .{};
    var table = try ring_inodes.Table.init(testing.allocator, 64);
    defer table.deinit(testing.allocator);
    const inv = Invalidator.init(&table, cap.hook(), WINDOW_MS_DEFAULT);
    var tr: TestRow = .{};
    const sup0 = stats.ipc_inval_suppressed.load();

    inv.onBind(7, 8192, 100);
    try testing.expectEqual(@as(u32, 0), cap.whole + cap.attrs + cap.awaited + cap.reserved);
    // The binding's first write: due, whatever the second.
    try testing.expectEqual(Verdict.whole, inv.onWrite(7, 4096, 100, table.genOf(7), tr.row()));
    try testing.expectEqual(@as(u32, 1), cap.whole);
    try testing.expect(!tr.dirty.load(.seq_cst));
    // Inside the window: below the high-water is suppressed; the row is
    // dirty and the session's sweep flag up.
    try testing.expectEqual(Verdict.suppressed, inv.onWrite(7, 4096, 100, table.genOf(7), tr.row()));
    try testing.expectEqual(sup0 + 1, stats.ipc_inval_suppressed.load());
    try testing.expect(tr.dirty.load(.seq_cst) and tr.sweep.load(.seq_cst));
    // A grow: the size announced (awaited), the high-water moves — in
    // BYTES: a grow inside the page is a grow. The row stays dirty.
    tr.sweep.store(false, .seq_cst);
    try testing.expectEqual(Verdict.attrs_only, inv.onWrite(7, 8200, 100, table.genOf(7), tr.row()));
    try testing.expectEqual(@as(u32, 1), cap.awaited);
    try testing.expect(!tr.sweep.load(.seq_cst)); // already dirty: no second raise
    try testing.expectEqual(Verdict.suppressed, inv.onWrite(7, 8200, 100, table.genOf(7), tr.row()));
    // The trailing sweep: one whole for the dirty row, then nothing.
    try testing.expect(inv.fireIfDirty(7, &tr.dirty));
    try testing.expectEqual(@as(u32, 2), cap.whole);
    try testing.expect(!inv.fireIfDirty(7, &tr.dirty));
    // The next second: whole again (the high-water keeps the max).
    try testing.expectEqual(Verdict.whole, inv.onWrite(7, 100, 101, table.genOf(7), tr.row()));
    try testing.expectEqual(@as(u32, 3), cap.whole);
    // A clean unbind fires nothing; a dirty one fires whole.
    tr.live.store(false, .seq_cst);
    try testing.expect(!inv.fireIfDirty(7, &tr.dirty));
    try testing.expectEqual(@as(u32, 3), cap.whole);
    tr.live.store(true, .seq_cst);
    inv.onBind(7, 8200, 200);
    try testing.expectEqual(Verdict.whole, inv.onWrite(7, 4096, 200, table.genOf(7), tr.row()));
    try testing.expectEqual(Verdict.suppressed, inv.onWrite(7, 4096, 200, table.genOf(7), tr.row()));
    tr.live.store(false, .seq_cst);
    try testing.expect(inv.fireIfDirty(7, &tr.dirty));
    try testing.expectEqual(@as(u32, 5), cap.whole);
    try testing.expectEqual(@as(u64, std.time.ns_per_s), inv.windowNs());
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expectEqual(WINDOW_MS_DEFAULT, Invalidator.windowMsFromEnv(&env));
}

test "the ladder's edges (decision 0003 §CE): a write on a dead binding fires whole itself; a full shared queue falls back to the reserved attrs-only cells (never dropped); a kernel size set lowers the high-water so the regrow is announced, and a write admitted before it raises nothing past it; a parked access time is announced attrs-only" {
    var cap: Capture = .{};
    var table = try ring_inodes.Table.init(testing.allocator, 64);
    defer table.deinit(testing.allocator);
    const inv = Invalidator.init(&table, cap.hook(), WINDOW_MS_DEFAULT);
    var tr: TestRow = .{};
    inv.onBind(9, 8192, 100);
    try testing.expectEqual(Verdict.whole, inv.onWrite(9, 4096, 100, table.genOf(9), tr.row()));
    // The session tore down with a write in flight: the write reads the
    // binding dead and fires whole, the window notwithstanding.
    const dead0 = stats.ipc_inval_dead_binding_fires.load();
    tr.torn_down.store(true, .seq_cst);
    try testing.expectEqual(Verdict.whole, inv.onWrite(9, 4096, 100, table.genOf(9), tr.row()));
    try testing.expectEqual(dead0 + 1, stats.ipc_inval_dead_binding_fires.load());
    try testing.expectEqual(@as(u32, 2), cap.whole);
    try testing.expect(!tr.dirty.load(.seq_cst));
    tr.torn_down.store(false, .seq_cst);
    // A full shared queue: the whole falls back to a reserved attrs-only.
    const full0 = stats.ipc_inval_queue_full.load();
    cap.full = true;
    try testing.expectEqual(Verdict.whole, inv.onWrite(9, 4096, 101, table.genOf(9), tr.row()));
    try testing.expectEqual(@as(u32, 1), cap.reserved);
    try testing.expectEqual(full0 + 1, stats.ipc_inval_queue_full.load());
    cap.full = false;
    // The kernel truncates to 0 (between a write's admission and its
    // announcement): that write raises nothing; the regrow after it is
    // announced although it ends below the old high-water.
    const g_before = table.genOf(9);
    table.sizeSet(9, 0);
    try testing.expectEqual(Verdict.attrs_only, inv.onWrite(9, 8192, 101, g_before, tr.row()));
    try testing.expectEqual(@as(u32, 1), cap.awaited);
    try testing.expectEqual(@as(u48, 0), (@as(Word, @bitCast(table.word(9).load(.seq_cst)))).hwm);
    try testing.expectEqual(Verdict.attrs_only, inv.onWrite(9, 4096, 101, table.genOf(9), tr.row()));
    try testing.expectEqual(@as(u32, 2), cap.awaited);
    try testing.expectEqual(Verdict.suppressed, inv.onWrite(9, 4096, 101, table.genOf(9), tr.row()));
    // A direct arm's note raises nothing (in-size by the lease's law).
    try testing.expectEqual(Verdict.attrs_only, inv.onWrite(9, 6000, 101, null, tr.row()));
    try testing.expectEqual(@as(u48, 4096), (@as(Word, @bitCast(table.word(9).load(.seq_cst)))).hwm);
    // A parked access time: attrs-only from the shared cells, else reserved.
    const at0 = stats.ipc_inval_atime.load();
    inv.onAtime(9);
    try testing.expectEqual(@as(u32, 1), cap.attrs);
    cap.full = true;
    inv.onAtime(9);
    try testing.expectEqual(@as(u32, 2), cap.reserved);
    try testing.expectEqual(at0 + 2, stats.ipc_inval_atime.load());
    // The window's ceiling keeps the 13-bit second an age.
    try testing.expectEqual(WINDOW_S_MAX, Invalidator.init(&table, cap.hook(), std.math.maxInt(u32)).window_s);
}
