//! The bound-fd table (spec `docs/porting/il_shim.md` §1.9–§1.11, §1.14,
//! §2): fd → refcounted binding cell, lock-free on the data path, with the
//! offset mirror (PERF-7), the fork epoch, the lane gate's sticky latch,
//! the negative device cache and the never-forgetting MAP_SHARED inode set.
//!
//! Shape: two-level slots (`SEGMENTS × SEGMENT_FDS` words, segments mapped
//! lazily) naming cells by index + 1 (0 = unbound); cells live in lazily
//! mapped chunks and RECYCLE through a tagged-index Treiber free list once
//! their refcount hits zero (decision 0002 ledger: the reference leaked
//! every cell for the process life). A data-path lookup PINS the cell
//! (acquire refusing 0, then a re-validation that the slot still names it —
//! a cell recycled for another fd is never used under this fd), so a
//! sibling's `mmap`/`close_range`/fork walk can release the last reference
//! under a live op without the op touching freed memory.
//!
//! Dup siblings share ONE cell (one offset, one lock, one latch — exactly
//! kernel shared `f_pos`). Refcount law: monotone-to-zero CAS loops
//! (`acquire` refuses 0; `release` saturates at 0 — a wrapped `fetch_sub` is
//! a binding that can never report again).
//!
//! The offset mirror is AUTHORITATIVE iff (a) armed at bind with a fork-
//! epoch snapshot taken BEFORE the fd's `open`, (b) that snapshot equals
//! the table's current epoch, (c) nothing has disarmed it. The arm is
//! monotone (never re-arms). Every transition out of authority flushes
//! mirror → kernel `f_pos` first; the op side's `publish` and the demote
//! side's `disarmIfCurrent` are the SeqCst Dekker pair modelled in
//! `fd_table_model.zig` (either side's SeqCst removed loses the final
//! offset — the recorded weakening).
//!
//! Async-signal posture: NO mutex guards any table operation (`close` is
//! AS-safe by POSIX and apps call it from handlers). The only lock is the
//! per-description offset lock, held across an offsetful op's serve window.

const std = @import("std");
const builtin = @import("builtin");
const Atomic = std.atomic.Value;
const wire = @import("../ipc/wire.zig");

pub const Rights = wire.Rights;

/// fds < 2^22 — beyond any `RLIMIT_NOFILE`; segments of 1024 fds so
/// `close_range(3, ~0)` (the systemd idiom) skips unallocated segments in
/// O(1) each.
pub const SEGMENT_FDS: u32 = 1024;
pub const SEGMENTS: u32 = 4096;
pub const MAX_FD: u32 = SEGMENTS * SEGMENT_FDS;
/// Cells per mapped chunk (the same grain as a segment; 1024 × 128 B).
pub const CHUNK_CELLS: u32 = 1024;
pub const CHUNKS: u32 = 4096;
/// `forkFlushAll`'s `tryLock` budget: an in-flight offsetful op holds the
/// lock only for its serve window (µs); a lock still held after 1024 spins
/// belongs to a thread the fork is about to orphan — its mirror is left
/// unflushed (the child's `f_pos` is the kernel's last word, as it would be
/// without the shim).
pub const FORK_TRYLOCK_SPINS: u32 = 1024;

/// A raw memory source for segments and chunks (the shim passes the raw
/// anonymous `mmap`; tests pass a page allocator). Never freed.
pub const MapFn = *const fn (len: usize) ?[*]align(4096) u8;

/// What a cell binds: the registry session, the daemon's binding, the
/// inode (for the per-inode `mmap` unbind) and the rights.
pub const Binding = struct {
    session: u32,
    id: u64,
    dev: u64,
    ino: u64,
    rights: Rights,
};

/// The release of a reference: `last` ⇒ the caller sends the Unbind ctl
/// (silent on a poisoned session).
pub const Drop = struct {
    last: bool,
    binding: Binding,
};

/// A mirror flush the caller writes through with raw `SEEK_SET`.
pub const Flush = struct {
    fd: i32,
    offset: u64,
};

// ---------------------------------------------------------------------------
// The per-description offset lock: Drepper's three-state futex mutex with
// `tryLock`. Linux parks on the futex; the portable arm (the dev box's unit
// tests of the mirror transitions) yields.
// ---------------------------------------------------------------------------

pub const Lock = struct {
    word: Atomic(u32) = .init(0),

    pub fn tryLock(self: *Lock) bool {
        return self.word.cmpxchgStrong(0, 1, .acquire, .monotonic) == null;
    }

    pub fn lock(self: *Lock) void {
        if (self.tryLock()) return;
        while (self.word.swap(2, .acquire) != 0) park(&self.word, 2);
    }

    pub fn unlock(self: *Lock) void {
        if (self.word.swap(0, .release) == 2) unpark(&self.word);
    }

    fn park(word: *const Atomic(u32), expect: u32) void {
        if (builtin.os.tag == .linux) {
            const linux = std.os.linux;
            _ = linux.futex_4arg(&word.raw, .{ .cmd = .WAIT, .private = true }, expect, null);
        } else {
            std.atomic.spinLoopHint();
        }
    }

    fn unpark(word: *const Atomic(u32)) void {
        if (builtin.os.tag == .linux) {
            const linux = std.os.linux;
            _ = linux.futex_3arg(&word.raw, .{ .cmd = .WAKE, .private = true }, 1);
        }
    }
};

// ---------------------------------------------------------------------------
// The cell.
// ---------------------------------------------------------------------------

pub const Cell = struct {
    refs: Atomic(u32) = .init(0),
    /// This cell's pool index (the free list links by index).
    index: u32 = 0,
    next_free: u32 = 0,
    binding: Binding = undefined,
    /// The mirror: armed (monotone off), the fork-epoch snapshot the arm
    /// was taken under, the offset.
    armed: Atomic(bool) = .init(false),
    epoch: u64 = 0,
    offset: Atomic(u64) = .init(0),
    /// The hybrid lane gate's sticky kernel-lane latch (per description:
    /// the first over-threshold offsetful op flushes + demotes once; every
    /// later offsetful op takes the real call at native cost).
    kernel_latch: Atomic(bool) = .init(false),
    lock: Lock = .{},

    /// Refcount acquire: refuses a count already at zero (a racing last
    /// close won). Acquire pairs with the bind's Release publish.
    pub fn acquire(self: *Cell) bool {
        var cur = self.refs.load(.acquire);
        while (true) {
            if (cur == 0) return false;
            cur = self.refs.cmpxchgWeak(cur, cur + 1, .acq_rel, .acquire) orelse return true;
        }
    }

    /// Refcount release, saturating at zero; true = this was the last.
    pub fn release(self: *Cell) bool {
        var cur = self.refs.load(.acquire);
        while (true) {
            if (cur == 0) return false;
            cur = self.refs.cmpxchgWeak(cur, cur - 1, .acq_rel, .acquire) orelse return cur == 1;
        }
    }

    /// The authority predicate (module doc).
    pub fn authoritative(self: *const Cell, table_epoch: u64) bool {
        return self.armed.load(.acquire) and self.epoch == table_epoch;
    }

    /// Op side of the Dekker pair: publish the post-op offset, then re-check
    /// the arm. `false` ⇒ a demote raced and OWNS no flush of this value:
    /// the caller writes `offset` through to the kernel itself.
    pub fn publish(self: *Cell, offset: u64) bool {
        self.offset.store(offset, .seq_cst); // paired with disarmIfCurrent's swap
        return self.armed.load(.seq_cst);
    }

    /// Demote side: disarm ONCE and capture the offset the arm covered.
    /// Null for a never-armed, already-disarmed or stale cell — flushing a
    /// never-armed seed would REWIND an offset the kernel advanced.
    pub fn disarmIfCurrent(self: *Cell, table_epoch: u64) ?u64 {
        if (!self.armed.load(.acquire) or self.epoch != table_epoch) return null;
        if (!self.armed.swap(false, .seq_cst)) return null; // paired with publish's store
        return self.offset.load(.seq_cst);
    }

    /// `lseek` arithmetic over an armed mirror: SEEK_SET/SEEK_CUR exactly
    /// `vfs_setpos` (a negative or sign-wrapped result is EINVAL). Other
    /// whences are the kernel's (the caller re-seeds on success).
    pub fn seekArith(self: *Cell, whence: u32, off: i64) ?u64 {
        const cur: i64 = @intCast(@min(self.offset.load(.monotonic), std.math.maxInt(i64)));
        const target: i64 = switch (whence) {
            0 => off,
            1 => std.math.add(i64, cur, off) catch return null,
            else => return null,
        };
        if (target < 0) return null;
        return @intCast(target);
    }
};

comptime {
    // A cell stays inside one line pair: the refcount, the mirror and the
    // lock are touched together by every offsetful op.
    std.debug.assert(@sizeOf(Cell) <= 128);
}

// ---------------------------------------------------------------------------
// The table.
// ---------------------------------------------------------------------------

/// A pinned cell: the lookup took a reference; `unpin` releases it (and
/// reports the last-ref drop a racing close left to us).
pub const Pin = struct {
    cell: *Cell,

    pub fn unpin(self: Pin) ?Drop {
        if (self.cell.release()) return .{ .last = true, .binding = self.cell.binding };
        return null;
    }
};

pub const Table = struct {
    segments: [SEGMENTS]Atomic(?[*]Atomic(u32)) = @splat(.init(null)),
    chunks: [CHUNKS]Atomic(?[*]Cell) = @splat(.init(null)),
    next_fresh: Atomic(u32) = .init(0),
    /// `tag << 32 | (index + 1)`; 0 = empty. The tag defeats the pop/push
    /// ABA on a recycled head.
    free_head: Atomic(u64) = .init(0),
    /// The fork epoch: bumped by every fork PREPARE / `posix_spawn` flush;
    /// a mirror armed under an older epoch is not authoritative.
    epoch: Atomic(u64) = .init(0),
    map: MapFn,
    /// Cells that could not be allocated (the mapper refused): the bind
    /// falls through; must stay 0 in any healthy process.
    alloc_failures: Atomic(u64) = .init(0),

    pub fn init(map: MapFn) Table {
        return .{ .map = map };
    }

    pub fn currentEpoch(self: *const Table) u64 {
        return self.epoch.load(.acquire);
    }

    pub fn bumpEpoch(self: *Table) u64 {
        return self.epoch.fetchAdd(1, .acq_rel) + 1;
    }

    // ---- slots ----

    fn slotPtr(self: *Table, fd: i32, create: bool) ?*Atomic(u32) {
        if (fd < 0) return null;
        const u: u32 = @intCast(fd);
        if (u >= MAX_FD) return null;
        const si = u / SEGMENT_FDS;
        var seg = self.segments[si].load(.acquire);
        if (seg == null) {
            if (!create) return null;
            const mem = self.map(SEGMENT_FDS * @sizeOf(Atomic(u32))) orelse return null;
            const fresh: [*]Atomic(u32) = @ptrCast(mem);
            // Zero = unbound; the mapper's pages are zero already, but the
            // test allocator's may not be.
            for (fresh[0..SEGMENT_FDS]) |*w| w.* = .init(0);
            if (self.segments[si].cmpxchgStrong(null, fresh, .acq_rel, .acquire)) |won| {
                // A racing allocator won; ours is abandoned (never freed:
                // a mapper has no unmap, and the race is once per segment).
                seg = won;
            } else seg = fresh;
        }
        return &seg.?[u % SEGMENT_FDS];
    }

    // ---- cells ----

    pub fn cellAt(self: *Table, index: u32) *Cell {
        const chunk = self.chunks[index / CHUNK_CELLS].load(.acquire).?;
        return &chunk[index % CHUNK_CELLS];
    }

    fn popFree(self: *Table) ?*Cell {
        var head = self.free_head.load(.acquire);
        while (true) {
            const idx1: u32 = @truncate(head);
            if (idx1 == 0) return null;
            const cell = self.cellAt(idx1 - 1);
            const tag: u64 = (head >> 32) + 1;
            const next: u64 = (tag << 32) | cell.next_free;
            head = self.free_head.cmpxchgWeak(head, next, .acq_rel, .acquire) orelse return cell;
        }
    }

    fn pushFree(self: *Table, cell: *Cell) void {
        var head = self.free_head.load(.acquire);
        while (true) {
            cell.next_free = @truncate(head);
            const tag: u64 = (head >> 32) + 1;
            const next: u64 = (tag << 32) | (@as(u64, cell.index) + 1);
            head = self.free_head.cmpxchgWeak(head, next, .acq_rel, .acquire) orelse return;
        }
    }

    fn freshCell(self: *Table) ?*Cell {
        const index = self.next_fresh.fetchAdd(1, .monotonic);
        if (index >= CHUNKS * CHUNK_CELLS) return null;
        const ci = index / CHUNK_CELLS;
        if (self.chunks[ci].load(.acquire) == null) {
            const mem = self.map(CHUNK_CELLS * @sizeOf(Cell)) orelse return null;
            const fresh: [*]Cell = @ptrCast(mem);
            for (fresh[0..CHUNK_CELLS], 0..) |*c, i| c.* = .{ .index = ci * CHUNK_CELLS + @as(u32, @intCast(i)) };
            _ = self.chunks[ci].cmpxchgStrong(null, fresh, .acq_rel, .acquire);
        }
        return self.cellAt(index);
    }

    fn allocCell(self: *Table) ?*Cell {
        const cell = self.popFree() orelse self.freshCell() orelse {
            _ = self.alloc_failures.fetchAdd(1, .monotonic);
            return null;
        };
        return cell;
    }

    /// A cell whose last reference dropped: back to the free list. Its
    /// fields are dead until the next bind re-fills them under refs == 0.
    fn recycle(self: *Table, cell: *Cell) void {
        cell.armed.store(false, .monotonic);
        cell.kernel_latch.store(false, .monotonic);
        self.pushFree(cell);
    }

    fn dropOf(self: *Table, cell: *Cell) ?Drop {
        const last = cell.release();
        const d: Drop = .{ .last = last, .binding = cell.binding };
        if (last) self.recycle(cell);
        return d;
    }

    // ---- the API ----

    pub const Mirror = struct {
        /// The raw `lseek(SEEK_CUR)` seed taken right after BIND.
        seed: u64,
        /// The fork-epoch snapshot taken BEFORE the fd's `open`.
        epoch: u64,
    };

    pub const Bound = struct {
        cell: *Cell,
        /// A stale entry the number carried (released by the caller as
        /// `close` would: unbind on last ref).
        displaced: ?Drop,
    };

    /// Bind `fd` to `binding`; `mirror` arms the offset mirror (null =
    /// unarmed: an unseekable description or a snapshot the fork epoch
    /// already passed). Null = no cell could be allocated.
    pub fn bind(self: *Table, fd: i32, binding: Binding, mirror: ?Mirror) ?Bound {
        const slot = self.slotPtr(fd, true) orelse return null;
        const cell = self.allocCell() orelse return null;
        cell.binding = binding;
        cell.kernel_latch.store(false, .monotonic);
        if (mirror) |m| {
            cell.epoch = m.epoch;
            cell.offset.store(m.seed, .monotonic);
            // A stale snapshot never arms: the fork already demoted this
            // epoch's mirrors.
            cell.armed.store(m.epoch == self.currentEpoch(), .monotonic);
        } else {
            cell.armed.store(false, .monotonic);
            cell.epoch = 0;
        }
        // The Release publish every acquirer pairs with.
        cell.refs.store(1, .release);
        const prev = slot.swap(cell.index + 1, .acq_rel);
        const displaced: ?Drop = if (prev != 0) self.dropOf(self.cellAt(prev - 1)) else null;
        return .{ .cell = cell, .displaced = displaced };
    }

    /// The data-path lookup: segment load, slot load, acquire, re-validate.
    pub fn lookup(self: *Table, fd: i32) ?Pin {
        const slot = self.slotPtr(fd, false) orelse return null;
        const idx1 = slot.load(.acquire);
        if (idx1 == 0) return null;
        const cell = self.cellAt(idx1 - 1);
        if (!cell.acquire()) return null;
        if (slot.load(.acquire) != idx1) {
            // The number was closed (and maybe re-bound) between the loads:
            // whatever we pinned is not this fd's binding any more.
            _ = self.dropOf(cell);
            return null;
        }
        return .{ .cell = cell };
    }

    /// Is `fd` bound? (no pin; the hygiene sweep's empty fast path).
    pub fn isBound(self: *Table, fd: i32) bool {
        const slot = self.slotPtr(fd, false) orelse return false;
        return slot.load(.acquire) != 0;
    }

    /// `close(fd)` / the creator sweep: clear the slot, release the ref.
    pub fn onClose(self: *Table, fd: i32) ?Drop {
        const slot = self.slotPtr(fd, false) orelse return null;
        const prev = slot.swap(0, .acq_rel);
        if (prev == 0) return null;
        return self.dropOf(self.cellAt(prev - 1));
    }

    /// `dup(oldfd) → newfd` (after the real call succeeded): one more
    /// reference on old's cell (refused if a racing last-close won), swapped
    /// into new's slot; the displaced entry is released (`dup2` implicitly
    /// closes `newfd`).
    pub fn onDup(self: *Table, oldfd: i32, newfd: i32) ?Drop {
        if (oldfd == newfd) return null;
        const old_slot = self.slotPtr(oldfd, false) orelse return self.onClose(newfd);
        const idx1 = old_slot.load(.acquire);
        if (idx1 == 0) return self.onClose(newfd);
        const cell = self.cellAt(idx1 - 1);
        if (!cell.acquire()) return self.onClose(newfd);
        const new_slot = self.slotPtr(newfd, true) orelse {
            return self.dropOf(cell);
        };
        const prev = new_slot.swap(idx1, .acq_rel);
        if (prev == 0) return null;
        return self.dropOf(self.cellAt(prev - 1));
    }

    /// `close_range(first, last)` (inclusive): whole unallocated segments
    /// are skipped in O(1).
    pub fn onCloseRange(self: *Table, first: u32, last: u32, ctx: anytype, comptime drop: fn (@TypeOf(ctx), Drop) void) void {
        if (first > last) return;
        var fd: u32 = first;
        const end: u32 = @min(last, MAX_FD - 1);
        while (fd <= end) {
            const si = fd / SEGMENT_FDS;
            if (self.segments[si].load(.acquire) == null) {
                fd = (si + 1) * SEGMENT_FDS;
                continue;
            }
            if (self.onClose(@intCast(fd))) |d| drop(ctx, d);
            fd += 1;
        }
    }

    /// The per-inode walk behind `mmap` on a bound fd: EVERY entry naming
    /// `(dev, ino)` is released; each armed cell is disarmed once and its
    /// offset surfaced for the raw `SEEK_SET` write-through BEFORE the real
    /// call; every last ref surfaces its Drop.
    pub fn unbindIno(self: *Table, dev: u64, ino: u64, ctx: anytype, comptime flush: fn (@TypeOf(ctx), Flush) void, comptime drop: fn (@TypeOf(ctx), Drop) void) void {
        const epoch = self.currentEpoch();
        var si: u32 = 0;
        while (si < SEGMENTS) : (si += 1) {
            const seg = self.segments[si].load(.acquire) orelse continue;
            var i: u32 = 0;
            while (i < SEGMENT_FDS) : (i += 1) {
                const idx1 = seg[i].load(.acquire);
                if (idx1 == 0) continue;
                const cell = self.cellAt(idx1 - 1);
                if (!cell.acquire()) continue;
                const fd: i32 = @intCast(si * SEGMENT_FDS + i);
                if (cell.binding.dev == dev and cell.binding.ino == ino) {
                    if (cell.disarmIfCurrent(epoch)) |off| flush(ctx, .{ .fd = fd, .offset = off });
                    if (seg[i].cmpxchgStrong(idx1, 0, .acq_rel, .acquire) == null) {
                        if (self.dropOf(cell)) |d| drop(ctx, d);
                    }
                }
                if (self.dropOf(cell)) |d| drop(ctx, d);
            }
        }
    }

    /// The fork PREPARE / `posix_spawn` flush: bump the epoch, then flush
    /// every cell armed under the PREVIOUS epoch exactly once (dup
    /// siblings share the cell) under a bounded `tryLock`, so the child
    /// inherits true kernel offsets and the demoted parent resyncs from
    /// them. AS-safe: atomics, bounded spins, the caller's raw `lseek`.
    pub fn forkFlushAll(self: *Table, ctx: anytype, comptime flush: fn (@TypeOf(ctx), Flush) void) void {
        const prev_epoch = self.bumpEpoch() - 1;
        var si: u32 = 0;
        while (si < SEGMENTS) : (si += 1) {
            const seg = self.segments[si].load(.acquire) orelse continue;
            var i: u32 = 0;
            while (i < SEGMENT_FDS) : (i += 1) {
                const idx1 = seg[i].load(.acquire);
                if (idx1 == 0) continue;
                const cell = self.cellAt(idx1 - 1);
                if (!cell.armed.load(.acquire) or cell.epoch != prev_epoch) continue;
                if (!cell.acquire()) continue;
                var spins: u32 = 0;
                var locked = false;
                while (spins < FORK_TRYLOCK_SPINS) : (spins += 1) {
                    if (cell.lock.tryLock()) {
                        locked = true;
                        break;
                    }
                    std.atomic.spinLoopHint();
                }
                if (locked) {
                    // Under the lock no op is mid-serve on this description:
                    // the disarm captures the final offset (an op that
                    // completed meanwhile published before unlocking).
                    if (cell.disarmIfCurrent(prev_epoch)) |off| flush(ctx, .{ .fd = @intCast(si * SEGMENT_FDS + i), .offset = off });
                    cell.lock.unlock();
                }
                _ = self.dropOf(cell);
            }
        }
    }

    /// The fork CHILD: every slot is cleared without ctl traffic (the
    /// parent still uses the bindings; the child's sessions are poisoned
    /// and its stale fds route to passthrough).
    pub fn clearAllInChild(self: *Table) void {
        var si: u32 = 0;
        while (si < SEGMENTS) : (si += 1) {
            const seg = self.segments[si].load(.acquire) orelse continue;
            for (seg[0..SEGMENT_FDS]) |*w| w.store(0, .monotonic);
        }
    }

    /// Bound fds (a census for tests and the stats line).
    pub fn boundCount(self: *Table) u32 {
        var n: u32 = 0;
        var si: u32 = 0;
        while (si < SEGMENTS) : (si += 1) {
            const seg = self.segments[si].load(.acquire) orelse continue;
            for (seg[0..SEGMENT_FDS]) |*w| {
                if (w.load(.monotonic) != 0) n += 1;
            }
        }
        return n;
    }
};

// ---------------------------------------------------------------------------
// The negative device cache (spec §2 step 2 + §11 S1): `st_dev`s that are
// not ours, or whose establish was refused, keyed `dev + 1` (so `st_dev ==
// 0` is legal), 4-way splitmix64 probe, plain overwrite eviction — a lost
// entry costs one re-classification, never correctness. `until` carries
// the S1 backoff: permanent (not FUSE, foreign FUSE, undecodable blob,
// Disabled/Version/PeerUntrusted/Unsealed/UntrustedRendezvous) or a deadline
// (Budget/Socket/Protocol back off from the nonce TTL to 15 min).
// ---------------------------------------------------------------------------

pub const DevCache = struct {
    pub const SLOTS: usize = 1024;
    pub const WAYS: usize = 4;
    pub const PERMANENT: u64 = std.math.maxInt(u64);

    keys: [SLOTS]Atomic(u64) = @splat(.init(0)),
    until: [SLOTS]Atomic(u64) = @splat(.init(0)),
    /// The S1 backoff last applied to the way (0 = none yet).
    backoff: [SLOTS]Atomic(u64) = @splat(.init(0)),

    fn mix(x: u64) u64 {
        var z = x +% 0x9E3779B97F4A7C15;
        z = (z ^ (z >> 30)) *% 0xBF58476D1CE4E5B9;
        z = (z ^ (z >> 27)) *% 0x94D049BB133111EB;
        return z ^ (z >> 31);
    }

    fn probe(dev: u64, way: usize) usize {
        return @intCast(mix(dev +% way) % SLOTS);
    }

    pub fn contains(self: *const DevCache, dev: u64, now_ns: u64) bool {
        return self.verdict(dev, now_ns) != null;
    }

    /// WHY the device is cached: a permanent never-ours verdict, or a
    /// refused establish in its backoff window (the mount IS ours; its
    /// opens are kernel-served by the refusal — the shim's ledger
    /// attributes them `establish_refused`, never `not_ours`).
    pub const Verdict = enum { permanent, backoff };

    pub fn verdict(self: *const DevCache, dev: u64, now_ns: u64) ?Verdict {
        const key = dev + 1;
        var w: usize = 0;
        while (w < WAYS) : (w += 1) {
            const i = probe(dev, w);
            if (self.keys[i].load(.acquire) != key) continue;
            const until = self.until[i].load(.acquire);
            if (now_ns >= until) return null;
            return if (until == PERMANENT) .permanent else .backoff;
        }
        return null;
    }

    /// The way holding `dev` (or the one it will take): the device's own
    /// way, else an empty or expired one, else way 0 (a lost entry is a
    /// re-classification).
    fn wayFor(self: *DevCache, dev: u64, now_ns: u64) struct { index: usize, hit: bool } {
        const key = dev + 1;
        var victim: ?usize = null;
        var w: usize = 0;
        while (w < WAYS) : (w += 1) {
            const i = probe(dev, w);
            const k = self.keys[i].load(.acquire);
            if (k == key) return .{ .index = i, .hit = true };
            if (victim == null and (k == 0 or now_ns >= self.until[i].load(.acquire))) victim = i;
        }
        return .{ .index = victim orelse probe(dev, 0), .hit = false };
    }

    /// Insert or refresh with an explicit deadline (`PERMANENT` for the
    /// never-ours verdicts).
    pub fn insert(self: *DevCache, dev: u64, until_ns: u64, now_ns: u64) void {
        const w = self.wayFor(dev, now_ns);
        if (!w.hit) self.backoff[w.index].store(0, .monotonic);
        self.until[w.index].store(until_ns, .release);
        self.keys[w.index].store(dev + 1, .release);
    }

    /// Insert with the S1 exponential backoff: the first refusal parks the
    /// device for `floor_ns`, every later one doubles up to `cap_ns`.
    /// Returns the backoff applied.
    pub fn insertBackoff(self: *DevCache, dev: u64, now_ns: u64, floor_ns: u64, cap_ns: u64) u64 {
        const w = self.wayFor(dev, now_ns);
        const prev = if (w.hit) self.backoff[w.index].load(.monotonic) else 0;
        const next = if (prev == 0) floor_ns else @min(prev * 2, cap_ns);
        self.backoff[w.index].store(next, .monotonic);
        self.until[w.index].store(now_ns + next, .release);
        self.keys[w.index].store(dev + 1, .release);
        return next;
    }
};

// ---------------------------------------------------------------------------
// The MAP_SHARED inode set (POSIX-7, spec §1.10): an inode mapped shared
// anywhere in this process is never bound again for the process's life.
// 8192 fingerprint words (the 64-bit mix of `(dev, ino)`; 0 reserved),
// never evicts; a lost entry would re-arm the ring under a mapping =
// corruption, so overflow poisons classification entirely.
// ---------------------------------------------------------------------------

pub const MappedInoSet = struct {
    pub const SLOTS: usize = 8192;
    /// A full table within this many probes is the overflow verdict.
    pub const PROBES: usize = 64;

    words: [SLOTS]Atomic(u64) = @splat(.init(0)),
    overflow: Atomic(bool) = .init(false),

    fn fingerprint(dev: u64, ino: u64) u64 {
        const f = DevCache.mix(DevCache.mix(dev) ^ ino);
        return if (f == 0) 1 else f;
    }

    pub fn contains(self: *const MappedInoSet, dev: u64, ino: u64) bool {
        if (self.overflow.load(.acquire)) return true;
        const fp = fingerprint(dev, ino);
        var p: usize = 0;
        while (p < PROBES) : (p += 1) {
            const w = self.words[(fp +% p) % SLOTS].load(.acquire);
            if (w == 0) return false;
            if (w == fp) return true;
        }
        return false;
    }

    pub fn insert(self: *MappedInoSet, dev: u64, ino: u64) void {
        const fp = fingerprint(dev, ino);
        var p: usize = 0;
        while (p < PROBES) : (p += 1) {
            const slot = &self.words[(fp +% p) % SLOTS];
            const w = slot.load(.acquire);
            if (w == fp) return;
            if (w == 0) {
                if (slot.cmpxchgStrong(0, fp, .acq_rel, .acquire)) |raced| {
                    if (raced == fp) return;
                    continue;
                }
                return;
            }
        }
        self.overflow.store(true, .release);
    }
};

// ---------------------------------------------------------------------------
// Tests (portable).
// ---------------------------------------------------------------------------

const testing = std.testing;

fn testMap(len: usize) ?[*]align(4096) u8 {
    const mem = std.heap.page_allocator.alignedAlloc(u8, .fromByteUnits(4096), len) catch return null;
    @memset(mem, 0);
    return mem.ptr;
}

fn testBinding(id: u64, ino: u64) Binding {
    return .{ .session = 0, .id = id, .dev = 7, .ino = ino, .rights = .{ .read = true, .write = true } };
}

const Collector = struct {
    drops: [16]Drop = undefined,
    n_drops: usize = 0,
    flushes: [16]Flush = undefined,
    n_flushes: usize = 0,

    fn drop(self: *Collector, d: Drop) void {
        self.drops[self.n_drops] = d;
        self.n_drops += 1;
    }
    fn flush(self: *Collector, f: Flush) void {
        self.flushes[self.n_flushes] = f;
        self.n_flushes += 1;
    }
    fn lastDrops(self: *const Collector) usize {
        var n: usize = 0;
        for (self.drops[0..self.n_drops]) |d| {
            if (d.last) n += 1;
        }
        return n;
    }
};

test "bind → lookup → close round trip; a negative fd and an unbound fd answer null; the table grows to large fds" {
    const t = try testing.allocator.create(Table);
    defer testing.allocator.destroy(t);
    t.* = .init(testMap);
    try testing.expect(t.lookup(-1) == null);
    try testing.expect(t.lookup(5) == null);
    const b = t.bind(5, testBinding(11, 100), null).?;
    try testing.expect(b.displaced == null);
    const p = t.lookup(5).?;
    try testing.expectEqual(@as(u64, 11), p.cell.binding.id);
    try testing.expectEqual(@as(u32, 2), p.cell.refs.load(.monotonic));
    try testing.expect(p.unpin() == null);
    const d = t.onClose(5).?;
    try testing.expect(d.last);
    try testing.expectEqual(@as(u64, 11), d.binding.id);
    try testing.expect(t.lookup(5) == null);
    try testing.expect(t.onClose(5) == null);
    // A large fd lands in a fresh segment.
    const big = t.bind(2_000_000, testBinding(12, 101), null).?;
    try testing.expect(big.displaced == null);
    try testing.expectEqual(@as(u32, 1), t.boundCount());
    try testing.expect(t.onClose(2_000_000).?.last);
    try testing.expect(t.bind(@intCast(MAX_FD), testBinding(1, 1), null) == null);
}

test "dup propagates the binding; closing the original keeps the dup bound and the unbind is reported exactly once, on the LAST close" {
    const t = try testing.allocator.create(Table);
    defer testing.allocator.destroy(t);
    t.* = .init(testMap);
    _ = t.bind(3, testBinding(1, 100), null).?;
    try testing.expect(t.onDup(3, 9) == null);
    const p3 = t.lookup(3).?;
    const p9 = t.lookup(9).?;
    try testing.expectEqual(p3.cell, p9.cell);
    _ = p3.unpin();
    _ = p9.unpin();
    try testing.expect(!t.onClose(3).?.last);
    const still = t.lookup(9) orelse return error.TestUnexpectedResult;
    try testing.expect(still.unpin() == null);
    try testing.expect(t.onClose(9).?.last);
    // dup of an unbound fd over a bound one releases the clobbered binding.
    _ = t.bind(4, testBinding(2, 101), null).?;
    const d = t.onDup(7, 4).?;
    try testing.expect(d.last and d.binding.id == 2);
    try testing.expect(t.lookup(4) == null);
}

test "dup2 over a live binding releases the clobbered one; a bind over a stale entry releases it first" {
    const t = try testing.allocator.create(Table);
    defer testing.allocator.destroy(t);
    t.* = .init(testMap);
    _ = t.bind(3, testBinding(1, 100), null).?;
    _ = t.bind(4, testBinding(2, 101), null).?;
    const d = t.onDup(3, 4).?;
    try testing.expect(d.last and d.binding.id == 2);
    const p = t.lookup(4).?;
    try testing.expectEqual(@as(u64, 1), p.cell.binding.id);
    _ = p.unpin();
    // The number 4 was raw-closed and reused: the next bind displaces.
    const b = t.bind(4, testBinding(3, 102), null).?;
    try testing.expect(!b.displaced.?.last);
    try testing.expect(t.onClose(3).?.last);
    try testing.expect(t.onClose(4).?.last);
}

test "close_range sweeps the inclusive range and skips unallocated segments; the hygiene sweep composes with refcounts (bind → dup → raw-close one → creator reuses the number → close the dup ⇒ exactly one unbind)" {
    const t = try testing.allocator.create(Table);
    defer testing.allocator.destroy(t);
    t.* = .init(testMap);
    _ = t.bind(3, testBinding(1, 100), null).?;
    _ = t.bind(5, testBinding(2, 101), null).?;
    _ = t.bind(1500, testBinding(3, 102), null).?;
    var c: Collector = .{};
    t.onCloseRange(4, std.math.maxInt(u32), &c, Collector.drop);
    try testing.expectEqual(@as(usize, 2), c.n_drops);
    try testing.expectEqual(@as(usize, 2), c.lastDrops());
    const p3 = t.lookup(3) orelse return error.TestUnexpectedResult;
    try testing.expect(p3.unpin() == null);
    try testing.expectEqual(@as(u32, 1), t.boundCount());
    // The composition row.
    _ = t.onDup(3, 10);
    // fd 3 raw-closed (no bookkeeping), the number comes back from socket():
    // the sweep releases ONE ref, not the binding.
    const sweep = t.onClose(3).?;
    try testing.expect(!sweep.last);
    try testing.expect(t.onClose(10).?.last);
    try testing.expectEqual(@as(u32, 0), t.boundCount());
}

test "cells recycle through the free list: a closed cell's index is reused by the next bind, and a pin taken across the recycle refuses the recycled cell (slot re-validation)" {
    const t = try testing.allocator.create(Table);
    defer testing.allocator.destroy(t);
    t.* = .init(testMap);
    const a = t.bind(3, testBinding(1, 100), null).?;
    const ai = a.cell.index;
    try testing.expect(t.onClose(3).?.last);
    const b = t.bind(8, testBinding(2, 200), null).?;
    try testing.expectEqual(ai, b.cell.index);
    try testing.expectEqual(@as(u64, 2), b.cell.binding.id);
    // A lookup that loaded slot 3's old index after a close + recycle for
    // another fd: the re-validation refuses it.
    const stale_cell = t.cellAt(ai);
    try testing.expect(stale_cell.acquire());
    try testing.expect(t.lookup(3) == null);
    try testing.expect(!stale_cell.release());
    try testing.expect(t.onClose(8).?.last);
}

test "the offset mirror: plain bind is unarmed; a seeded bind arms and tracks; a stale epoch never arms; dup siblings share one mirror; publish after demote reports the write-through; disarm captures the latest offset exactly once; a never-armed cell never flushes" {
    const t = try testing.allocator.create(Table);
    defer testing.allocator.destroy(t);
    t.* = .init(testMap);
    const plain = t.bind(3, testBinding(1, 100), null).?;
    try testing.expect(!plain.cell.authoritative(t.currentEpoch()));
    try testing.expect(plain.cell.disarmIfCurrent(t.currentEpoch()) == null);
    const e = t.currentEpoch();
    const armed = t.bind(4, testBinding(2, 101), .{ .seed = 4096, .epoch = e }).?;
    try testing.expect(armed.cell.authoritative(e));
    try testing.expectEqual(@as(u64, 4096), armed.cell.offset.load(.monotonic));
    try testing.expect(armed.cell.publish(8192));
    _ = t.onDup(4, 12);
    const sib = t.lookup(12).?;
    try testing.expectEqual(@as(u64, 8192), sib.cell.offset.load(.monotonic));
    _ = sib.unpin();
    try testing.expectEqual(@as(?u64, 8192), armed.cell.disarmIfCurrent(e));
    try testing.expect(armed.cell.disarmIfCurrent(e) == null);
    try testing.expect(!armed.cell.publish(9000));
    // A stale snapshot: the epoch moved between the snapshot and the bind.
    const stale = t.bind(5, testBinding(3, 102), .{ .seed = 0, .epoch = e -% 1 }).?;
    try testing.expect(!stale.cell.authoritative(t.currentEpoch()));
    _ = t.bumpEpoch();
    const fresh = t.bind(6, testBinding(4, 103), .{ .seed = 0, .epoch = e }).?;
    try testing.expect(!fresh.cell.authoritative(t.currentEpoch()));
}

test "lseek arithmetic over the mirror matches vfs_setpos: SEEK_SET/SEEK_CUR, negative and wrapped results refuse; other whences are the kernel's" {
    var cell: Cell = .{};
    cell.offset.store(100, .monotonic);
    try testing.expectEqual(@as(?u64, 50), cell.seekArith(0, 50));
    try testing.expectEqual(@as(?u64, 130), cell.seekArith(1, 30));
    try testing.expectEqual(@as(?u64, 0), cell.seekArith(1, -100));
    try testing.expect(cell.seekArith(1, -101) == null);
    try testing.expect(cell.seekArith(0, -1) == null);
    try testing.expect(cell.seekArith(1, std.math.maxInt(i64)) == null);
    try testing.expect(cell.seekArith(2, 0) == null);
}

test "fork flush: the epoch bumps, each cell armed under the previous epoch flushes exactly once (dup siblings once), a never-armed or already-demoted cell never rewinds, a later publish reports write-through" {
    const t = try testing.allocator.create(Table);
    defer testing.allocator.destroy(t);
    t.* = .init(testMap);
    const e = t.currentEpoch();
    const a = t.bind(3, testBinding(1, 100), .{ .seed = 10, .epoch = e }).?;
    _ = t.onDup(3, 4);
    _ = t.bind(5, testBinding(2, 101), null).?;
    const c = t.bind(6, testBinding(3, 102), .{ .seed = 30, .epoch = e }).?;
    try testing.expectEqual(@as(?u64, 30), c.cell.disarmIfCurrent(e));
    var col: Collector = .{};
    t.forkFlushAll(&col, Collector.flush);
    try testing.expectEqual(@as(usize, 1), col.n_flushes);
    try testing.expectEqual(@as(i32, 3), col.flushes[0].fd);
    try testing.expectEqual(@as(u64, 10), col.flushes[0].offset);
    try testing.expectEqual(e + 1, t.currentEpoch());
    try testing.expect(!a.cell.authoritative(t.currentEpoch()));
    try testing.expect(!a.cell.publish(99));
    // A second fork flushes nothing: nothing is armed under the new epoch.
    var col2: Collector = .{};
    t.forkFlushAll(&col2, Collector.flush);
    try testing.expectEqual(@as(usize, 0), col2.n_flushes);
    // A locked cell (an op mid-serve) is skipped after the bounded spins.
    const d = t.bind(7, testBinding(4, 103), .{ .seed = 70, .epoch = t.currentEpoch() }).?;
    d.cell.lock.lock();
    var col3: Collector = .{};
    t.forkFlushAll(&col3, Collector.flush);
    try testing.expectEqual(@as(usize, 0), col3.n_flushes);
    d.cell.lock.unlock();
}

test "unbind_ino releases every sibling of the inode in the process, flushes each armed cell once, and leaves other inodes bound; the child clear drops every slot silently" {
    const t = try testing.allocator.create(Table);
    defer testing.allocator.destroy(t);
    t.* = .init(testMap);
    const e = t.currentEpoch();
    _ = t.bind(3, testBinding(1, 100), .{ .seed = 1, .epoch = e }).?;
    _ = t.onDup(3, 4);
    _ = t.bind(5, testBinding(2, 100), .{ .seed = 2, .epoch = e }).?;
    _ = t.bind(6, testBinding(3, 200), .{ .seed = 3, .epoch = e }).?;
    var col: Collector = .{};
    t.unbindIno(7, 100, &col, Collector.flush, Collector.drop);
    try testing.expectEqual(@as(usize, 2), col.n_flushes);
    try testing.expectEqual(@as(usize, 2), col.lastDrops());
    try testing.expect(t.lookup(3) == null);
    try testing.expect(t.lookup(4) == null);
    try testing.expect(t.lookup(5) == null);
    const p6 = t.lookup(6) orelse return error.TestUnexpectedResult;
    try testing.expect(p6.unpin() == null);
    t.clearAllInChild();
    try testing.expectEqual(@as(u32, 0), t.boundCount());
}

test "concurrent dup/close stress: 8 threads × 2000 rounds on one cell unbind exactly once per bind" {
    const t = try testing.allocator.create(Table);
    defer testing.allocator.destroy(t);
    t.* = .init(testMap);
    const Worker = struct {
        fn run(table: *Table, base: i32, lasts: *Atomic(u64)) void {
            var r: u32 = 0;
            while (r < 2000) : (r += 1) {
                // Each worker dups the shared fd 3 into its own number and closes it.
                _ = table.onDup(3, base);
                if (table.onClose(base)) |d| {
                    if (d.last) _ = lasts.fetchAdd(1, .monotonic);
                }
                if (table.lookup(3)) |p| {
                    if (p.unpin()) |d| {
                        if (d.last) _ = lasts.fetchAdd(1, .monotonic);
                    }
                }
            }
        }
    };
    var lasts: Atomic(u64) = .init(0);
    _ = t.bind(3, testBinding(1, 100), null).?;
    var threads: [8]std.Thread = undefined;
    for (&threads, 0..) |*th, i| th.* = try std.Thread.spawn(.{}, Worker.run, .{ t, @as(i32, @intCast(100 + i)), &lasts });
    for (threads) |th| th.join();
    try testing.expectEqual(@as(u64, 0), lasts.load(.monotonic));
    try testing.expect(t.onClose(3).?.last);
    try testing.expectEqual(@as(u32, 0), t.boundCount());
}

test "the negative device cache remembers, never false-positives, expires a deadline and evicts gracefully under a full way set" {
    const c = try testing.allocator.create(DevCache);
    defer testing.allocator.destroy(c);
    c.* = .{};
    try testing.expect(!c.contains(0, 0));
    c.insert(0, DevCache.PERMANENT, 0);
    try testing.expect(c.contains(0, 0));
    try testing.expect(!c.contains(1, 0));
    c.insert(42, 1000, 0);
    try testing.expect(c.contains(42, 999));
    try testing.expect(!c.contains(42, 1000));
    // A refresh extends.
    c.insert(42, 5000, 1000);
    try testing.expect(c.contains(42, 4999));
    // The backoff doubles from the floor to the cap and resets on a fresh entry.
    try testing.expectEqual(@as(u64, 60), c.insertBackoff(77, 0, 60, 900));
    try testing.expect(c.contains(77, 59));
    try testing.expect(!c.contains(77, 60));
    try testing.expectEqual(@as(u64, 120), c.insertBackoff(77, 60, 60, 900));
    try testing.expectEqual(@as(u64, 240), c.insertBackoff(77, 180, 60, 900));
    try testing.expectEqual(@as(u64, 480), c.insertBackoff(77, 420, 60, 900));
    try testing.expectEqual(@as(u64, 900), c.insertBackoff(77, 900, 60, 900));
    try testing.expectEqual(@as(u64, 900), c.insertBackoff(77, 1800, 60, 900));
    c.insert(77, DevCache.PERMANENT, 0);
    try testing.expectEqual(@as(u64, 60), c.insertBackoff(78, 0, 60, 900));
    // Fill far past capacity: no crash, every lookup answers consistently
    // (an evicted entry reads as absent, never as a different device).
    var d: u64 = 100;
    while (d < 100 + 8 * DevCache.SLOTS) : (d += 1) c.insert(d, DevCache.PERMANENT, 0);
    try testing.expect(c.contains(100 + 8 * DevCache.SLOTS - 1, 0));
    var false_pos: usize = 0;
    var x: u64 = 1 << 40;
    while (x < (1 << 40) + 10_000) : (x += 1) {
        if (c.contains(x, 0)) false_pos += 1;
    }
    try testing.expectEqual(@as(usize, 0), false_pos);
}

test "the MAP_SHARED inode set never forgets, distinguishes (dev, ino) pairs, and overflow poisons everything" {
    const s = try testing.allocator.create(MappedInoSet);
    defer testing.allocator.destroy(s);
    s.* = .{};
    try testing.expect(!s.contains(1, 2));
    s.insert(1, 2);
    try testing.expect(s.contains(1, 2));
    try testing.expect(!s.contains(2, 1));
    try testing.expect(!s.contains(1, 3));
    var i: u64 = 0;
    while (i < MappedInoSet.SLOTS + 1) : (i += 1) s.insert(9, 1000 + i);
    try testing.expect(s.overflow.load(.monotonic));
    try testing.expect(s.contains(123, 456));
}
