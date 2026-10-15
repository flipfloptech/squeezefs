//! Open count, kernel lookup references and `open_count_stranded`
//! (`docs/porting/fuse_handlers_lanes.md` §8; `fuse_abi_session.md` §7;
//! charter §4 Teardown: "`open_count_stranded` (a final FORGET with open
//! handles counted = a lost RELEASE)").
//!
//! One entry per ino the kernel knows: `{count, lookups, dirty}`.
//!
//! - `addOpen` (OPEN / OPENDIR / CREATE): `count 0 → 1` clears `dirty` — a
//!   fresh open generation starts clean (the `fuse_release_clean_fastpath`
//!   elision keys on it). `removeOpen` (RELEASE / RELEASEDIR) saturates at
//!   0. The open/reclaim handshake is count-FIRST: open counts, then
//!   refuses if a reclaim already claimed the slot; the reclaim side claims
//!   then re-checks the count — every interleaving safe.
//! - `noteLookup` records ONE kernel reference per entry reply sent —
//!   LOOKUP, CREATE, MKNOD, MKDIR, SYMLINK, LINK, each READDIRPLUS entry;
//!   the root (`ino ≤ 1`) is never counted (the kernel never forgets it).
//! - `returnLookups(ino, nlookup)`: `n = max(nlookup, 1)` ("the kernel never
//!   sends 0; treat it as one so a malformed forget can never make an inode
//!   unevictable"); `lookups −= n` saturating; EVICT iff it reached 0 — an
//!   UNTRACKED inode evicts on its first forget (a miscount can only cost
//!   cache retention, never correctness). **POSIX-14**: reaching zero is the
//!   kernel certifying no `struct file` is open (it cannot evict an inode a
//!   file holds), so `count > 0` here is a LOST RELEASE: the tripwire
//!   `open_count_stranded` fires, the count is NOT zeroed ("an unlinked-open
//!   file's live data must never be destroyed on the strength of an
//!   accounting we already know is wrong" — generic/795) and the entry
//!   stays until `count == 0` too. Zig has no unwinding, so a lost RELEASE
//!   can only be a logic error: the tripwire must read 0 on every teardown.
//!
//! **The no-open posture** (`no_open`, decision 0003 §AF): the daemon
//! answers OPEN with ENOSYS, the kernel opens and releases regular files
//! locally and sends no RELEASE — for a CREATE-opened file either, once the
//! connection latched. Only OPENDIR counts then (CREATE counts nothing), so
//! `open_count_stranded` means a lost RELEASEDIR; a regular file's custody
//! needs no count at all — the last FORGET (`fuse_evict_inode`, the only
//! FORGET of a live inode) comes after every `struct file` let go of the
//! inode, which is the certificate the corpse destroy already keys on.
//! What RELEASE carried — the write-back drain at the end of a file's use
//! — rides the FLUSH of every close(2), and the table keeps what that
//! cannot see: the OWED set, the inos holding a write no FLUSH has covered
//! (`noteWrite`; a FLUSH removes the writes noted before it began —
//! `flushBegin` / `flushEnd`, ordered by the stripe's sequence under its
//! lock). A held-open writer's custody, a mapping written back after its
//! fd closed (`fuse_vma_close` at munmap), an async write landing after
//! its close: each stays owed until a FLUSH covers it or the reclaim
//! cadence drains it once idle (`takeIdleOwed`). A close burst coalesces:
//! a FLUSH on an ino a FLUSH drained within the window (`recentlyDrained`,
//! stamped by `markDrained`) only notes it owed with `deferred`
//! (`deferDrain`), which the cadence's next tick drains whatever its age.
//!
//! **The write-back timer** (`Posture.timer`, decision 0003 §AJ — the
//! owner's ruling: close is no durability point): every close is local
//! (the daemon latches the kernel's `no_flush`; a CREATE answers
//! `FOPEN_NOFLUSH`), and the owed set is AGE-ORDERED instead — each owed
//! ino carries the birth of its oldest custody (`Owed.born_ns`, the first
//! note since it last held none) in a per-stripe min-heap, and ONE timer
//! pass per flush cadence (`timerPass`) takes every ino born at or before
//! `now − age`, lands its custody born by then and re-owes the ino at the
//! birth of the custody still held. An acknowledged unsynced write leaves
//! daemon memory within `T_wb = age + period + the pass's drain` (the
//! derivations below). FLUSH, the idle harvest and the burst coalescing are
//! the `close` posture's alone.
//!
//! Striped under spin locks (the metadata-transaction class; O(1)
//! sections) and pre-sized once from the transport geometry; the kernel's
//! inode cache can outgrow any fixed bound, so growth past the pre-size is
//! one allocation per doubling of a stripe, never per op (the
//! `economy.PendingTimes` posture).

const std = @import("std");
const assert = std.debug.assert;
const counters = @import("../../core/counters.zig");
const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const jc = @import("../../meta/journal_core.zig");
const checkpoint = @import("../../meta/checkpoint.zig");

/// Where an acknowledged write's custody becomes durable when no fsync,
/// syncfs or unmount asks first (`SQUEEZEFS_WRITEBACK_POSTURE`, decision
/// 0003 §AJ): `timer` — within `T_wb` of its write, closes local; `close`
/// — at the close that tells the daemon (the no-open FLUSH, the control
/// arm's RELEASE), the A/B control arm.
pub const Posture = enum {
    timer,
    close,

    /// The registry row's value (the knob's ONE reader passes it).
    pub fn ofKnobWord(word: []const u8) Posture {
        return if (std.mem.eql(u8, word, "timer")) .timer else .close;
    }
};

/// The timer's period: the flush cadence (`checkpoint.checkpointTickPeriodMs`
/// — the coalesced barrier's interval, 50 ms derived; the strict cadence's
/// 100 ms task tick). The pass is O(1) per stripe when nothing is owed, so
/// it rides the same rate as the barrier it hands its publishes to.
pub fn timerPeriodNs(flush_interval_ms: u64) u64 {
    return checkpoint.checkpointTickPeriodMs(flush_interval_ms) * std.time.ns_per_ms;
}

/// The age a custody must reach before the timer lands it: the checkpoint's
/// cadence ceiling (`SQUEEZEFS_CHECKPOINT_MAX_AGE_MS`, 1 s — the flush
/// cadence is its twentieth), so with the pass's period and one period of
/// drain an acked write lands within the ceiling a dirty metadata leaf
/// lands within (`max_age + 2 × tick`). A block filled within the age
/// writes itself through (`union == whole block`) and never lands partial:
/// a streamer at ≥ block ÷ age (4 MB/s at 4 MiB) pays the timer nothing.
/// Measured: an age of one period (50 ms) landed IOR's in-progress blocks
/// partial under host load — 1.27 device bytes per user byte against the
/// close posture's 1.00 (`rung-10-writeback-timer.md`).
pub fn timerAgeNs(max_age_ms: u64) u64 {
    return max_age_ms * std.time.ns_per_ms;
}

/// `T_wb`'s static part: a custody born just after a pass's cutoff waits
/// `age` to come of age and up to one `period` for the next pass; the
/// pass's own drain latency adds to it (`writeback_residency_ns` measures
/// the whole).
pub fn boundStaticNs(age_ns: u64, period_ns: u64) u64 {
    return age_ns + period_ns;
}

/// `open_count_stranded` (must stay 0).
pub var open_count_stranded: counters.Tripwire = .init("open_count_stranded");
/// `fuse_open_inodes`: inos with `count > 0` right now.
pub var fuse_open_inodes: counters.Gauge = .{};
/// `fuse_tracked_inodes`: entries in the table (kernel-known inos).
pub var fuse_tracked_inodes: counters.Gauge = .{};
/// `fuse_owed_drain_inos`: no-open inos holding a write no FLUSH covered.
pub var fuse_owed_drain_inos: counters.Gauge = .{};
/// `fuse_owed_idle_drains`: owed inos the reclaim cadence drained once idle
/// (custody no close covered: a held-open writer, a mapping or an async
/// write outliving its fd).
pub var fuse_owed_idle_drains: counters.ShardedCounter = .{};
/// `writeback_timer_passes`: timer passes that found an aged ino.
pub var writeback_timer_passes: counters.ShardedCounter = .{};
/// `writeback_timer_drains`: aged inos the timer drained.
pub var writeback_timer_drains: counters.ShardedCounter = .{};
/// `writeback_timer_kept`: drained inos still holding custody younger than
/// the cutoff — re-owed at its birth, landed by a later pass.
pub var writeback_timer_kept: counters.ShardedCounter = .{};
/// `writeback_timer_reowe_overflows`: a kept ino the owed set could not
/// re-note (it could not grow) — its custody waits for its next write's
/// note, an fsync, a syncfs or the unmount (logged).
pub var writeback_timer_reowe_overflows: counters.ShardedCounter = .{};
/// `writeback_residency_ns`: per drained ino, its oldest custody's birth →
/// the drain's end — the measured `T_wb` (exact-sum).
pub var writeback_residency_ns: counters.ShardedLatencyHistogram = .{};
/// `writeback_residency_max_ns`: the longest residency measured.
pub var writeback_residency_max_ns: counters.Gauge = .{};
/// `writeback_bound_ns`: `T_wb`'s static part (`boundStaticNs`) the mount
/// derived — 0 under the `close` posture.
pub var writeback_bound_ns: counters.Gauge = .{};
/// `writeback_age_ns`: the age a custody reaches before the timer lands it
/// (`timerAgeNs`, or the seam) — 0 under the `close` posture.
pub var writeback_age_ns: counters.Gauge = .{};

pub const Entry = struct {
    count: u32 = 0,
    lookups: u64 = 0,
    dirty: bool = false,
    /// No-open: the monotonic stamp of the last FLUSH that drained this
    /// ino's custody (the burst coalescing's clock; 0 = never).
    drained_ns: u64 = 0,
};

/// 16 stripes: an inode-cache storm (FORGET sweeps, lookup storms) spreads
/// its O(1) sections over 16 lines.
const stripe_count = 16;

/// The pre-size: four entries per delivered slot, floor 4096 — a steady
/// state holds a handful of inos per in-flight op and the floor is one
/// typical working set.
pub fn derivedCapacity(nqueues: u32, q_depth: u32) u32 {
    return @max(4096, 4 * nqueues * q_depth);
}

pub const Verdict = struct {
    /// The last reference went: the caller invalidates caches, sweeps the
    /// per-inode side structures and queues the reclaim (iff not open).
    evicted: bool,
    /// The count found open at eviction (0 healthy).
    stranded_count: u32,
};

/// A write no FLUSH has covered yet (the no-open posture): the stripe
/// sequence of its note (a FLUSH begun at `s` covers every note `≤ s`), its
/// monotonic stamp (the idle drain's clock), and `deferred`: a close whose
/// drain was coalesced into the next reclaim tick (`deferDrain`) — harvested
/// at that tick whatever its idle age. `born_ns`: the birth of the ino's
/// oldest custody (the timer posture's age; its heap entry carries it).
pub const Owed = struct { seq: u64, last_ns: u64, deferred: bool = false, born_ns: u64 = 0 };

/// One age-ordered heap entry (the timer posture): an ino and the birth it
/// was owed at. Stale once its ino's `Owed.born_ns` differs (re-owed
/// younger-first, or harvested and noted again) — skipped at the pop.
pub const Aged = struct { ino: u64, born_ns: u64 };

fn bornFirst(_: void, a: Aged, b: Aged) std.math.Order {
    return std.math.order(a.born_ns, b.born_ns);
}

const AgeHeap = std.PriorityQueue(Aged, void, bornFirst);

/// What the timer's drain of one ino did: `landed` — it held custody born
/// by the cutoff and landed it; `kept_born_ns` — the birth of the oldest
/// custody it still holds (null: none).
pub const Drained = struct { landed: bool, kept_born_ns: ?u64 };

/// The timer pass's drain face (the filesystem's `drainAged`).
pub const DrainAged = struct {
    ctx: *anyopaque,
    call: *const fn (ctx: *anyopaque, ino: u64, cutoff_ns: u64) Drained,
};

pub const OpenTable = struct {
    gpa: std.mem.Allocator,
    stripes: [stripe_count]Stripe,
    /// The no-open posture (module doc), set once at mount before the
    /// session serves its first request.
    no_open: bool = false,
    /// The write-back posture (module doc), set with `no_open`.
    posture: Posture = .close,

    const Stripe = struct {
        lock: jc.SpinLock = .{},
        map: std.AutoHashMapUnmanaged(u64, Entry) = .empty,
        /// No-open: ino → its latest uncovered write. Independent of `map`:
        /// an evicted ino's custody is still owed its drain.
        owed: std.AutoHashMapUnmanaged(u64, Owed) = .empty,
        /// The timer posture's age order over `owed`: one live entry per
        /// owed ino (its `born_ns`), plus the stale ones a re-owe left.
        aged: AgeHeap = .empty,
        /// Bumped by every `noteWrite` under the lock: the order a FLUSH's
        /// begin and a write's note are judged by (both sections hold the
        /// stripe lock, so the order is total).
        seq: u64 = 0,
    };

    pub fn init(gpa: std.mem.Allocator, capacity: u32) std.mem.Allocator.Error!OpenTable {
        var self: OpenTable = .{ .gpa = gpa, .stripes = @splat(.{}) };
        errdefer self.deinit();
        for (&self.stripes) |*s| {
            try s.map.ensureTotalCapacity(gpa, capacity / stripe_count + 1);
            // A handful of files written per in-flight op: the same pre-size.
            try s.owed.ensureTotalCapacity(gpa, capacity / stripe_count + 1);
            try s.aged.ensureTotalCapacity(gpa, capacity / stripe_count + 1);
        }
        return self;
    }

    /// Whether WRITEs note their ino owed: the no-open close posture (a
    /// FLUSH covers the notes) and the timer posture in either open arm.
    fn tracksOwed(self: *const OpenTable) bool {
        return self.no_open or self.posture == .timer;
    }

    pub fn deinit(self: *OpenTable) void {
        var tracked: u64 = 0;
        var open: u64 = 0;
        var owed: u64 = 0;
        for (&self.stripes) |*s| {
            var it = s.map.valueIterator();
            while (it.next()) |e| {
                tracked += 1;
                if (e.count > 0) open += 1;
            }
            owed += s.owed.count();
            s.map.deinit(self.gpa);
            s.owed.deinit(self.gpa);
            s.aged.deinit(self.gpa);
        }
        fuse_tracked_inodes.sub(tracked);
        fuse_open_inodes.sub(open);
        fuse_owed_drain_inos.sub(owed);
        self.* = undefined;
    }

    /// A WRITE landed on `ino` (after the filesystem recorded its custody).
    /// The control arm marks the open generation dirty (the release path's
    /// clean-handle elision keys on it); the no-open posture notes the ino
    /// OWED a drain — the note follows the custody, so a FLUSH whose
    /// custody check missed this write began after it and leaves it owed.
    /// The timer posture notes it in either open arm; its first note since
    /// the ino last held nothing is the custody's birth (the heap's key).
    /// `error.OutOfMemory`: the owed set could not grow; the caller drains.
    pub fn noteWrite(self: *OpenTable, ino: u64, now_ns: u64) std.mem.Allocator.Error!void {
        if (!self.no_open) self.markDirty(ino);
        if (!self.tracksOwed()) return;
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const gop = try s.owed.getOrPut(self.gpa, ino);
        const deferred = gop.found_existing and gop.value_ptr.deferred;
        const born_ns = if (gop.found_existing) gop.value_ptr.born_ns else now_ns;
        if (!gop.found_existing and self.posture == .timer) {
            s.aged.push(self.gpa, .{ .ino = ino, .born_ns = born_ns }) catch |e| {
                s.owed.removeByPtr(gop.key_ptr);
                return e;
            };
        }
        if (!gop.found_existing) fuse_owed_drain_inos.add(1);
        s.seq += 1;
        gop.value_ptr.* = .{ .seq = s.seq, .last_ns = now_ns, .deferred = deferred, .born_ns = born_ns };
    }

    /// The timer's harvest: every owed ino whose custody was born at or
    /// before `cutoff_ns`, popped in birth order per stripe and removed from
    /// the set, up to `out.len` (the rest wait for the next call). A write
    /// after the harvest notes the ino again (a new birth); the caller
    /// drains each one harvested and re-owes what it kept (`reowe`).
    pub fn takeAgedOwed(self: *OpenTable, cutoff_ns: u64, out: []Aged) []Aged {
        var n: usize = 0;
        for (&self.stripes) |*s| {
            if (n == out.len) break;
            s.lock.lock();
            defer s.lock.unlock();
            while (n < out.len) {
                const top = s.aged.peek() orelse break;
                if (top.born_ns > cutoff_ns) break;
                _ = s.aged.pop();
                const o = s.owed.get(top.ino) orelse continue;
                if (o.born_ns != top.born_ns) continue;
                _ = s.owed.remove(top.ino);
                fuse_owed_drain_inos.sub(1);
                out[n] = top;
                n += 1;
            }
        }
        return out[0..n];
    }

    /// A drained ino still holds custody born at `born_ns` (younger than
    /// the pass's cutoff, or a drain that failed): owed again at that
    /// birth — the older of it and a note a write made since the harvest.
    pub fn reowe(self: *OpenTable, ino: u64, born_ns: u64) std.mem.Allocator.Error!void {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const gop = try s.owed.getOrPut(self.gpa, ino);
        if (gop.found_existing and gop.value_ptr.born_ns <= born_ns) return;
        s.aged.push(self.gpa, .{ .ino = ino, .born_ns = born_ns }) catch |e| {
            // A new note rolls back; an existing one keeps its own entry.
            if (!gop.found_existing) s.owed.removeByPtr(gop.key_ptr);
            return e;
        };
        if (gop.found_existing) {
            gop.value_ptr.born_ns = born_ns;
            return;
        }
        fuse_owed_drain_inos.add(1);
        s.seq += 1;
        gop.value_ptr.* = .{ .seq = s.seq, .last_ns = born_ns, .born_ns = born_ns };
    }

    /// A close of `ino` whose drain is coalesced (a FLUSH-drain of the ino
    /// is younger than the coalescing window — a burst of closes): the ino
    /// is owed with `deferred`, so the reclaim cadence drains it at its next
    /// tick; the FLUSH replies without a drain.
    pub fn deferDrain(self: *OpenTable, ino: u64, now_ns: u64) std.mem.Allocator.Error!void {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const gop = try s.owed.getOrPut(self.gpa, ino);
        if (!gop.found_existing) fuse_owed_drain_inos.add(1);
        s.seq += 1;
        gop.value_ptr.* = .{ .seq = s.seq, .last_ns = now_ns, .deferred = true };
    }

    /// Whether a FLUSH drained `ino`'s custody less than `window_ns` ago.
    pub fn recentlyDrained(self: *OpenTable, ino: u64, now_ns: u64, window_ns: u64) bool {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const e = s.map.get(ino) orelse return false;
        return e.drained_ns != 0 and now_ns -| e.drained_ns < window_ns;
    }

    /// A FLUSH drained `ino`'s custody at `now_ns` (the coalescing clock).
    pub fn markDrained(self: *OpenTable, ino: u64, now_ns: u64) void {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        if (s.map.getPtr(ino)) |e| e.drained_ns = now_ns;
    }

    /// A write that leaves no custody behind (the direct-drive lane's W1
    /// patch, durable at its completion): the control arm's dirty mark;
    /// nothing is owed a drain.
    pub fn noteDurableWrite(self: *OpenTable, ino: u64) void {
        if (!self.no_open) self.markDirty(ino);
    }

    /// A FLUSH on `ino` begins (no-open): the stripe's sequence BEFORE its
    /// custody check — every write noted at or below it recorded its
    /// custody before the check runs.
    pub fn flushBegin(self: *OpenTable, ino: u64) u64 {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        return s.seq;
    }

    /// The FLUSH begun at `begun` drained (or found nothing): the owed note
    /// it covers goes; a write noted after it began stays owed.
    pub fn flushEnd(self: *OpenTable, ino: u64, begun: u64) void {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const o = s.owed.get(ino) orelse return;
        if (o.seq > begun) return;
        _ = s.owed.remove(ino);
        fuse_owed_drain_inos.sub(1);
    }

    /// The idle drain's harvest: every owed ino whose latest write is at
    /// least `idle_ns` old — or whose close was deferred — removed from the
    /// set, up to `out.len` (the rest wait for the next call). A write after
    /// the harvest notes the ino again; the caller drains each one harvested.
    pub fn takeIdleOwed(self: *OpenTable, now_ns: u64, idle_ns: u64, out: []u64) []u64 {
        var n: usize = 0;
        for (&self.stripes) |*s| {
            if (n == out.len) break;
            s.lock.lock();
            defer s.lock.unlock();
            const start = n;
            var it = s.owed.iterator();
            while (n < out.len) {
                const kv = it.next() orelse break;
                if (!kv.value_ptr.deferred and now_ns -| kv.value_ptr.last_ns < idle_ns) continue;
                out[n] = kv.key_ptr.*;
                n += 1;
            }
            // Removed after the walk: the iterator never sees a removal.
            for (out[start..n]) |ino| {
                if (s.owed.remove(ino)) fuse_owed_drain_inos.sub(1);
            }
        }
        return out[0..n];
    }

    /// The aged inos one harvest takes at most: the pass's stack room (a
    /// longer backlog loops within the pass).
    pub const TIMER_HARVEST = 64;

    /// ONE timer pass at `now_ns` (module doc): every ino owed since
    /// `now − age_ns` or earlier through `drain`, oldest first per stripe;
    /// what the drain kept is re-owed — no earlier than the next pass's
    /// cutoff, so a drain that keeps aged custody (a failure the data plane
    /// already logged) is retried at the next pass, never spun on here.
    pub fn timerPass(self: *OpenTable, now_ns: u64, age_ns: u64, drain: DrainAged) void {
        const cutoff_ns = now_ns -| age_ns;
        var buf: [TIMER_HARVEST]Aged = undefined;
        var any = false;
        while (true) {
            const aged = self.takeAgedOwed(cutoff_ns, &buf);
            for (aged) |a| {
                any = true;
                const d = drain.call(drain.ctx, a.ino, cutoff_ns);
                writeback_timer_drains.inc();
                if (d.landed) {
                    const residency = clock.monotonicNs() -| a.born_ns;
                    writeback_residency_ns.record(residency);
                    writeback_residency_max_ns.raise(residency);
                }
                const kept = d.kept_born_ns orelse continue;
                writeback_timer_kept.inc();
                self.reowe(a.ino, @max(kept, cutoff_ns + 1)) catch {
                    writeback_timer_reowe_overflows.inc();
                    log.err("write-back timer: ino {d} keeps custody the owed set could not re-note — it lands at its next write's note, an fsync, a syncfs or the unmount", .{a.ino});
                };
            }
            if (aged.len < buf.len) break;
        }
        if (any) writeback_timer_passes.inc();
    }

    pub fn owedCount(self: *OpenTable) u64 {
        var n: u64 = 0;
        for (&self.stripes) |*s| {
            s.lock.lock();
            defer s.lock.unlock();
            n += s.owed.count();
        }
        return n;
    }

    /// Every ino the kernel holds a lookup reference on, copied out one
    /// stripe at a time (the caller notifies with no stripe lock held) —
    /// the kernel view's whole-cache invalidation (decision 0003 §AB.2).
    pub fn trackedInos(self: *OpenTable, gpa: std.mem.Allocator) std.mem.Allocator.Error![]u64 {
        var out: std.ArrayList(u64) = .empty;
        errdefer out.deinit(gpa);
        for (&self.stripes) |*s| {
            // The room is taken with no lock held (a spin lock never covers
            // an allocation); a stripe that grew between retries.
            while (true) {
                s.lock.lock();
                const n = s.map.count();
                if (out.unusedCapacitySlice().len >= n) {
                    var it = s.map.keyIterator();
                    while (it.next()) |k| out.appendAssumeCapacity(k.*);
                    s.lock.unlock();
                    break;
                }
                s.lock.unlock();
                try out.ensureUnusedCapacity(gpa, n + 16);
            }
        }
        return out.toOwnedSlice(gpa);
    }

    fn stripeOf(self: *OpenTable, ino: u64) *Stripe {
        // Fibonacci hashing: consecutive mints spread over the stripes.
        return &self.stripes[@intCast((ino *% 0x9E3779B97F4A7C15) >> 60)];
    }

    /// OPEN / OPENDIR / CREATE. Returns the new count.
    pub fn addOpen(self: *OpenTable, ino: u64) std.mem.Allocator.Error!u32 {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const gop = try s.map.getOrPut(self.gpa, ino);
        if (!gop.found_existing) {
            gop.value_ptr.* = .{};
            fuse_tracked_inodes.add(1);
        }
        if (gop.value_ptr.count == 0) {
            gop.value_ptr.dirty = false;
            fuse_open_inodes.add(1);
        }
        gop.value_ptr.count += 1;
        return gop.value_ptr.count;
    }

    /// RELEASE / RELEASEDIR: saturating at 0. Returns the new count (0 =
    /// the last close) and removes an entry the kernel no longer references.
    pub fn removeOpen(self: *OpenTable, ino: u64) u32 {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const e = s.map.getPtr(ino) orelse return 0;
        if (e.count == 0) return 0;
        e.count -= 1;
        const left = e.count;
        if (left == 0) {
            fuse_open_inodes.sub(1);
            if (e.lookups == 0) {
                _ = s.map.remove(ino);
                fuse_tracked_inodes.sub(1);
            }
        }
        return left;
    }

    pub fn isOpen(self: *OpenTable, ino: u64) bool {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const e = s.map.get(ino) orelse return false;
        return e.count > 0;
    }

    /// A WRITE through an open handle (the control arm's `noteWrite`): the
    /// release path's clean-handle elision keys on it.
    fn markDirty(self: *OpenTable, ino: u64) void {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        if (s.map.getPtr(ino)) |e| e.dirty = true;
    }

    pub fn isDirty(self: *OpenTable, ino: u64) bool {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const e = s.map.get(ino) orelse return false;
        return e.dirty;
    }

    /// One kernel reference per entry reply; the root is never counted.
    pub fn noteLookup(self: *OpenTable, ino: u64) std.mem.Allocator.Error!void {
        if (ino <= 1) return;
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const gop = try s.map.getOrPut(self.gpa, ino);
        if (!gop.found_existing) {
            gop.value_ptr.* = .{};
            fuse_tracked_inodes.add(1);
        }
        gop.value_ptr.lookups += 1;
    }

    /// FORGET / one BATCH_FORGET entry (module doc).
    pub fn returnLookups(self: *OpenTable, ino: u64, nlookup: u64) Verdict {
        const n = @max(nlookup, 1);
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const e = s.map.getPtr(ino) orelse return .{ .evicted = true, .stranded_count = 0 };
        e.lookups -|= n;
        if (e.lookups != 0) return .{ .evicted = false, .stranded_count = 0 };
        const stranded = e.count;
        if (stranded > 0) {
            open_count_stranded.fire("final FORGET with open handles counted — a lost RELEASE (no-open: a lost RELEASEDIR); the count is kept, never zeroed");
            log.err("open_count_stranded: ino {d} reached nlookup 0 with {d} open handle(s) — a RELEASE / RELEASEDIR was lost; the entry is kept until its count drains", .{ ino, stranded });
            return .{ .evicted = true, .stranded_count = stranded };
        }
        _ = s.map.remove(ino);
        fuse_tracked_inodes.sub(1);
        return .{ .evicted = true, .stranded_count = 0 };
    }

    pub fn get(self: *OpenTable, ino: u64) ?Entry {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        return s.map.get(ino);
    }

    /// Entries whose `count > 0` (the teardown sweep's census).
    pub fn openCount(self: *OpenTable) u64 {
        var n: u64 = 0;
        for (&self.stripes) |*s| {
            s.lock.lock();
            defer s.lock.unlock();
            var it = s.map.valueIterator();
            while (it.next()) |e| {
                if (e.count > 0) n += 1;
            }
        }
        return n;
    }
};

// ---------------------------------------------------------------------------
// Tests (the reference's lookup_refcount_tests)
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the tracked inos (decision 0003 §AB.2's whole-view invalidation): every ino a lookup reference holds, the root never" {
    var t = try OpenTable.init(testing.allocator, 64);
    defer t.deinit();
    for ([_]u64{ 1, 2, 3, 500, 2, 77 }) |ino| try t.noteLookup(ino);
    const got = try t.trackedInos(testing.allocator);
    defer testing.allocator.free(got);
    std.mem.sort(u64, got, {}, std.sort.asc(u64));
    try testing.expectEqualSlices(u64, &.{ 2, 3, 77, 500 }, got);
}

test "capacity derivation: four per delivered slot, floor 4096" {
    try testing.expectEqual(@as(u32, 4096), derivedCapacity(1, 4));
    try testing.expectEqual(@as(u32, 4096), derivedCapacity(32, 32));
    try testing.expectEqual(@as(u32, 8192), derivedCapacity(64, 32));
}

test "entry replies count and a partial forget does not evict; the root is never counted" {
    var t = try OpenTable.init(testing.allocator, 64);
    defer t.deinit();
    try t.noteLookup(1);
    try testing.expectEqual(@as(?Entry, null), t.get(1));
    try t.noteLookup(10);
    try t.noteLookup(10);
    try t.noteLookup(10);
    try testing.expectEqual(@as(u64, 3), t.get(10).?.lookups);
    var v = t.returnLookups(10, 2);
    try testing.expect(!v.evicted);
    try testing.expectEqual(@as(u64, 1), t.get(10).?.lookups);
    v = t.returnLookups(10, 1);
    try testing.expect(v.evicted);
    try testing.expectEqual(@as(u32, 0), v.stranded_count);
    try testing.expectEqual(@as(?Entry, null), t.get(10));
}

test "an uncounted inode still evicts on its first forget; nlookup 0 is treated as one" {
    var t = try OpenTable.init(testing.allocator, 64);
    defer t.deinit();
    try testing.expect(t.returnLookups(77, 5).evicted);
    try t.noteLookup(78);
    try testing.expect(t.returnLookups(78, 0).evicted);
    try testing.expectEqual(@as(?Entry, null), t.get(78));
}

test "open count: 0→1 clears dirty, saturating release, the last close removes an unreferenced entry" {
    var t = try OpenTable.init(testing.allocator, 64);
    defer t.deinit();
    try testing.expectEqual(@as(u32, 1), try t.addOpen(5));
    t.markDirty(5);
    try testing.expect(t.isDirty(5));
    try testing.expectEqual(@as(u32, 2), try t.addOpen(5));
    try testing.expect(t.isDirty(5));
    try testing.expectEqual(@as(u32, 1), t.removeOpen(5));
    try testing.expectEqual(@as(u32, 0), t.removeOpen(5));
    try testing.expect(!t.isOpen(5));
    try testing.expectEqual(@as(?Entry, null), t.get(5));
    try testing.expectEqual(@as(u32, 0), t.removeOpen(5));
    // A fresh open generation starts clean.
    try testing.expectEqual(@as(u32, 1), try t.addOpen(5));
    try testing.expect(!t.isDirty(5));
    try testing.expectEqual(@as(u64, 1), t.openCount());
    _ = t.removeOpen(5);
}

test "POSIX-14: a lost release leaves a stranded open count that is detected, counted and NEVER zeroed" {
    var t = try OpenTable.init(testing.allocator, 64);
    defer t.deinit();
    try t.noteLookup(9);
    _ = try t.addOpen(9);
    const before = counters.tripwireCount("open_count_stranded");
    open_count_stranded.resetLogBudgetForTest();
    // The tripwire's own line plus the table's.
    log.testing.expectErrorLogs(2);
    const v = t.returnLookups(9, 1);
    try log.testing.expectAllFired();
    try testing.expect(v.evicted);
    try testing.expectEqual(@as(u32, 1), v.stranded_count);
    try testing.expectEqual(before + 1, counters.tripwireCount("open_count_stranded"));
    // The entry survives with its count intact until the count drains.
    try testing.expectEqual(@as(u32, 1), t.get(9).?.count);
    try testing.expectEqual(@as(u64, 0), t.get(9).?.lookups);
    try testing.expectEqual(@as(u32, 0), t.removeOpen(9));
    try testing.expectEqual(@as(?Entry, null), t.get(9));
}

test "no-open (decision 0003 §AF): a write is OWED until a FLUSH that began after its note ends; a write noted after the FLUSH began stays owed; the owed note outlives the ino's eviction; the control arm owes nothing and marks the generation dirty" {
    var t = try OpenTable.init(testing.allocator, 64);
    defer t.deinit();
    // The control arm: the open generation's dirty bit, no owed set.
    try t.noteLookup(7);
    _ = try t.addOpen(7);
    try t.noteWrite(7, 100);
    try testing.expect(t.isDirty(7));
    try testing.expectEqual(@as(u64, 0), t.owedCount());
    _ = t.removeOpen(7);
    _ = t.returnLookups(7, 1);

    t.no_open = true;
    const g0 = fuse_owed_drain_inos.load();
    try t.noteLookup(9);
    try t.noteWrite(9, 100);
    try testing.expectEqual(@as(u64, 1), t.owedCount());
    try testing.expectEqual(g0 + 1, fuse_owed_drain_inos.load());
    // write → FLUSH: the FLUSH began after the note — covered.
    var b = t.flushBegin(9);
    t.flushEnd(9, b);
    try testing.expectEqual(@as(u64, 0), t.owedCount());
    try testing.expectEqual(g0, fuse_owed_drain_inos.load());
    // FLUSH begins → a write lands (its custody after the FLUSH's check) →
    // the FLUSH ends: the write stays owed.
    b = t.flushBegin(9);
    try t.noteWrite(9, 200);
    t.flushEnd(9, b);
    try testing.expectEqual(@as(u64, 1), t.owedCount());
    // The kernel evicts the ino (its last FORGET): the drain is still owed.
    try testing.expect(t.returnLookups(9, 1).evicted);
    try testing.expectEqual(@as(?Entry, null), t.get(9));
    try testing.expectEqual(@as(u64, 1), t.owedCount());
    // A FLUSH of another ino covers nothing here.
    b = t.flushBegin(9);
    t.flushEnd(9 + 16, b);
    try testing.expectEqual(@as(u64, 1), t.owedCount());
    t.flushEnd(9, b);
    try testing.expectEqual(@as(u64, 0), t.owedCount());
    try testing.expectEqual(g0, fuse_owed_drain_inos.load());
}

test "no-open: the idle harvest takes every owed ino whose latest write is at least the idle bound old — and only those — removes them, bounded by the caller's room; a later write notes the ino again" {
    var t = try OpenTable.init(testing.allocator, 64);
    defer t.deinit();
    t.no_open = true;
    const idle: u64 = 1000;
    for ([_]u64{ 10, 11, 12, 13, 14 }) |ino| try t.noteWrite(ino, 0);
    try t.noteWrite(12, 900); // rewritten recently
    var out: [8]u64 = undefined;
    // Nothing is idle yet at 999 for anyone.
    try testing.expectEqual(@as(usize, 0), t.takeIdleOwed(999, idle, &out).len);
    // At 1000 four are idle; 12 (written at 900) is not.
    var small: [3]u64 = undefined;
    const first = t.takeIdleOwed(1000, idle, &small);
    try testing.expectEqual(@as(usize, 3), first.len);
    const rest = t.takeIdleOwed(1000, idle, &out);
    try testing.expectEqual(@as(usize, 1), rest.len);
    var got: [4]u64 = undefined;
    @memcpy(got[0..3], first);
    got[3] = rest[0];
    std.mem.sort(u64, &got, {}, std.sort.asc(u64));
    try testing.expectEqualSlices(u64, &.{ 10, 11, 13, 14 }, &got);
    try testing.expectEqual(@as(u64, 1), t.owedCount());
    try testing.expectEqual(@as(usize, 0), t.takeIdleOwed(1000, idle, &out).len);
    const late = t.takeIdleOwed(1900, idle, &out);
    try testing.expectEqualSlices(u64, &.{12}, late);
    try testing.expectEqual(@as(u64, 0), t.owedCount());
    try t.noteWrite(10, 2000);
    try testing.expectEqual(@as(u64, 1), t.owedCount());
    try testing.expectEqualSlices(u64, &.{10}, t.takeIdleOwed(3000, idle, &out));
}

test "no-open close bursts coalesce: `recentlyDrained` is true within the window after `markDrained` and false outside it (and for an untracked ino); a deferred close is harvested at the next call whatever its idle age, a later write keeps it deferred, a FLUSH that drains covers it" {
    var t = try OpenTable.init(testing.allocator, 64);
    defer t.deinit();
    t.no_open = true;
    const window: u64 = 100;
    try testing.expect(!t.recentlyDrained(5, 1000, window));
    try t.noteLookup(5);
    try testing.expect(!t.recentlyDrained(5, 1000, window));
    t.markDrained(5, 1000);
    try testing.expect(t.recentlyDrained(5, 1050, window));
    try testing.expect(!t.recentlyDrained(5, 1100, window));
    // A deferred close: owed, harvested at once (idle bound far away).
    try t.deferDrain(5, 1050);
    var out: [4]u64 = undefined;
    try testing.expectEqualSlices(u64, &.{5}, t.takeIdleOwed(1051, std.math.maxInt(u64), &out));
    try testing.expectEqual(@as(u64, 0), t.owedCount());
    // Deferred, then written again: still deferred.
    try t.deferDrain(5, 1060);
    try t.noteWrite(5, 1061);
    try testing.expectEqualSlices(u64, &.{5}, t.takeIdleOwed(1062, std.math.maxInt(u64), &out));
    // A plain write is not harvested before its idle bound.
    try t.noteWrite(5, 1070);
    try testing.expectEqual(@as(usize, 0), t.takeIdleOwed(1071, std.math.maxInt(u64), &out).len);
    // A deferred note a later draining FLUSH covers.
    try t.deferDrain(6, 1080);
    const b = t.flushBegin(6);
    t.flushEnd(6, b);
    try testing.expectEqual(@as(u64, 1), t.owedCount());
    _ = t.returnLookups(5, 1);
    try testing.expectEqualSlices(u64, &.{5}, t.takeIdleOwed(std.math.maxInt(u64), 0, &out));
}

test "the write-back timer's derivations (decision 0003 §AJ): period = the flush cadence (50 ms derived; the strict cadence's 100 ms tick), age = the checkpoint's cadence ceiling (1 s), T_wb's static part = age + period — with one period of drain, the metadata leaves' landing ceiling `max_age + 2 × tick`; the knob's words" {
    try testing.expectEqual(@as(u64, checkpoint.DEFAULT_FLUSH_INTERVAL_MS * std.time.ns_per_ms), timerPeriodNs(checkpoint.DEFAULT_FLUSH_INTERVAL_MS));
    try testing.expectEqual(@as(u64, 50 * std.time.ns_per_ms), timerPeriodNs(50));
    try testing.expectEqual(@as(u64, checkpoint.STRICT_TICK_PERIOD_MS * std.time.ns_per_ms), timerPeriodNs(0));
    try testing.expectEqual(@as(u64, checkpoint.CHECKPOINT_MAX_AGE_MS * std.time.ns_per_ms), timerAgeNs(checkpoint.CHECKPOINT_MAX_AGE_MS));
    const p = timerPeriodNs(checkpoint.DEFAULT_FLUSH_INTERVAL_MS);
    try testing.expectEqual(@as(u64, 1050 * std.time.ns_per_ms), boundStaticNs(timerAgeNs(checkpoint.CHECKPOINT_MAX_AGE_MS), p));
    try testing.expectEqual(checkpoint.checkpointLandingCeilingMs(checkpoint.CHECKPOINT_MAX_AGE_MS, checkpoint.DEFAULT_FLUSH_INTERVAL_MS) * std.time.ns_per_ms, boundStaticNs(timerAgeNs(checkpoint.CHECKPOINT_MAX_AGE_MS), p) + p);
    try testing.expectEqual(Posture.timer, Posture.ofKnobWord("timer"));
    try testing.expectEqual(Posture.close, Posture.ofKnobWord("close"));
}

test "the timer posture: the owed set is age-ordered — a note's birth is its ino's first since it held nothing (later notes keep it), the harvest takes exactly the inos born by the cutoff in birth order, bounded by the caller's room; a re-owe keeps the older birth and its stale heap entry is skipped; the control open arm owes too" {
    var t = try OpenTable.init(testing.allocator, 64);
    defer t.deinit();
    t.posture = .timer;
    // The control open arm (no_open off) still notes under the timer.
    try t.noteLookup(7);
    _ = try t.addOpen(7);
    try t.noteWrite(7, 100);
    try testing.expect(t.isDirty(7));
    try testing.expectEqual(@as(u64, 1), t.owedCount());
    _ = t.removeOpen(7);
    // Births 100 (7), 200 (8), 300 (9); 7 written again at 900 keeps 100.
    try t.noteWrite(8, 200);
    try t.noteWrite(9, 300);
    try t.noteWrite(7, 900);
    var out: [8]Aged = undefined;
    try testing.expectEqual(@as(usize, 0), t.takeAgedOwed(99, &out).len);
    var small: [1]Aged = undefined;
    var two: [2]Aged = undefined;
    two[0] = t.takeAgedOwed(250, &small)[0];
    const rest = t.takeAgedOwed(250, &out);
    try testing.expectEqual(@as(usize, 1), rest.len);
    two[1] = rest[0];
    try testing.expectEqualSlices(Aged, &.{ .{ .ino = 7, .born_ns = 100 }, .{ .ino = 8, .born_ns = 200 } }, sortedAged(&two));
    try testing.expectEqual(@as(u64, 1), t.owedCount());
    // A write after the harvest is a new birth.
    try t.noteWrite(7, 1000);
    // A drained ino kept custody born at 260: re-owed at it (older than
    // the note at 1000); the 1000 entry turns stale.
    try t.reowe(7, 260);
    // A re-owe younger than the live birth changes nothing.
    try t.reowe(9, 5000);
    try testing.expectEqual(@as(u64, 2), t.owedCount());
    try testing.expectEqualSlices(Aged, &.{ .{ .ino = 7, .born_ns = 260 }, .{ .ino = 9, .born_ns = 300 } }, sortedAged(t.takeAgedOwed(300, &out)));
    // The stale entry (7 at 1000) is skipped; nothing is owed.
    try testing.expectEqual(@as(usize, 0), t.takeAgedOwed(std.math.maxInt(u64), &out).len);
    try testing.expectEqual(@as(u64, 0), t.owedCount());
    // A re-owe of an ino nothing owes notes it.
    try t.reowe(11, 400);
    try testing.expectEqualSlices(Aged, &.{.{ .ino = 11, .born_ns = 400 }}, t.takeAgedOwed(400, &out));
}

fn sortedAged(a: []Aged) []Aged {
    std.mem.sort(Aged, a, {}, struct {
        fn lt(_: void, x: Aged, y: Aged) bool {
            return x.ino < y.ino;
        }
    }.lt);
    return a;
}

test "the timer pass: every ino born by `now − age` drains once, in birth order per stripe; what the drain kept is re-owed at its birth (never earlier than the next cutoff, so a drain keeping aged custody is retried next pass, not spun on); the residency records only drains that landed; a pass with nothing aged counts nothing" {
    var t = try OpenTable.init(testing.allocator, 64);
    defer t.deinit();
    t.posture = .timer;
    const Fake = struct {
        calls: std.ArrayList(u64) = .empty,
        cutoffs: std.ArrayList(u64) = .empty,
        fn drain(ctx: *anyopaque, ino: u64, cutoff_ns: u64) Drained {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.calls.append(testing.allocator, ino) catch unreachable;
            self.cutoffs.append(testing.allocator, cutoff_ns) catch unreachable;
            return switch (ino) {
                // A younger block kept.
                21 => .{ .landed = true, .kept_born_ns = 1500 },
                // A drain that failed: its aged custody kept.
                22 => .{ .landed = false, .kept_born_ns = 100 },
                // fsync drained it first: nothing landed, nothing kept.
                23 => .{ .landed = false, .kept_born_ns = null },
                else => .{ .landed = true, .kept_born_ns = null },
            };
        }
    };
    var fake: Fake = .{};
    defer fake.calls.deinit(testing.allocator);
    defer fake.cutoffs.deinit(testing.allocator);
    const face: DrainAged = .{ .ctx = &fake, .call = &Fake.drain };
    for ([_]u64{ 20, 21, 22, 23 }) |ino| try t.noteWrite(ino, 100);
    try t.noteWrite(24, 1800);
    const p0 = writeback_timer_passes.load();
    const d0 = writeback_timer_drains.load();
    const k0 = writeback_timer_kept.load();
    const r0 = writeback_residency_ns.snapshot().count;
    // Nothing aged at now = 1000 − age 1000.
    t.timerPass(1000, 1000, face);
    try testing.expectEqual(@as(usize, 0), fake.calls.items.len);
    try testing.expectEqual(p0, writeback_timer_passes.load());
    // now = 2000, age 1000: cutoff 1000 — the four born at 100.
    t.timerPass(2000, 1000, face);
    std.mem.sort(u64, fake.calls.items, {}, std.sort.asc(u64));
    try testing.expectEqualSlices(u64, &.{ 20, 21, 22, 23 }, fake.calls.items);
    for (fake.cutoffs.items) |c| try testing.expectEqual(@as(u64, 1000), c);
    try testing.expectEqual(p0 + 1, writeback_timer_passes.load());
    try testing.expectEqual(d0 + 4, writeback_timer_drains.load());
    try testing.expectEqual(k0 + 2, writeback_timer_kept.load());
    try testing.expectEqual(r0 + 2, writeback_residency_ns.snapshot().count);
    // 21 owed at 1500 (its kept block), 22 at the next cutoff (1001), 24 at 1800.
    try testing.expectEqual(@as(u64, 3), t.owedCount());
    var out: [8]Aged = undefined;
    try testing.expectEqualSlices(Aged, &.{ .{ .ino = 21, .born_ns = 1500 }, .{ .ino = 22, .born_ns = 1001 }, .{ .ino = 24, .born_ns = 1800 } }, sortedAged(t.takeAgedOwed(std.math.maxInt(u64), &out)));
}

test "a healthy open/lookup lifecycle never trips: lookup, open, release, forget" {
    var t = try OpenTable.init(testing.allocator, 64);
    defer t.deinit();
    const before = counters.tripwireCount("open_count_stranded");
    try t.noteLookup(20);
    _ = try t.addOpen(20);
    try testing.expectEqual(@as(u32, 0), t.removeOpen(20));
    // Still referenced by the kernel's lookup: the entry stays.
    try testing.expectEqual(@as(u64, 1), t.get(20).?.lookups);
    try testing.expect(t.returnLookups(20, 1).evicted);
    try testing.expectEqual(before, counters.tripwireCount("open_count_stranded"));
    try testing.expectEqual(@as(u64, 0), t.openCount());
}
