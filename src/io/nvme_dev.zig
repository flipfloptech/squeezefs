//! NVMe block-device io_uring workers (`docs/porting/nvme_dev.md`; charter
//! §1.2 "Block I/O = io_uring workers with fixed files", §6.4 fsync, §13
//! rung 1). Linux only.
//!
//! One `Device` = one O_DIRECT data volume (regular file or block device)
//! served by N lanes; a lane is one OS thread (`sqz-nvme<N>`) owning one
//! io_uring, one O_DIRECT fd, one bounded request queue, one write watermark
//! and one eventfd wake. Callers never block on the queue (try-push) and the
//! worker never blocks on it (try-pop): the single park is the ring's
//! (`ring_park.ParkedRing` — the ONE ring-park primitive, decision 0003
//! WP-C), with the eventfd READ kept armed so an arrival wakes the lane
//! THROUGH the ring (measured on the stock kernel before this shape:
//! `dev_queue` 500 µs vs `dev_service` 285 µs — the queue wait cost more
//! than the device). A request is a preallocated cell whose index is the
//! SQE `user_data`; the caller parks on the cell's `sync.Completion`, the
//! queue is the ONE `sync.BoundedMpmc`, errors are the ONE table
//! (`core/errors.zig`).
//!
//! Discard/trim POLICY (elision, the queue, pacing) lives in the reclaim
//! module (`data/reclaim.zig`); the device offers only the COMMAND
//! (`discardRange`): `IORING_OP_FALLOCATE(PUNCH_HOLE | KEEP_SIZE)` on a
//! regular file, `IORING_OP_URING_CMD(BLOCK_URING_CMD_DISCARD)` on a block
//! device (the NVMe DSM Deallocate; no payload, not write-bandwidth-
//! accounted) — through the SAME write lane, fence-gated like every other
//! write-class command, never a classical ioctl on a product path. The
//! substrate is classified ONCE at open (`reclaimOp`), never per batch.

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const nd = @import("nvme_derive.zig");
const ring_park = @import("ring_park.zig");
const mpmc = @import("../sync/mpmc.zig");
const completion = @import("../sync/completion.zig");
const futex = @import("../sync/futex.zig");
const derive = @import("../core/derive.zig");
const counters = @import("../core/counters.zig");
const log = @import("../core/log.zig");
const clock = @import("../core/clock.zig");
const thread_name = @import("../core/thread_name.zig");
const reclaim = @import("../data/reclaim.zig");
const mem_budget = @import("../mem/budget.zig");

pub const Refusal = nd.Refusal;
pub const Terminal = nd.Terminal;
pub const Retryable = nd.Retryable;
pub const DeviceError = nd.DeviceError;
/// The portable halves (`nvme_derive.zig`): stats, fence/custody, seams.
pub const stats = nd.stats;
pub const writeStatsJson = nd.writeStatsJson;
pub const FenceSignal = nd.FenceSignal;
pub const CustodyHooks = nd.CustodyHooks;
pub const installCustodyHooks = nd.installCustodyHooks;
pub const clearCustodyPoisonForTest = nd.clearCustodyPoisonForTest;
const Fence = nd.Fence;
const Seams = nd.Seams;
const takeArmed = nd.takeArmed;
const poisonCustody = nd.poisonCustody;
const authorizeDma = nd.authorizeDma;

// ---------------------------------------------------------------------------
// Buffer pools (pool.rs). Process-global like the reference: the ranged
// slab is ONE mapping every lane registers as fixed buffer 0.
// ---------------------------------------------------------------------------

/// A DMA buffer handle. The owner (worker between submit and CQE, caller
/// after a completed read) releases it exactly once.
pub const Buf = struct {
    ptr: [*]u8,
    /// Valid bytes (the requested read size; the slot may be larger).
    len: usize,
    kind: Kind,
    slot: u32 = 0,

    pub const Kind = enum(u8) {
        /// Nothing owned (zero-length result, or an aligned caller buffer).
        none,
        /// A 64 KiB slot inside the registered slab.
        ranged,
        /// A 4 MiB whole-block pool buffer.
        block,
        /// A one-off page-aligned mapping (slab exhausted, or a bounce
        /// larger than the pool slot); `slot` holds its page count.
        adhoc,
    };

    pub const empty: Buf = .{ .ptr = undefined, .len = 0, .kind = .none };

    pub fn bytes(self: Buf) []u8 {
        return if (self.len == 0) &.{} else self.ptr[0..self.len];
    }

    pub fn release(self: Buf) void {
        switch (self.kind) {
            .none => {},
            .ranged => pools().ranged.release(self.slot),
            .block => pools().block.release(self.ptr),
            .adhoc => unmapAdhoc(self.ptr, self.slot),
        }
    }
};

/// Anonymous page-aligned mapping of `len` bytes. Page alignment ≥ 4096 on
/// every supported arch, so the result honours the DMA contract.
fn mapAnon(len: usize) Terminal![*]u8 {
    const r = linux.mmap(null, len, .{ .READ = true, .WRITE = true }, .{ .TYPE = .PRIVATE, .ANONYMOUS = true }, -1, 0);
    if (linux.errno(r) != .SUCCESS) return error.OutOfMemory;
    return @ptrFromInt(r);
}

/// Ad-hoc mappings carry their page count in `slot` so `release` can unmap
/// the exact length; the page is the kernel's (`derive.facts()`, the ONE
/// source).
fn mapAdhoc(len: usize) Terminal!Buf {
    const page: usize = derive.facts().page_size;
    const pages: u32 = @intCast(derive.ceilDiv(len, page));
    const ptr = try mapAnon(pages * page);
    return .{ .ptr = ptr, .len = len, .kind = .adhoc, .slot = pages };
}

fn unmapAdhoc(ptr: [*]u8, pages: u32) void {
    _ = linux.munmap(ptr, pages * @as(usize, derive.facts().page_size));
}

/// The ranged read-bounce slab: `rangedPoolCapacity(cpus)` × 64 KiB in ONE
/// mapping. Registered by every lane as fixed buffer 0 so slab-resident cold
/// fills ride ReadFixed and skip the per-op page pin (gup).
const RangedSlab = struct {
    base: [*]u8,
    len: usize,
    cap: u32,
    free: mpmc.BoundedMpmc(u32),
    allocated_bytes: counters.Gauge = .{},
    /// The whole slab: pinned by every lane's fixed-buffer registration,
    /// resident for the process's life (`nvme_ranged_slab`).
    slab_bytes: counters.Gauge = .{},

    fn init(gpa: std.mem.Allocator, cap: u32) !RangedSlab {
        const len = @as(usize, cap) * nd.ranged_buf_size;
        const base = try mapAnon(len);
        var free = try mpmc.BoundedMpmc(u32).init(gpa, cap);
        var i: u32 = 0;
        while (i < cap) : (i += 1) std.debug.assert(free.tryPush(i));
        return .{ .base = base, .len = len, .cap = cap, .free = free };
    }

    fn acquire(self: *RangedSlab, len: usize) ?Buf {
        const slot = self.free.tryPop() orelse return null;
        self.allocated_bytes.add(nd.ranged_buf_size);
        return .{ .ptr = self.base + @as(usize, slot) * nd.ranged_buf_size, .len = len, .kind = .ranged, .slot = slot };
    }

    /// The ring holds every slot and any reader takes: a refused push is a
    /// taker mid-pop, never a full ring — retried (decision 0003 §AI.4).
    fn release(self: *RangedSlab, slot: u32) void {
        self.allocated_bytes.sub(nd.ranged_buf_size);
        self.free.pushSpin(slot);
    }

    /// `base <= ptr && ptr + len <= base + len`: the ReadFixed eligibility
    /// test (the kernel refuses a fixed read outside the registered range).
    fn contains(self: *const RangedSlab, ptr: [*]const u8, len: usize) bool {
        const p = @intFromPtr(ptr);
        const b = @intFromPtr(self.base);
        return b <= p and p + len <= b + self.len;
    }

    fn iovec(self: *RangedSlab) std.posix.iovec {
        return .{ .base = self.base, .len = self.len };
    }
};

/// The whole-block (4 MiB) pool: `blockPoolCapacity(cpus)` cached mappings,
/// allocated lazily; exhaustion degrades to a fresh mapping
/// (`aligned_pool_misses`), a release past the cap unmaps. Deliberately NOT
/// registered as a fixed buffer: pinning cores×16×4 MiB (GiBs) trades real
/// RAM for a pin cost already amortized across 4 MiB DMAs. A cached
/// mapping is a whole touched block (a DMA filled it): the cache is the
/// pool's retention (`cached_bytes`, the `aligned_buf_pool` component's
/// gauge) and the idle-trim law unmaps the entries idle long enough.
const BlockPool = struct {
    free: mpmc.BoundedMpmc(Cached),
    cap: u32,
    allocated_bytes: counters.Gauge = .{},
    /// Free cached mappings × the block size.
    cached_bytes: counters.Gauge = .{},

    const Cached = struct { addr: usize, idle_tick: u64 };

    fn init(gpa: std.mem.Allocator, cap: u32) !BlockPool {
        return .{ .free = try mpmc.BoundedMpmc(Cached).init(gpa, cap), .cap = cap };
    }

    fn acquire(self: *BlockPool, len: usize) Terminal!Buf {
        std.debug.assert(len <= nd.block_buf_size);
        const ptr: [*]u8 = if (self.free.tryPop()) |c| blk: {
            stats.aligned_pool_hits.inc();
            self.cached_bytes.sub(nd.block_buf_size);
            break :blk @ptrFromInt(c.addr);
        } else blk: {
            stats.aligned_pool_misses.inc();
            break :blk try mapAnon(nd.block_buf_size);
        };
        self.allocated_bytes.add(nd.block_buf_size);
        return .{ .ptr = ptr, .len = len, .kind = .block };
    }

    fn release(self: *BlockPool, ptr: [*]u8) void {
        self.allocated_bytes.sub(nd.block_buf_size);
        // The gauge moves BEFORE the push: a trimmer that pops the entry
        // subtracts what was added.
        self.cached_bytes.add(nd.block_buf_size);
        if (!self.free.tryPush(.{ .addr = @intFromPtr(ptr), .idle_tick = mem_budget.authority.ticks() })) {
            self.cached_bytes.sub(nd.block_buf_size);
            _ = linux.munmap(ptr, nd.block_buf_size);
        }
    }

    /// The idle-trim law: unmap cached entries idle ≥ `min_idle_ticks`
    /// until `cached_bytes ≤ target` (0 = every idle one). One entry out
    /// of the FIFO at a time, pushed back when it stays; one lap over the
    /// cap bounds the pass.
    fn trimToward(self: *BlockPool, min_idle_ticks: u64, target_cached_bytes: u64) void {
        const now = mem_budget.authority.ticks();
        var seen: u32 = 0;
        while (seen < self.cap and self.cached_bytes.load() > target_cached_bytes) : (seen += 1) {
            const c = self.free.tryPop() orelse break;
            if (now -| c.idle_tick >= min_idle_ticks) {
                self.cached_bytes.sub(nd.block_buf_size);
                stats.aligned_pool_trimmed_bytes.add(nd.block_buf_size);
                _ = linux.munmap(@as([*]align(std.heap.page_size_min) u8, @ptrFromInt(c.addr)), nd.block_buf_size);
                continue;
            }
            // Below the cap by construction (we hold one); a refusal is a
            // releaser's wrap cell mid-pop (`mpmc.pushSpin`).
            self.free.pushSpin(c);
        }
    }

    fn shedFn(ctx: *anyopaque, target: u64) void {
        const self: *BlockPool = @ptrCast(@alignCast(ctx));
        self.trimToward(mem_budget.trim_red_min_idle_ticks, target);
    }

    fn trimFn(ctx: *anyopaque, min_idle_ticks: u64) void {
        const self: *BlockPool = @ptrCast(@alignCast(ctx));
        self.trimToward(min_idle_ticks, 0);
    }

    /// `aligned_buf_pool` (the reference's registry name for this pool):
    /// gauge = the cached (idle) mappings; floor 16 blocks — one 16-fill
    /// burst's worth of hot buffers survives a Red shed (the shared pool
    /// floor of `docs/porting/mem_budget.md` §2.5); weight 2.
    fn component(self: *BlockPool) mem_budget.Component {
        return mem_budget.Component.init("aligned_buf_pool", 16 * nd.block_buf_size, 2, .{ .gauge = &self.cached_bytes }, .{ .ctx = self, .call = &shedFn }).withTrim(.{ .ctx = self, .call = &trimFn });
    }
};

const Pools = struct {
    ranged: RangedSlab,
    block: BlockPool,
};

var pools_storage: Pools = undefined;
/// 0 = uninitialised, 1 = initialising, 2 = ready, 3 = failed.
var pools_state: std.atomic.Value(u8) = .init(0);

/// Build the process pools once, from the process CPU root in force
/// (`derive.facts().cpus` — never the calling thread's affinity: whichever
/// thread first opens a device must not size process-wide pools by its own
/// pin). The release store of the ready state pairs with the acquire loads
/// in `pools()` so every thread sees the built pools. A failure is terminal
/// for the process's data plane and is reported, not panicked, to the
/// opener.
pub fn initPools() Terminal!void {
    switch (pools_state.load(.acquire)) {
        2 => return,
        3 => return error.OutOfMemory,
        else => {},
    }
    if (pools_state.cmpxchgStrong(0, 1, .acq_rel, .acquire) == null) {
        const cpus = derive.facts().cpus;
        // Process-lifetime pool memory: never freed, so the page allocator
        // (not a test allocator that would report it as a leak).
        const gpa = std.heap.page_allocator;
        const ranged = RangedSlab.init(gpa, nd.rangedPoolCapacity(cpus)) catch {
            pools_state.store(3, .release);
            return error.OutOfMemory;
        };
        const block = BlockPool.init(gpa, nd.blockPoolCapacity(cpus)) catch {
            pools_state.store(3, .release);
            return error.OutOfMemory;
        };
        pools_storage = .{ .ranged = ranged, .block = block };
        // The process-lifetime pools register once: the pinned ranged slab
        // (attribution only — a registered fixed buffer cannot be
        // returned) and the block cache (idle-trimmed, sheddable).
        pools_storage.ranged.slab_bytes.set(pools_storage.ranged.len);
        mem_budget.authority.register(mem_budget.Component.init("nvme_ranged_slab", pools_storage.ranged.len, 0, .{ .gauge = &pools_storage.ranged.slab_bytes }, null));
        mem_budget.authority.register(pools_storage.block.component());
        pools_state.store(2, .release);
        return;
    }
    while (true) {
        switch (pools_state.load(.acquire)) {
            2 => return,
            3 => return error.OutOfMemory,
            else => std.atomic.spinLoopHint(),
        }
    }
}

/// The built pools; `initPools` must have succeeded (every device open and
/// every acquire runs it first, so a release always finds them).
pub fn pools() *Pools {
    std.debug.assert(pools_state.load(.acquire) == 2);
    return &pools_storage;
}

/// Pick and check out the read-bounce buffer for `size` bytes: ≤ 64 KiB
/// from the slab (fixed-buffer eligible; an exhausted slab degrades to an
/// ad-hoc mapping on a plain Read), larger from the whole-block pool.
pub fn acquireReadBuf(size: usize) Terminal!Buf {
    try initPools();
    const p = pools();
    return switch (nd.readBouncePool(size)) {
        .ranged => p.ranged.acquire(size) orelse mapAdhoc(size),
        .block => p.block.acquire(size),
    };
}

/// A whole-block pool buffer (4 MiB, 4 KiB-aligned) — the write path's
/// aligned-source shape.
pub fn acquireBlockBuf() Terminal!Buf {
    try initPools();
    return pools().block.acquire(nd.block_buf_size);
}

// ---------------------------------------------------------------------------
// Request cells and their completion.
// ---------------------------------------------------------------------------

const Op = enum(u8) { read, write, fsync, discard };

/// How the worker answered: a kernel result, or a synthetic class.
const Outcome = union(enum) {
    /// The CQE result (≥ 0 bytes, < 0 is `-errno`).
    io: i32,
    /// Teardown drained the request before it reached the ring.
    shutdown,
    /// The SQ stayed full after `sq_push_retries` flushes.
    sq_full,
    /// The lane's ring died with the request in it, or before it was pushed.
    ring_failed,
};

/// One request = one slot. Cells are preallocated per lane (the queue bound
/// IS the slot bound, so `user_data = cell index < uring_req_queue_cap`); the
/// hot path allocates nothing. A cell is handed off with the queue push
/// (release) and back with the completion CAS (release), never raced; the
/// worker's own "in the ring" mark is `Worker.in_ring`.
const Cell = struct {
    op: Op = .read,
    offset: u64 = 0,
    /// What the SQE names (a discard names no memory: `dma_len` is its
    /// byte length).
    dma_ptr: [*]u8 = undefined,
    dma_len: usize = 0,
    /// Buffer the WORKER owns until the CQE: the read bounce slot, or the
    /// write bounce copy. `.none` for an aligned caller-owned write source
    /// (the caller is parked for the whole DMA, so its slice outlives it).
    buf: Buf = .empty,
    /// The enqueue stamp, 0 while idle — the overdue census's input (read
    /// by the watchdog's thread as a sample, so atomic; the worker's queue
    /// histograms read it too). Relaxed: nothing is published through it.
    enq_ns: std.atomic.Value(u64) = .init(0),
    /// The submit stamp: the service phase's origin (worker-written).
    submitted_ns: u64 = 0,
    /// The ONE completion cell (`sync/completion.zig`). The caller parks on
    /// it for the whole op — no per-op timeout (charter §4; decision 0003
    /// Q5), so it is never abandoned: the worker owns `buf` until it
    /// completes, the caller owns the cell after.
    done: completion.Completion(Outcome) = .{},
};

// ---------------------------------------------------------------------------
// Write watermark (spec §4): per-lane submitted/completed counters the
// barrier drains. ALL SeqCst: the Dekker pair between `waitCompleted`'s
// `waiters++ ; re-read completed` and `noteCompleted`'s `completed++ ; read
// waiters` guarantees a completer that misses the waiter forces the waiter
// to see the count, and vice versa.
// ---------------------------------------------------------------------------

pub const Watermark = struct { submitted: u64, completed: u64 };

const WriteWatermark = struct {
    submitted: std.atomic.Value(u64) = .init(0),
    completed: std.atomic.Value(u64) = .init(0),
    waiters: std.atomic.Value(u32) = .init(0),
    dead: std.atomic.Value(bool) = .init(false),
    /// Futex word bumped on every notify so a waiter never sleeps on a
    /// stale observation.
    epoch: futex.Word = .init(0),

    /// Caller thread, BEFORE the queue push.
    fn noteSubmitted(self: *WriteWatermark) void {
        _ = self.submitted.fetchAdd(1, .seq_cst);
    }

    /// Worker thread at the CQE (authoritative), or the caller on a submit
    /// failure. Writes AND fsyncs count symmetrically.
    fn noteCompleted(self: *WriteWatermark) void {
        const c = self.completed.fetchAdd(1, .seq_cst) + 1;
        if (c > self.submitted.load(.seq_cst)) stats.watermark_underflow.fire("completed ran past submitted");
        if (self.waiters.load(.seq_cst) > 0) self.notify();
    }

    fn notify(self: *WriteWatermark) void {
        _ = self.epoch.fetchAdd(1, .seq_cst);
        futex.wakeAll(&self.epoch);
    }

    /// Every exit path of the worker: parked barriers fail loud, never
    /// falsely complete.
    fn markDead(self: *WriteWatermark) void {
        self.dead.store(true, .seq_cst);
        self.notify();
    }

    fn snapshot(self: *const WriteWatermark) Watermark {
        return .{ .submitted = self.submitted.load(.seq_cst), .completed = self.completed.load(.seq_cst) };
    }

    fn waitCompleted(self: *WriteWatermark, target: u64) Terminal!void {
        while (true) {
            if (self.completed.load(.seq_cst) >= target) return;
            if (self.dead.load(.seq_cst)) return error.WorkerShutdown;
            const e = self.epoch.load(.seq_cst);
            _ = self.waiters.fetchAdd(1, .seq_cst);
            defer _ = self.waiters.fetchSub(1, .seq_cst);
            // Re-check under the registered waiter (the Dekker half).
            if (self.completed.load(.seq_cst) >= target) return;
            if (self.dead.load(.seq_cst)) return error.WorkerShutdown;
            futex.wait(&self.epoch, e);
        }
    }
};

// ---------------------------------------------------------------------------
// Lane: thread + ring + O_DIRECT fd + bounded queue + watermark + wake.
// ---------------------------------------------------------------------------

/// Process-global lane numbering (`sqz-nvme<N>` across ALL devices), so
/// `daemon_cpu_ns_by_class` can attribute every lane.
var next_lane: std.atomic.Value(u32) = .init(0);

pub fn laneThreadName(n: u32) []const u8 {
    const S = struct {
        threadlocal var buf: [16]u8 = undefined;
    };
    return std.fmt.bufPrint(&S.buf, "sqz-nvme{d}", .{n}) catch unreachable;
}

/// The arming handshake's verdict: the fd, the ring and the registrations
/// are done, or which of the two openings refused.
const ArmVerdict = enum { armed, open_refused, ring_failed };

const Lane = struct {
    dev: *Device,
    /// Process-global lane number (the thread name's `<N>`).
    number: u32,
    queue: mpmc.BoundedMpmc(u32),
    cells: []Cell,
    free_cells: mpmc.BoundedMpmc(u32),
    wm: WriteWatermark = .{},
    /// The ring, its eventfd wake and the park (`ring_park.ParkedRing`).
    /// Built ON the lane thread (`SINGLE_ISSUER`: the thread that sets the
    /// ring up is its only submitter) and quiesced there at exit; freed by
    /// the closer after the join. Other threads touch only its wake face,
    /// and only between `ready == .armed` and the seal.
    park: ring_park.ParkedRing = undefined,
    /// Refuses new submitters (SeqCst — Dekker with `inflight_submitters`).
    closing: std.atomic.Value(bool) = .init(false),
    /// Producers inside `submit`. The closer waits for 0 before sealing.
    inflight_submitters: std.atomic.Value(u32) = .init(0),
    /// Set only after every in-flight submitter left: the worker's
    /// disconnect signal, after which the queue can only drain.
    sealed: std.atomic.Value(bool) = .init(false),
    /// The arming handshake (a `sync.Completion`): `spawn` waits for it so
    /// an opener never gets a zombie lane and a node that vanishes between
    /// the probe and the worker's open is reported at open time, not at
    /// first I/O. Its release publishes `park`, `fixed_file`, `fixed_buf`.
    ready: completion.Completion(ArmVerdict) = .{},
    /// Registration outcomes, published by `ready` (engagement facts for
    /// the stats face and the tests: a ReadFixed row is invalid unless the
    /// slab is registered on the lane that served it).
    fixed_file: bool = false,
    fixed_buf: bool = false,
    thread: ?std.Thread = null,

    fn spawn(gpa: std.mem.Allocator, dev: *Device) !*Lane {
        const lane = try gpa.create(Lane);
        errdefer gpa.destroy(lane);
        const cells = try gpa.alloc(Cell, nd.uring_req_queue_cap);
        errdefer gpa.free(cells);
        @memset(cells, .{});
        var queue = try mpmc.BoundedMpmc(u32).init(gpa, nd.uring_req_queue_cap);
        errdefer queue.deinit(gpa);
        var free_cells = try mpmc.BoundedMpmc(u32).init(gpa, nd.uring_req_queue_cap);
        errdefer free_cells.deinit(gpa);
        var i: u32 = 0;
        while (i < nd.uring_req_queue_cap) : (i += 1) std.debug.assert(free_cells.tryPush(i));
        lane.* = .{
            .dev = dev,
            .number = next_lane.fetchAdd(1, .monotonic),
            .queue = queue,
            .cells = cells,
            .free_cells = free_cells,
        };
        lane.thread = try std.Thread.spawn(.{}, workerMain, .{lane});
        return lane;
    }

    /// Block until the worker armed (or failed); the caller deinits a
    /// failed lane.
    fn awaitArmed(self: *Lane) error{ DirectIoUnsupported, SystemResources }!void {
        return switch (self.ready.wait()) {
            .armed => {},
            .open_refused => error.DirectIoUnsupported,
            .ring_failed => error.SystemResources,
        };
    }

    fn publishReady(self: *Lane, verdict: ArmVerdict) void {
        // The opener is parked on the handshake and never leaves it early.
        const opener_waits = self.ready.complete(verdict);
        std.debug.assert(opener_waits);
    }

    /// The park exists exactly when the lane armed (the worker built it
    /// before publishing, and tears it down only after the seal).
    fn hasPark(self: *Lane) bool {
        return if (self.ready.poll()) |v| v == .armed else false;
    }

    /// Caller side: a free cell or null (the backpressure door; the queue
    /// shares this bound so a held cell always has a queue slot).
    fn acquireCell(self: *Lane) ?u32 {
        const idx = self.free_cells.tryPop() orelse return null;
        const c = &self.cells[idx];
        c.* = .{};
        return idx;
    }

    fn freeCell(self: *Lane, idx: u32) void {
        // A 4096-cell ring holding all 4096 cells with many allocating
        // threads: a refusal is a wrap cell mid-pop (`mpmc.pushSpin`).
        self.free_cells.pushSpin(idx);
    }

    /// Publish a filled cell: queue push THEN wake (the coalescer orders
    /// the two). False when the lane is closing (the caller counts
    /// `uring_queue_full` and frees the cell).
    fn submit(self: *Lane, idx: u32) bool {
        // SeqCst RMW: pairs with the closer's SeqCst `closing` store +
        // `inflight_submitters` load so exactly one of us sees the other.
        _ = self.inflight_submitters.fetchAdd(1, .seq_cst);
        defer _ = self.inflight_submitters.fetchSub(1, .release);
        if (self.closing.load(.seq_cst)) return false;
        if (!self.queue.tryPush(idx)) {
            stats.queue_push_with_cell.fire("lane queue refused a push while a cell was held");
            return false;
        }
        self.wake();
        return true;
    }

    /// The producer half of the wake pair (`ring_park.Wake`, always through
    /// the coalescer): N publishes between two lane passes cost ONE eventfd
    /// write, the rest elide — `dev_wake_{writes,elided}`.
    fn wake(self: *Lane) void {
        if (self.park.wake()) stats.dev_wake_writes.inc() else stats.dev_wakes_elided.inc();
    }

    /// Caller side: park on the cell until the worker publishes the outcome
    /// — UNBOUNDED for every class. A per-op timeout is banned (charter
    /// §4): an op the device never answers is OVERDUE in
    /// `Device.overdueCensus` for the watchdog to log loudly and, past its
    /// bound, to fence the VOLUME through the lattice (§1.8) — never one
    /// EIO per op after 30 s while the writes behind it park on the same
    /// lane (decision 0003 Q5).
    fn wait(self: *Lane, idx: u32) Outcome {
        // The completion's acquire pairs with the worker's release CAS so
        // `buf` is visible with the outcome.
        return self.cells[idx].done.wait();
    }

    /// Worker side: publish the outcome; the parked caller owns the cell
    /// from here (the completion's release CAS publishes `buf`). A caller
    /// never abandons (no per-op timeout), so the completer never inherits
    /// the cleanup.
    fn complete(self: *Lane, idx: u32, outcome: Outcome) void {
        const c = &self.cells[idx];
        c.enq_ns.store(0, .monotonic);
        const waiter_owns = c.done.complete(outcome);
        std.debug.assert(waiter_owns);
    }

    /// Stop + join (idempotent): refuse new submitters → wait for in-flight
    /// submitters → seal → unconditional eventfd write → join. Join is
    /// deliberately SYNCHRONOUS: exit cleanup has run when this returns.
    /// Worst case = device latency of in-flight SQEs.
    fn shutdown(self: *Lane) void {
        self.closing.store(true, .seq_cst);
        while (self.inflight_submitters.load(.seq_cst) != 0) std.Thread.yield() catch {};
        // Release: every push that preceded the seal is visible to the
        // worker's sealed-then-scan (acquire) pass.
        self.sealed.store(true, .release);
        if (self.thread) |t| {
            // The eventfd outlives the worker (freed in `deinit`, after the
            // join), so this write never races the lane's teardown.
            if (self.hasPark()) self.park.forceWake();
            t.join();
            self.thread = null;
        }
    }

    fn deinit(self: *Lane, gpa: std.mem.Allocator) void {
        self.shutdown();
        // The worker quiesced the ring on its own thread; what is left is
        // the close and the free, legal from the joining thread.
        if (self.hasPark()) self.park.deinit();
        self.queue.deinit(gpa);
        self.free_cells.deinit(gpa);
        gpa.free(self.cells);
        gpa.destroy(self);
    }
};

// ---------------------------------------------------------------------------
// The worker thread.
// ---------------------------------------------------------------------------

fn workerMain(lane: *Lane) void {
    var name_buf: thread_name.CommBuf = undefined;
    _ = thread_name.nameCurrentThread(&name_buf, laneThreadName(lane.number));
    workerThreadLoop(lane);
    // EVERY exit path (normal drain, refused open, refused ring) marks the
    // watermark dead and drains the queue so no caller parks forever.
    lane.closing.store(true, .seq_cst);
    while (lane.inflight_submitters.load(.seq_cst) != 0) std.Thread.yield() catch {};
    lane.sealed.store(true, .release);
    drainQueueAtExit(lane);
    lane.wm.markDead();
}

/// Fail every still-queued request with the shutdown verdict.
fn drainQueueAtExit(lane: *Lane) void {
    while (lane.queue.tryPop()) |idx| answerUnrun(lane, idx, .shutdown);
}

/// Answer a request the ring will never complete (the exit drain, an
/// SQ-full refusal, a dead ring): writes and fsyncs balance the watermark,
/// the worker's buffer goes home, the outcome publishes.
fn answerUnrun(lane: *Lane, idx: u32, outcome: Outcome) void {
    const c = &lane.cells[idx];
    if (c.op != .read) lane.wm.noteCompleted();
    c.buf.release();
    c.buf = .empty;
    lane.complete(idx, outcome);
}

/// Runtime probe ladder, never a kernel-version table (charter §3):
/// `SINGLE_ISSUER` is free by construction (the worker thread that builds
/// the ring is its only submitter); `DEFER_TASKRUN` moves completion
/// task-work onto this thread's own `io_uring_enter` instead of async
/// IPI/task_work interrupts; `COOP_TASKRUN` is the milder variant. Each
/// refusal falls back one rung (`ParkedRing.initProbing`); the last rung is
/// a plain ring.
const ring_ladder = [_]struct { flags: u32, rung: []const u8 }{
    .{ .flags = linux.IORING_SETUP_SINGLE_ISSUER | linux.IORING_SETUP_DEFER_TASKRUN, .rung = "single_issuer+defer_taskrun" },
    .{ .flags = linux.IORING_SETUP_SINGLE_ISSUER | linux.IORING_SETUP_COOP_TASKRUN, .rung = "single_issuer+coop_taskrun" },
    .{ .flags = linux.IORING_SETUP_SINGLE_ISSUER, .rung = "single_issuer" },
    .{ .flags = 0, .rung = "plain" },
};

const ring_ladder_flags = blk: {
    var flags: [ring_ladder.len]u32 = undefined;
    for (ring_ladder, 0..) |rung, i| flags[i] = rung.flags;
    break :blk flags;
};

const Worker = struct {
    lane: *Lane,
    park: *ring_park.ParkedRing,
    fd: linux.fd_t,
    /// `register_files([fd])` succeeded: hot SQEs name fixed file 0.
    use_fixed_file: bool,
    /// The ranged slab is fixed buffer 0 on this ring.
    use_fixed_buf: bool,
    /// Cells whose SQE is in the ring (worker-private, by cell index): the
    /// dead-ring sweep's input, never read through a caller's reset.
    in_ring: []bool,
    active_count: usize = 0,
    disconnected: bool = false,
    /// The ring broke: every request answers `RingFailed` (`failInFlight`).
    ring_dead: bool = false,
    cqes: [256]linux.io_uring_cqe = undefined,

    /// The fd an SQE names: the fixed-file index or the raw fd.
    fn sqeFd(self: *const Worker) linux.fd_t {
        return if (self.use_fixed_file) 0 else self.fd;
    }

    fn markFixed(self: *const Worker, sqe: *linux.io_uring_sqe) void {
        if (self.use_fixed_file) sqe.flags |= linux.IOSQE_FIXED_FILE;
    }

    /// `get_sqe` with up to `sq_push_retries` SQ-full flushes: a plain
    /// `submit()` frees the SQ independently of completions (and under
    /// DEFER_TASKRUN is the one legal mid-pass enter — no GETEVENTS).
    fn getSqe(self: *Worker) ?*linux.io_uring_sqe {
        var attempt: u32 = 0;
        while (true) {
            if (self.park.ring.get_sqe()) |sqe| return sqe else |_| {}
            if (attempt == nd.sq_push_retries) return null;
            attempt += 1;
            stats.dev_enters.inc();
            _ = self.park.ring.submit() catch {};
        }
    }
};

fn sleepMs(ms: u64) void {
    const ts: linux.timespec = .{ .sec = @intCast(ms / 1000), .nsec = @intCast((ms % 1000) * std.time.ns_per_ms) };
    _ = linux.nanosleep(&ts, null);
}

fn workerThreadLoop(lane: *Lane) void {
    const dev = lane.dev;
    // O_DIRECT or nothing: buffered device I/O is not a supported data-plane
    // mode (DUR-2 — the barrier's durability rides the device's write cache,
    // not the page cache). `Device.open` probed this first, so a failure here
    // is a vanished node; every queued request fails via the exit drain.
    const fd_raw = linux.open(dev.path, .{ .ACCMODE = .RDWR, .DIRECT = true, .CLOEXEC = true }, 0);
    if (linux.errno(fd_raw) != .SUCCESS) {
        log.err("REFUSING data volume {s}: O_DIRECT open failed ({t}). Buffered device I/O is not a supported data-plane mode (DUR-2)", .{ dev.path, linux.errno(fd_raw) });
        lane.publishReady(.open_refused);
        return;
    }
    const fd: linux.fd_t = @intCast(fd_raw);
    defer _ = linux.close(fd);

    const in_ring = dev.gpa.alloc(bool, lane.cells.len) catch {
        log.err("NvmeBlockDev lane {d} ({s}): out of memory at start; the lane is dead", .{ lane.number, dev.path });
        lane.publishReady(.ring_failed);
        return;
    };
    defer dev.gpa.free(in_ring);
    @memset(in_ring, false);

    // The ring, its eventfd and the park — on THIS thread (SINGLE_ISSUER),
    // over the probe ladder. Freed by the closer after the join (`Lane.deinit`).
    var rung: usize = undefined;
    lane.park = ring_park.ParkedRing.initProbing(dev.gpa, nd.ring_entries, &ring_ladder_flags, &rung) catch {
        log.err("NvmeBlockDev lane {d} ({s}): io_uring setup failed; the lane is dead", .{ lane.number, dev.path });
        lane.publishReady(.ring_failed);
        return;
    };
    // At exit, still on this thread: cancel the standing READ and reap its
    // CQE, so nothing lands in the wake buffer after the lane is gone.
    defer lane.park.quiesce();
    log.debug("NvmeBlockDev lane {d} ({s}): ring rung {s}", .{ lane.number, dev.path, ring_ladder[rung].rung });

    // Fixed FILE: hot SQEs then name index 0 with IOSQE_FIXED_FILE (one fd
    // lookup/refcount per op saved). Refusal ⇒ plain fd.
    const use_fixed_file = lane.park.registerFile(fd);
    if (!use_fixed_file) log.debug("NvmeBlockDev lane {d}: register_files refused; using the raw fd", .{lane.number});

    // Fixed BUFFER: the ranged read-bounce slab as ONE registered buffer
    // (index 0), so slab-resident cold-fill DMAs ride ReadFixed and skip the
    // per-op page pin (gup) every unregistered DMA pays. A refusal (memlock
    // caps, old kernel) degrades to plain Read SQEs. The whole-block 4 MiB
    // pool is deliberately NOT registered: pinning cores×16×4 MiB (GiBs)
    // trades real RAM for a pin cost already amortized across 4 MiB DMAs.
    var iov = pools().ranged.iovec();
    const use_fixed_buf = if (lane.park.ring.register_buffers((&iov)[0..1])) |_| blk: {
        log.debug("NvmeBlockDev lane {d}: registered read-bounce slab ({d} MiB) as fixed buffer 0", .{ lane.number, iov.len >> 20 });
        break :blk true;
    } else |e| blk: {
        log.debug("NvmeBlockDev lane {d}: fixed-buffer register failed ({t}); ranged reads use plain Read", .{ lane.number, e });
        break :blk false;
    };

    var w: Worker = .{ .lane = lane, .park = &lane.park, .fd = fd, .use_fixed_file = use_fixed_file, .use_fixed_buf = use_fixed_buf, .in_ring = in_ring };
    lane.fixed_file = use_fixed_file;
    lane.fixed_buf = use_fixed_buf;
    lane.publishReady(.armed);
    workerPasses(&w);
}

/// One pass per iteration: disarm-if-consumed → re-arm the wake → pump the
/// queue (never blocks) → ONE park → reap.
fn workerPasses(w: *Worker) void {
    const lane = w.lane;
    const dev = lane.dev;
    while (true) {
        // Drain → disarm → scan: the worker half of the coalescer protocol.
        _ = w.park.service();
        if (takeArmed(&dev.seams.kill_ring)) {
            // The seam's loud line is declared on the thread that logs it.
            // The ring is healthy here, so the standing READ is cancelled
            // first: a dead park polls the eventfd itself.
            if (builtin.is_test) log.testing.expectErrorLogs(1);
            w.park.quiesce();
            w.park.markDead();
            failInFlight(w);
        }
        if (!w.ring_dead and !w.park.armWake()) stats.wake_arm_failed.fire("eventfd READ could not be queued after an SQ flush");

        // Sealed is read BEFORE the pump: every push ordered before the
        // seal (release/acquire through `sealed`) is visible to this scan.
        const sealed = lane.sealed.load(.acquire);
        var pass_fills: usize = 0;
        while (lane.queue.tryPop()) |idx| {
            if (w.ring_dead) {
                answerUnrun(lane, idx, .ring_failed);
                continue;
            }
            if (!pumpOne(w, idx)) break;
            w.active_count += 1;
            pass_fills += 1;
        }
        if (sealed) w.disconnected = true;
        if (w.disconnected and w.active_count == 0) break;
        holdIfArmed(&dev.seams);

        // THE enter (`ring_park.park`): flushes every SQE pushed this pass
        // (requests + the re-armed wake) and parks for the FIRST event of
        // either class. Untimed: the wake ends an idle park, a CQE a busy
        // one; the backstop tick exists only for the unarmed-wake case
        // (tripwired above). A dead ring parks on the eventfd alone.
        stats.dev_enters.inc();
        if (pass_fills > 0) stats.dev_submit_enters.inc();
        const tick: ?u64 = if (w.ring_dead or w.park.wake_armed) null else nd.wake_unarmed_backstop_ns;
        _ = w.park.park(tick);
        if (w.park.isDead() and !w.ring_dead) failInFlight(w);
        reap(w);
    }
}

/// The hold seam: park-adjacent, so a test can place N publishes between
/// two lane passes (`Seams.hold_before_park`).
fn holdIfArmed(seams: *Seams) void {
    if (seams.hold_before_park.load(.acquire) == 0) return;
    seams.held.store(1, .release);
    futex.wakeAll(&seams.held);
    while (seams.hold_before_park.load(.acquire) != 0) futex.wait(&seams.hold_before_park, 1);
}

/// The ring broke (the park found its enter failing, or the seam killed
/// it): every request IN the ring fails with `RingFailed` — once, here,
/// loud — and every later one at the pump; the lane stays up to answer
/// them (charter §1.2: no classical fallback; §1.8: fail loud). Nothing on
/// a dead ring completes, so its CQEs are never read again.
fn failInFlight(w: *Worker) void {
    const lane = w.lane;
    w.ring_dead = true;
    log.err("NvmeBlockDev lane {d} ({s}): the ring is dead — failing {d} in-flight ops and every later request on this lane (RingFailed; charter §1.2: no classical fallback)", .{ lane.number, lane.dev.path, w.active_count });
    for (w.in_ring, 0..) |*in_ring, i| {
        if (!in_ring.*) continue;
        in_ring.* = false;
        answerUnrun(lane, @intCast(i), .ring_failed);
    }
    w.active_count = 0;
}

/// Build and push one request's SQE(s). False ⇒ the request was refused
/// (SQ full after retries) and the pump must break; remaining queue items
/// wait for the next pass.
fn pumpOne(w: *Worker, idx: u32) bool {
    const lane = w.lane;
    const dev = lane.dev;
    const c = &lane.cells[idx];
    var slow_link_ms: ?u64 = null;
    switch (c.op) {
        .read => {
            if (takeArmed(&dev.seams.stall_reads)) sleepMs(dev.seams.stall_read_ms.load(.monotonic));
            if (takeArmed(&dev.seams.slow_reads)) slow_link_ms = dev.seams.slow_read_ms.load(.monotonic);
            // ONE clock read stamps the queue phase and the service start.
            c.submitted_ns = clock.monotonicNs();
            stats.read_fill_dev_queue_ns.record(c.submitted_ns -| c.enq_ns.load(.monotonic));
        },
        .write => {
            if (takeArmed(&dev.seams.stall_writes)) sleepMs(dev.seams.stall_write_ms.load(.monotonic));
            c.submitted_ns = clock.monotonicNs();
            stats.write_pipeline_dev_queue_ns.record(c.submitted_ns -| c.enq_ns.load(.monotonic));
        },
        .fsync, .discard => {},
    }

    // Slow-read seam: a linked TIMEOUT (ETIME counts as success so the link
    // survives) ahead of the read holds the lane for exactly the window.
    if (slow_link_ms) |ms| {
        if (w.getSqe()) |tsqe| {
            const S = struct {
                threadlocal var ts: linux.kernel_timespec = undefined;
            };
            S.ts = .{ .sec = @intCast(ms / 1000), .nsec = @intCast((ms % 1000) * std.time.ns_per_ms) };
            tsqe.prep_timeout(&S.ts, 0, linux.IORING_TIMEOUT_ETIME_SUCCESS);
            tsqe.flags |= linux.IOSQE_IO_LINK;
            tsqe.user_data = nd.UD_SEAM;
        }
    }

    stats.dev_fills.inc();
    const sqe = w.getSqe() orelse {
        // Queue-full refusal: terminal for this request; writes/fsyncs
        // balance the watermark here.
        log.err("NvmeBlockDev lane {d} ({s}): submission queue full after {d} flushes; refusing a {t} at offset {d}", .{ lane.number, dev.path, nd.sq_push_retries, c.op, c.offset });
        answerUnrun(lane, idx, .sq_full);
        return false;
    };
    switch (c.op) {
        .read => {
            // ReadFixed ONLY for a slab-resident buffer: the kernel refuses
            // a fixed read outside the registered range.
            const in_slab = w.use_fixed_buf and pools().ranged.contains(c.dma_ptr, c.dma_len);
            if (in_slab) {
                var iov: std.posix.iovec = .{ .base = c.dma_ptr, .len = c.dma_len };
                sqe.prep_read_fixed(w.sqeFd(), &iov, c.offset, 0);
                stats.dev_fixed_buf_fills.inc();
            } else {
                sqe.prep_read(w.sqeFd(), c.dma_ptr[0..c.dma_len], c.offset);
            }
        },
        // Writes never use WriteFixed (the sources are caller slices and
        // whole-block pool buffers — neither registered).
        .write => sqe.prep_write(w.sqeFd(), c.dma_ptr[0..c.dma_len], c.offset),
        // Never `fsync(2)`: O_DIRECT bypasses the page cache, NOT the
        // device's volatile write cache; the ring Fsync with DATASYNC is
        // the DUR-2 barrier (a plain fsync is representable, no caller
        // wants one).
        .fsync => sqe.prep_fsync(w.sqeFd(), linux.IORING_FSYNC_DATASYNC),
        .discard => switch (dev.substrate) {
            // A regular file: the host FS returns the range's space.
            .file_punch => sqe.prep_fallocate(w.sqeFd(), linux.FALLOC.FL_PUNCH_HOLE | linux.FALLOC.FL_KEEP_SIZE, c.offset, c.dma_len),
            // A block device: `BLOCK_URING_CMD_DISCARD` — `cmd_op` rides
            // `off`, the range `addr`/`addr3` (block/ioctl.c
            // `blkdev_uring_cmd`); `len`, `rw_flags`, `ioprio` must be 0.
            .bdev_discard => {
                sqe.prep_rw(.URING_CMD, w.sqeFd(), 0, 0, 0);
                sqe.off = BLOCK_URING_CMD_DISCARD;
                sqe.addr = c.offset;
                sqe.addr3 = c.dma_len;
            },
            // `discardRange` refused the class before the cell existed.
            .skip => unreachable,
        },
    }
    w.markFixed(sqe);
    sqe.user_data = idx;
    w.in_ring[idx] = true;
    return true;
}

/// Reap every ready CQE after the pass's park. A dead ring's CQEs are never
/// read: its in-flight ops were already answered.
fn reap(w: *Worker) void {
    if (w.ring_dead) return;
    const lane = w.lane;
    var completed: usize = 0;
    while (true) {
        const n = w.park.ring.copy_cqes(&w.cqes, 0) catch 0;
        if (n == 0) break;
        for (w.cqes[0..n]) |cqe| {
            if (w.park.consumeWake(cqe)) continue;
            if (cqe.user_data >= nd.UD_RESERVED) continue;
            if (cqe.user_data >= lane.cells.len or !w.in_ring[cqe.user_data]) {
                stats.cqe_orphan_slot.fire("CQE named a slot with no in-flight request");
                continue;
            }
            completeOne(w, @intCast(cqe.user_data), cqe.res);
            completed += 1;
        }
    }
    if (completed > 0) {
        stats.dev_wake_batches.inc();
        w.active_count -= completed;
    }
}

/// ORDER MATTERS: service phase → watermark (BEFORE the completion, so
/// barrier drains never depend on caller scheduling) → release what the
/// worker owns → publish.
fn completeOne(w: *Worker, idx: u32, res: i32) void {
    const lane = w.lane;
    const c = &lane.cells[idx];
    w.in_ring[idx] = false;
    switch (c.op) {
        .read => {
            stats.read_fill_dev_service_ns.record(clock.monotonicNs() -| c.submitted_ns);
            // The read buffer travels to the parked caller (MEM-1: the
            // worker owns the DMA destination until this CQE, the caller
            // from here).
            lane.complete(idx, .{ .io = res });
        },
        .write => {
            stats.write_pipeline_dev_service_ns.record(clock.monotonicNs() -| c.submitted_ns);
            lane.wm.noteCompleted();
            c.buf.release();
            c.buf = .empty;
            lane.complete(idx, .{ .io = res });
        },
        .fsync, .discard => {
            lane.wm.noteCompleted();
            lane.complete(idx, .{ .io = res });
        },
    }
}

/// `_IO(0x12, 0)` (uapi `linux/blkdev.h`, io_uring discard since 6.12): the
/// block device's uring command for a DSM Deallocate of `[addr, addr + addr3)`.
pub const BLOCK_URING_CMD_DISCARD: u64 = linux.IOCTL.IO(0x12, 0);

// ---------------------------------------------------------------------------
// Device probing (spec §3).
// ---------------------------------------------------------------------------

/// `seek(End)`: works for a regular file and a block device (a block
/// device's stat size is 0). No BLKGETSIZE64/statx.
pub fn capacityBytes(path: [:0]const u8) DeviceError!u64 {
    const fd_raw = linux.open(path, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
    if (linux.errno(fd_raw) != .SUCCESS) return nd.errnoToError(linux.errno(fd_raw));
    const fd: linux.fd_t = @intCast(fd_raw);
    defer _ = linux.close(fd);
    const end = linux.lseek(fd, 0, linux.SEEK.END);
    if (linux.errno(end) != .SUCCESS) return nd.errnoToError(linux.errno(end));
    return end;
}

/// Open `read|write|O_DIRECT`, close; failure ⇒ LOUD refusal so a mount
/// fails at setup, not at first I/O.
pub fn probeDirectIo(path: [:0]const u8) Refusal!void {
    const fd_raw = linux.open(path, .{ .ACCMODE = .RDWR, .DIRECT = true, .CLOEXEC = true }, 0);
    if (linux.errno(fd_raw) != .SUCCESS) {
        log.err("data volume {s} cannot be opened with O_DIRECT ({t}); buffered device I/O is not a supported data-plane mode (DUR-2: the barrier's durability rides the device write cache, not the page cache). Use a substrate that supports direct I/O.", .{ path, linux.errno(fd_raw) });
        return error.DirectIoUnsupported;
    }
    _ = linux.close(@intCast(fd_raw));
}

fn readSmallFile(path: [:0]const u8, buf: []u8) ?[]const u8 {
    const fd_raw = linux.open(path, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
    if (linux.errno(fd_raw) != .SUCCESS) return null;
    const fd: linux.fd_t = @intCast(fd_raw);
    defer _ = linux.close(fd);
    const n = linux.read(fd, buf.ptr, buf.len);
    if (linux.errno(n) != .SUCCESS) return null;
    return buf[0..n];
}

/// Write-cache class, probed ONCE at open, never on a cadence. Regular file
/// ⇒ file-backed; block device ⇒ `/sys/dev/block/M:m/queue/write_cache`
/// (partition → parent disk); anything unreadable ⇒ unknown. Informational
/// only — the barrier is what makes writes safe (a write-through device
/// pays nothing). The NVMe identify VWC bit is deliberately not probed
/// (needs CAP_SYS_ADMIN; sysfs honours the operator's override).
pub fn probeWriteCache(path: [:0]const u8) nd.WriteCacheClass {
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(linux.AT.FDCWD, path, 0, .{ .TYPE = true }, &st)) != .SUCCESS) return .unknown;
    if (linux.S.ISREG(st.mode)) return .file_backed;
    if (!linux.S.ISBLK(st.mode)) return .unknown;
    var pbuf: nd.SysfsPathBuf = undefined;
    var text: [64]u8 = undefined;
    inline for (.{ nd.SysfsQueueDir.own, nd.SysfsQueueDir.parent }) |which| {
        if (readSmallFile(nd.sysfsWriteCachePath(&pbuf, st.rdev_major, st.rdev_minor, which), &text)) |t| return nd.WriteCacheClass.fromSysfs(t);
    }
    return .unknown;
}

/// The reclaim substrate, classified ONCE at open from `st_mode` (a regular
/// file punches, a block device discards, anything else is never written
/// to zero): the reclaim module reads it through `Device.reclaimOp` (Q5 —
/// no `stat` per batch, no re-open per group).
pub fn probeSubstrate(path: [:0]const u8) reclaim.Substrate {
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(linux.AT.FDCWD, path, 0, .{ .TYPE = true }, &st)) != .SUCCESS) return .skip;
    return reclaim.classifySubstrate(st.mode);
}

// ---------------------------------------------------------------------------
// The device.
// ---------------------------------------------------------------------------

/// Knob range ceiling of `SQUEEZEFS_NVME_*_LANES` — the pool's static bound.
pub const max_lanes: u32 = 1024;

/// Spawn a lane and wait for its arming handshake; a lane that failed to
/// arm is torn down here and the failure surfaces to the opener.
fn spawnArmedLane(gpa: std.mem.Allocator, dev: *Device) !*Lane {
    const lane = try Lane.spawn(gpa, dev);
    lane.awaitArmed() catch |e| {
        lane.deinit(gpa);
        return e;
    };
    return lane;
}

pub const ReadOutcome = union(enum) {
    full: Buf,
    /// Bytes the kernel returned (the buffer went home).
    short: usize,
};

pub const Device = struct {
    gpa: std.mem.Allocator,
    path: [:0]const u8,
    /// Lane 0 is always present at index 0; the pool only grows.
    lanes: [max_lanes]?*Lane = @splat(null),
    /// Published with release after the lane pointers are stored; readers
    /// load it with acquire and index below it.
    lane_count: std.atomic.Value(u32) = .init(0),
    /// Growth is a control-plane op (mount-time arm): a CAS spin lock
    /// serializes concurrent growers so no winner's pointer is overwritten.
    grow_lock: std.atomic.Value(bool) = .init(false),
    lane_rr: std.atomic.Value(usize) = .init(0),
    read_lanes: std.atomic.Value(u32) = .init(1),
    write_lanes: std.atomic.Value(u32) = .init(1),
    write_grain: std.atomic.Value(u64) = .init(0),
    node_probe: nd.NodeProbe = .{},
    fence: Fence = .{},
    seams: Seams = .{},
    write_cache: nd.WriteCacheClass,
    /// `probeSubstrate` at open: the discard command's shape.
    substrate: reclaim.Substrate,
    /// The daemon-lifetime O_DIRECT fd the zero-copy legs name on a FOREIGN
    /// ring (`READ_FIXED(device → slot)`, `WRITE_FIXED(device ← slot)` on
    /// the FUSE queue ring that registered the slot): a lane's own fd is
    /// that lane's (fixed file 0 on its ring) and never leaves it.
    direct_fd: linux.fd_t,
    closed: bool = false,

    /// `open_checked`: `probeDirectIo` FIRST (loud refusal), spawn lane 0,
    /// probe the write cache once, log the class.
    pub fn open(gpa: std.mem.Allocator, path: [:0]const u8) (DeviceError || std.mem.Allocator.Error || std.Thread.SpawnError || error{SystemResources})!*Device {
        try probeDirectIo(path);
        try initPools();
        const dev = try gpa.create(Device);
        errdefer gpa.destroy(dev);
        const owned = try gpa.dupeSentinel(u8, path, 0);
        errdefer gpa.free(owned);
        const direct_raw = linux.open(path, .{ .ACCMODE = .RDWR, .DIRECT = true, .CLOEXEC = true }, 0);
        if (linux.errno(direct_raw) != .SUCCESS) return error.DirectOpenRefused;
        const direct_fd: linux.fd_t = @intCast(direct_raw);
        errdefer _ = linux.close(direct_fd);
        dev.* = .{
            .gpa = gpa,
            .path = owned,
            .write_cache = probeWriteCache(path),
            .substrate = probeSubstrate(path),
            .direct_fd = direct_fd,
        };
        dev.lanes[0] = try spawnArmedLane(gpa, dev);
        dev.lane_count.store(1, .release);
        log.info("data volume {s}: write cache {s}", .{ path, dev.write_cache.name() });
        return dev;
    }

    /// The fd of the zero-copy legs (module field doc). Valid until `deinit`.
    pub fn directFd(self: *const Device) linux.fd_t {
        return self.direct_fd;
    }

    /// Stop every lane (idempotent): close queues → wake → join. Callers
    /// still parked get `WorkerShutdown`; later submits get `QueueFull`.
    pub fn close(self: *Device) void {
        if (self.closed) return;
        self.closed = true;
        const n = self.lane_count.load(.acquire);
        for (self.lanes[0..n]) |l| l.?.shutdown();
    }

    pub fn deinit(self: *Device) void {
        self.close();
        const n = self.lane_count.load(.acquire);
        for (self.lanes[0..n]) |l| l.?.deinit(self.gpa);
        _ = linux.close(self.direct_fd);
        self.gpa.free(self.path);
        self.gpa.destroy(self);
    }

    // ---- lanes (spec §1.1, §1.2) ----

    pub fn laneCount(self: *const Device) u32 {
        return self.lane_count.load(.acquire);
    }

    pub fn laneNumber(self: *const Device, idx: u32) u32 {
        return self.lanes[idx].?.number;
    }

    /// Lane 0's registration outcomes: `(fixed file, fixed buffer)`.
    pub fn registrations(self: *const Device) struct { fixed_file: bool, fixed_buf: bool } {
        const l = self.lanes[0].?;
        return .{ .fixed_file = l.fixed_file, .fixed_buf = l.fixed_buf };
    }

    /// Monotone, idempotent growth to `n` lanes.
    fn growPool(self: *Device, n: u32) !void {
        const target = @min(n, max_lanes);
        while (self.grow_lock.swap(true, .acquire)) std.atomic.spinLoopHint();
        defer self.grow_lock.store(false, .release);
        var cur = self.lane_count.load(.acquire);
        while (cur < target) : (cur += 1) {
            self.lanes[cur] = try spawnArmedLane(self.gpa, self);
            self.lane_count.store(cur + 1, .release);
        }
    }

    pub fn setReadLanes(self: *Device, n: u32) !void {
        const want = @max(n, 1);
        try self.growPool(want);
        if (self.read_lanes.fetchMax(want, .monotonic) < want) log.info("data volume {s}: read lanes → {d}", .{ self.path, want });
    }

    pub fn setWriteLanes(self: *Device, n: u32, grain: u64) !void {
        const want = @max(n, 1);
        try self.growPool(want);
        if (self.write_lanes.fetchMax(want, .monotonic) < want) log.info("data volume {s}: write lanes → {d} (grain {d})", .{ self.path, want, grain });
        self.setWriteLaneGrain(grain);
    }

    pub fn setWriteLaneGrain(self: *Device, grain: u64) void {
        self.write_grain.store(@max(grain, 1), .monotonic);
    }

    pub fn readLanes(self: *const Device) u32 {
        return self.read_lanes.load(.monotonic);
    }

    pub fn writeLanes(self: *const Device) u32 {
        return self.write_lanes.load(.monotonic);
    }

    /// Per pool lane `(submitted, completed)` — the `data_write_lane_submits`
    /// stats face: a fan-out row is invalid unless > 1 lane moved.
    pub fn writeLaneWatermarks(self: *const Device, out: []Watermark) []Watermark {
        const n = @min(self.lane_count.load(.acquire), out.len);
        for (out[0..n], 0..) |*o, i| o.* = self.lanes[i].?.wm.snapshot();
        return out[0..n];
    }

    fn readLanePick(self: *Device) *Lane {
        const count = self.lane_count.load(.acquire);
        const lanes = @max(@min(self.read_lanes.load(.monotonic), count), 1);
        const idx = self.lane_rr.fetchAdd(1, .monotonic) % lanes;
        return self.lanes[idx].?;
    }

    /// `write_lanes <= 1` ⇒ lane 0 (byte-identical to the pre-fanout
    /// posture); else the block-offset affinity lane.
    fn writeLanePick(self: *Device, offset: u64) *Lane {
        const wl = self.write_lanes.load(.monotonic);
        if (wl <= 1) return self.lanes[0].?;
        const count = self.lane_count.load(.acquire);
        const lanes = @max(@min(wl, count), 1);
        return self.lanes[nd.writeLaneIndex(offset, self.write_grain.load(.monotonic), lanes)].?;
    }

    // ---- fence / dismount (spec §5) ----

    pub fn setFenceSignal(self: *Device, sig: *const FenceSignal) void {
        _ = self.fence.signal.cmpxchgStrong(null, sig, .release, .monotonic);
    }

    /// May latch.
    pub fn fenced(self: *Device) bool {
        return self.fence.isHalted(self.path);
    }

    pub fn closeAtDismount(self: *Device) void {
        if (!self.fence.dismounted.swap(true, .acq_rel)) log.info("data volume {s}: dismount teardown completed; further DMA refused (EROFS class)", .{self.path});
    }

    pub fn dismounted(self: *const Device) bool {
        return self.fence.dismounted.load(.acquire);
    }

    /// Run by every write-class entry (`writeBlock`, `flush`), NEVER by
    /// reads. Dismount is judged AFTER the fence so a fenced holder keeps
    /// its verdict.
    fn fenceGate(self: *Device) Refusal!void {
        if (self.fence.isHalted(self.path)) poisonCustody("data volume writer guard fenced");
        try authorizeDma();
        if (self.fence.dismounted.load(.monotonic)) {
            stats.data_dma_dismount_refusals.inc();
            return error.DismountRefused;
        }
    }

    /// The ONE write-outcome funnel (write completion, barrier completion,
    /// injected failure): EBADE latches the fence and poisons custody; the
    /// error itself passes through UNCHANGED (callers still see the class).
    /// Every other error passes untouched — a transient hiccup never
    /// self-inflicts a fail-stop.
    fn classifyDmaOutcome(self: *Device, e: linux.E) void {
        if (!nd.isReservationConflict(e)) return;
        if (!self.fence.halted.swap(true, .acq_rel)) {
            log.err("DEVICE {s} rejected a DMA with the reservation-conflict errno class (EBADE/os error 52): another writer holds the persistent reservation — writer guard FENCED; refusing ALL further data-plane DMA permanently (DLM S7; remount required)", .{self.path});
        }
        poisonCustody("device reservation conflict on data volume");
    }

    // ---- probes ----

    pub fn writeCache(self: *const Device) nd.WriteCacheClass {
        return self.write_cache;
    }

    /// TTL-cached `exists(path)` (1 s).
    pub fn nodeExistsCached(self: *Device) bool {
        const now_ms = clock.monotonicNs() / std.time.ns_per_ms;
        if (self.node_probe.cached(now_ms)) |seen| return seen;
        const seen = linux.errno(linux.access(self.path, linux.F_OK)) == .SUCCESS;
        self.node_probe.record(now_ms, seen);
        return seen;
    }

    /// The watchdog's census (charter §4 "a deadline watchdog that logs
    /// overdue ops loudly"): every op submitted and not yet completed on
    /// any lane, those older than `bound_ns`, and the oldest's age — a
    /// sample over the cells' submit stamps, taken from any thread. The
    /// watchdog (rung 3) owns the bounds and the escalation (log, then the
    /// volume fence through the lattice); nothing here times an op out.
    pub fn overdueCensus(self: *const Device, now_ns: u64, bound_ns: u64) nd.OverdueCensus {
        var out: nd.OverdueCensus = .{};
        const n = self.lane_count.load(.acquire);
        for (self.lanes[0..n]) |l| {
            for (l.?.cells) |*c| {
                const since = c.enq_ns.load(.monotonic);
                if (since == 0) continue;
                out.note(now_ns -| since, bound_ns);
            }
        }
        return out;
    }

    /// Per-device stats face: `data_volume_write_cache`, lane gauges and
    /// per-lane `submitted` watermarks.
    pub fn writeStatsFaceJson(self: *const Device, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("{{\"data_volume_write_cache\":\"{s}={s}\",\"data_read_lanes\":{d},\"data_write_lanes\":{d},\"data_write_lane_submits\":\"{s}=", .{ self.path, self.write_cache.name(), self.readLanes(), self.writeLanes(), self.path });
        const n = self.lane_count.load(.acquire);
        for (self.lanes[0..n], 0..) |l, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("{d}", .{l.?.wm.submitted.load(.seq_cst)});
        }
        try w.writeAll("\"}");
    }

    // ---- test seams ----

    pub fn setFailNextWritesErrno(self: *Device, n: u32, errno: linux.E) void {
        self.seams.fail_errno.store(@backingInt(errno), .seq_cst);
        self.seams.fail_next_writes.store(n, .seq_cst);
    }

    pub fn setTestReadStall(self: *Device, count: u32, ms: u64) void {
        self.seams.stall_read_ms.store(ms, .seq_cst);
        self.seams.stall_reads.store(count, .seq_cst);
    }

    pub fn setTestWriteStall(self: *Device, count: u32, ms: u64) void {
        self.seams.stall_write_ms.store(ms, .seq_cst);
        self.seams.stall_writes.store(count, .seq_cst);
    }

    pub fn setTestReadSlow(self: *Device, count: u32, ms: u64) void {
        self.seams.slow_read_ms.store(ms, .seq_cst);
        self.seams.slow_reads.store(count, .seq_cst);
    }

    // ---- writes (spec §2.1, §2.2) ----

    /// `write_block`: any offset, any length; the device enforces O_DIRECT
    /// alignment on the offset, the code on the buffer. A 4 KiB-aligned
    /// pointer AND length is the zero-copy branch (the caller's slice is
    /// the keep-alive — it is parked for the DMA); anything else bounces
    /// through a pooled 4 KiB-padded copy whose zero tail is WRITTEN.
    pub fn writeBlock(self: *Device, offset: u64, data: []const u8) DeviceError!void {
        // Injected failure, through the funnel (so an EBADE injection
        // latches the fence exactly like a device verdict).
        if (takeArmed(&self.seams.fail_next_writes)) {
            const e: linux.E = @fromBackingInt(@intCast(@as(u16, @intCast(self.seams.fail_errno.load(.seq_cst)))));
            if (e == .SUCCESS) return error.InputOutput;
            self.classifyDmaOutcome(e);
            return nd.errnoToError(e);
        }
        try self.fenceGate();
        // A zero-length DMA is a no-op by definition; never hand the kernel
        // a 0-byte SQE (the zero-length ReadFixed oops class, §2.1).
        if (data.len == 0) return;

        const lane = self.writeLanePick(offset);
        var bounce: Buf = .empty;
        var dma: []u8 = undefined;
        if (nd.isDmaAligned(data)) {
            dma = @constCast(data);
        } else {
            stats.nvme_unaligned_write_fallbacks.inc();
            const padded = nd.paddedLen(data.len);
            bounce = if (padded <= nd.block_buf_size) try pools().block.acquire(padded) else try mapAdhoc(padded);
            dma = bounce.ptr[0..padded];
            @memcpy(dma[0..data.len], data);
            @memset(dma[data.len..], 0);
        }
        errdefer bounce.release();

        const idx = lane.acquireCell() orelse {
            stats.uring_queue_full.inc();
            return error.QueueFull;
        };
        const c = &lane.cells[idx];
        c.op = .write;
        c.offset = offset;
        c.dma_ptr = dma.ptr;
        c.dma_len = dma.len;
        c.buf = bounce;
        c.enq_ns.store(clock.monotonicNs(), .monotonic);
        // Balanced by `noteCompleted` on every terminal outcome, including a
        // refused submit.
        lane.wm.noteSubmitted();
        if (!lane.submit(idx)) {
            lane.wm.noteCompleted();
            lane.freeCell(idx);
            stats.uring_queue_full.inc();
            return error.QueueFull;
        }
        // Ownership of `bounce` moved to the worker (released at the CQE).
        bounce = .empty;

        // NO timeout; the census names a lost completion.
        stats.device_lock_waiters.add(1);
        defer stats.device_lock_waiters.sub(1);
        const outcome = lane.wait(idx);
        defer lane.freeCell(idx);
        const res = switch (outcome) {
            .shutdown => return error.WorkerShutdown,
            .sq_full => return error.SubmissionQueueOverflow,
            .ring_failed => return error.RingFailed,
            .io => |res| res,
        };
        if (res < 0) {
            const e: linux.E = @fromBackingInt(@intCast(@as(u16, @intCast(-res))));
            self.classifyDmaOutcome(e);
            return nd.errnoToError(e);
        }
        if (@as(usize, @intCast(res)) != dma.len) return error.ShortWrite;
        stats.put_obj.inc();
    }

    // ---- reads (spec §2.1, §2.3) ----

    /// `read_block`: a pooled read of exactly `size` bytes (≤ 4 MiB). The
    /// returned buffer is the caller's to `release()`.
    pub fn readBlock(self: *Device, offset: u64, size: usize) DeviceError!Buf {
        return switch (try self.readBlockOutcome(offset, size, true)) {
            .full => |b| b,
            .short => error.ShortRead,
        };
    }

    /// Same I/O, NOT counted in `get_obj`: a control-plane probe completing
    /// behind data I/O inflated counter-window gates (the 2026-07-26 flake
    /// class).
    pub fn probeReadBlock(self: *Device, offset: u64, size: usize) DeviceError!Buf {
        return switch (try self.readBlockOutcome(offset, size, false)) {
            .full => |b| b,
            .short => error.ShortRead,
        };
    }

    /// The read with its exact-length verdict exposed: the ONE sanctioned
    /// prefix-tolerant consumer (the router's old-image read) needs `got`.
    pub fn readBlockOutcome(self: *Device, offset: u64, size: usize, count_in_get_obj: bool) DeviceError!ReadOutcome {
        // Zero-length guard: on sqz kernel 7.1.8 a 0-len ReadFixed against
        // the registered slab oopsed (`iov_iter_alignment_bvec` NULL deref
        // via `io_read_fixed` → `btrfs_direct_read`), SIGKILLing the worker
        // thread alone and stranding the lane. Never submit it.
        if (size == 0) return .{ .full = Buf.empty };
        if (size > nd.block_buf_size) return error.ReadSizeExceedsPool;
        // Reads are NOT fence-gated: a fenced holder reading its own device
        // corrupts nothing.
        var buf = try acquireReadBuf(size);
        errdefer buf.release();
        const lane = self.readLanePick();
        const idx = lane.acquireCell() orelse {
            stats.uring_queue_full.inc();
            return error.QueueFull;
        };
        const c = &lane.cells[idx];
        c.op = .read;
        c.offset = offset;
        c.dma_ptr = buf.ptr;
        c.dma_len = size;
        c.buf = buf;
        c.enq_ns.store(clock.monotonicNs(), .monotonic);
        if (!lane.submit(idx)) {
            lane.freeCell(idx);
            stats.uring_queue_full.inc();
            return error.QueueFull;
        }
        // The worker owns `buf` from here until the CQE.
        buf = .empty;

        // Unbounded (charter §4: no per-op timeout); a read the device never
        // answers sits in `overdueCensus` for the watchdog.
        const outcome = lane.wait(idx);
        defer lane.freeCell(idx);
        const got_buf = c.buf;
        c.buf = .empty;
        const res = switch (outcome) {
            .shutdown => {
                got_buf.release();
                return error.WorkerShutdown;
            },
            .sq_full => {
                got_buf.release();
                return error.SubmissionQueueOverflow;
            },
            .ring_failed => {
                got_buf.release();
                return error.RingFailed;
            },
            .io => |res| res,
        };
        if (res < 0) {
            got_buf.release();
            const e: linux.E = @fromBackingInt(@intCast(@as(u16, @intCast(-res))));
            // A device ERROR (not a delay) classifies through the one
            // funnel: an EBADE on a read is the same reservation conflict.
            self.classifyDmaOutcome(e);
            return nd.errnoToError(e);
        }
        switch (nd.finishRead(@intCast(res), size)) {
            .full => {
                if (count_in_get_obj) stats.get_obj.inc();
                return .{ .full = got_buf };
            },
            .short => |got| {
                log.debug("short read: kernel returned {d} of {d} bytes at offset {d} on {s} (past-EOF/short reads must not surface recycled buffer bytes)", .{ got, size, offset, self.path });
                got_buf.release();
                return .{ .short = got };
            },
        }
    }

    /// The caller-dest read (rung 4's ranged window and whole-block fill
    /// into the read fill pool / the registered reply window): exactly
    /// `dest.len` bytes at `offset` DMA'd into `dest` — a 4 KiB-aligned
    /// pointer, length and offset (the O_DIRECT grain; refused otherwise).
    /// No bounce, no pool buffer (`Buf.Kind.none`): the caller's slice is
    /// the keep-alive, parked for the whole op. Counted in `get_obj`.
    pub fn readInto(self: *Device, offset: u64, dest: []u8) DeviceError!void {
        if (dest.len == 0) return;
        if (!nd.isDmaAligned(dest) or !derive.isAligned(offset, nd.dma_align)) return error.UnalignedDirectWrite;
        const lane = self.readLanePick();
        const idx = lane.acquireCell() orelse {
            stats.uring_queue_full.inc();
            return error.QueueFull;
        };
        const c = &lane.cells[idx];
        c.op = .read;
        c.offset = offset;
        c.dma_ptr = dest.ptr;
        c.dma_len = dest.len;
        c.buf = .empty;
        c.enq_ns.store(clock.monotonicNs(), .monotonic);
        if (!lane.submit(idx)) {
            lane.freeCell(idx);
            stats.uring_queue_full.inc();
            return error.QueueFull;
        }
        const outcome = lane.wait(idx);
        defer lane.freeCell(idx);
        const res = switch (outcome) {
            .shutdown => return error.WorkerShutdown,
            .sq_full => return error.SubmissionQueueOverflow,
            .ring_failed => return error.RingFailed,
            .io => |res| res,
        };
        if (res < 0) {
            const e: linux.E = @fromBackingInt(@intCast(@as(u16, @intCast(-res))));
            self.classifyDmaOutcome(e);
            return nd.errnoToError(e);
        }
        switch (nd.finishRead(@intCast(res), dest.len)) {
            .full => stats.get_obj.inc(),
            .short => |got| {
                log.debug("short read: kernel returned {d} of {d} bytes at offset {d} on {s}", .{ got, dest.len, offset, self.path });
                return error.ShortRead;
            },
        }
    }

    // ---- flush / barrier (spec §4) ----

    /// The DUR-2 barrier: drain every write lane's watermark, then ONE
    /// `IORING_OP_FSYNC|IORING_FSYNC_DATASYNC` on lane 0. No coalescing yet
    /// (the metadata plane's `SyncCoalescer` wraps this later), so
    /// `data_device_sync_requests == data_device_syncs` for now.
    pub fn flush(self: *Device) DeviceError!void {
        stats.data_device_sync_requests.inc();
        try self.fenceGate();
        stats.data_device_syncs.inc();
        try self.drainWriteLanes();

        const lane = self.lanes[0].?;
        const idx = lane.acquireCell() orelse {
            stats.uring_queue_full.inc();
            return error.QueueFull;
        };
        const c = &lane.cells[idx];
        c.op = .fsync;
        lane.wm.noteSubmitted();
        if (!lane.submit(idx)) {
            lane.wm.noteCompleted();
            lane.freeCell(idx);
            stats.uring_queue_full.inc();
            return error.QueueFull;
        }
        stats.device_lock_waiters.add(1);
        defer stats.device_lock_waiters.sub(1);
        const outcome = lane.wait(idx);
        defer lane.freeCell(idx);
        const res = switch (outcome) {
            .shutdown => return error.WorkerShutdown,
            .sq_full => return error.SubmissionQueueOverflow,
            .ring_failed => return error.RingFailed,
            .io => |res| res,
        };
        if (res < 0) {
            // A barrier is a write-class command: EBADE latches the fence.
            const e: linux.E = @fromBackingInt(@intCast(@as(u16, @intCast(-res))));
            self.classifyDmaOutcome(e);
            return nd.errnoToError(e);
        }
    }

    // ---- discard (the reclaim module's command) ----

    /// The substrate class `probeSubstrate` found at open.
    pub fn reclaimOp(self: *const Device) reclaim.Substrate {
        return self.substrate;
    }

    /// ONE discard command over `[offset, offset + len)`: the punch or the
    /// DSM Deallocate per the substrate (a `.skip` substrate refuses
    /// `NotSupported` — never a zeroing write). Write-class: fence-gated,
    /// on the block-offset write lane, counted on its watermark (a barrier
    /// drains it like any write), its CQE through `classifyDmaOutcome`.
    /// The device's logical-block alignment of the range is the caller's
    /// (blocks are whole multiples of it).
    pub fn discardRange(self: *Device, offset: u64, len: u64) DeviceError!void {
        if (self.substrate == .skip) return error.NotSupported;
        try self.fenceGate();
        if (len == 0) return;
        const lane = self.writeLanePick(offset);
        const idx = lane.acquireCell() orelse {
            stats.uring_queue_full.inc();
            return error.QueueFull;
        };
        const c = &lane.cells[idx];
        c.op = .discard;
        c.offset = offset;
        c.dma_len = @intCast(len);
        c.enq_ns.store(clock.monotonicNs(), .monotonic);
        lane.wm.noteSubmitted();
        if (!lane.submit(idx)) {
            lane.wm.noteCompleted();
            lane.freeCell(idx);
            stats.uring_queue_full.inc();
            return error.QueueFull;
        }
        stats.device_lock_waiters.add(1);
        defer stats.device_lock_waiters.sub(1);
        const outcome = lane.wait(idx);
        defer lane.freeCell(idx);
        const res = switch (outcome) {
            .shutdown => return error.WorkerShutdown,
            .sq_full => return error.SubmissionQueueOverflow,
            .ring_failed => return error.RingFailed,
            .io => |res| res,
        };
        if (res < 0) {
            const e: linux.E = @fromBackingInt(@intCast(@as(u16, @intCast(-res))));
            self.classifyDmaOutcome(e);
            return nd.errnoToError(e);
        }
    }

    /// Snapshot EVERY lane's `submitted` (SeqCst) FIRST, then wait each
    /// lane's `completed` up to its snapshot. WHY: a device flush covers
    /// only commands the device completed before processing it, and an
    /// io_uring Fsync never drains its own ring's in-flight SQEs (no
    /// IO_DRAIN); chosen over an IO_DRAIN Fsync fan-out, which would order
    /// behind in-flight READS (a wedged read's 30 s class) and pay N
    /// flushes. Writes racing in behind the snapshot never extend the wait;
    /// reads do not ride the watermark; a dead lane fails the barrier.
    fn drainWriteLanes(self: *Device) Terminal!void {
        const n = self.lane_count.load(.acquire);
        var targets: [max_lanes]u64 = undefined;
        for (self.lanes[0..n], 0..) |l, i| targets[i] = l.?.wm.submitted.load(.seq_cst);
        for (self.lanes[0..n], 0..) |l, i| try l.?.wm.waitCompleted(targets[i]);
    }
};

// ---------------------------------------------------------------------------
// Tests — the contract (spec §8 table + sibling-suite pins).
// ---------------------------------------------------------------------------

const testing = std.testing;

/// A regular-file substrate under `/build` (ext4 in the venue; O_DIRECT
/// works there) or `/tmp` elsewhere. The `std.Io.File` handle doubles as the
/// page-cache view for byte-exact verification: an O_DIRECT write
/// invalidates the cached range, so a buffered read after completion sees
/// the device bytes.
const TestFile = struct {
    path_buf: [96]u8 = undefined,
    path: [:0]const u8 = "",
    file: std.Io.File = undefined,

    var seq: std.atomic.Value(u32) = .init(0);

    /// In place (`path` aliases `path_buf`, so the struct must not move).
    fn create(tf: *TestFile, len: u64) !void {
        const dir: []const u8 = blk: {
            std.Io.Dir.accessAbsolute(testing.io, "/build", .{}) catch break :blk "/tmp";
            break :blk "/build";
        };
        tf.path = try std.mem.printSentinel(&tf.path_buf, "{s}/sqz-nvme-{d}-{d}.img", .{ dir, linux.getpid(), seq.fetchAdd(1, .monotonic) }, 0);
        tf.file = try std.Io.Dir.createFileAbsolute(testing.io, tf.path, .{ .read = true, .truncate = true });
        try tf.file.setLength(testing.io, len);
    }

    fn readAt(self: *TestFile, buf: []u8, offset: u64) !void {
        const n = try self.file.readPositionalAll(testing.io, buf, offset);
        try testing.expectEqual(buf.len, n);
    }

    fn destroy(self: *TestFile) void {
        self.file.close(testing.io);
        std.Io.Dir.deleteFileAbsolute(testing.io, self.path) catch {};
    }
};

/// Deterministic payload: `(offset + i) * 2654435761 >> 24` keeps every 4 KiB
/// distinct so a misplaced DMA cannot pass by accident.
fn fillPattern(buf: []u8, seed: u64) void {
    for (buf, 0..) |*b, i| b.* = @truncate(((seed + i) *% 2654435761) >> 24);
}

fn expectPattern(buf: []const u8, seed: u64) !void {
    for (buf, 0..) |b, i| {
        const want: u8 = @truncate(((seed + i) *% 2654435761) >> 24);
        if (b != want) {
            std.debug.print("byte {d}: got {d} want {d}\n", .{ i, b, want });
            return error.TestExpectedEqual;
        }
    }
}

test "test_write_block_to_offset: a 4 MiB aligned write lands byte-exact at 4 MiB" {
    var tf: TestFile = .{};
    try tf.create(16 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    const payload = try testing.allocator.alignedAlloc(u8, .fromByteUnits(nd.dma_align), nd.block_buf_size);
    defer testing.allocator.free(payload);
    fillPattern(payload, 4 * derive.MiB);
    const before = stats.nvme_unaligned_write_fallbacks.load();
    try dev.writeBlock(4 * derive.MiB, payload);
    // Aligned source ⇒ the zero-copy branch; the bounce counter must not move.
    try testing.expectEqual(before, stats.nvme_unaligned_write_fallbacks.load());
    const back = try testing.allocator.alloc(u8, nd.block_buf_size);
    defer testing.allocator.free(back);
    try tf.readAt(back, 4 * derive.MiB);
    try expectPattern(back, 4 * derive.MiB);
}

test "discardRange on a regular file is a PUNCH through the write lane: the range reads zeros, neighbours survive, the file keeps its length; the substrate is classified once at open" {
    var tf: TestFile = .{};
    try tf.create(16 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    try testing.expectEqual(reclaim.Substrate.file_punch, dev.reclaimOp());
    const payload = try testing.allocator.alignedAlloc(u8, .fromByteUnits(nd.dma_align), 3 * 4096);
    defer testing.allocator.free(payload);
    fillPattern(payload, 4096);
    try dev.writeBlock(4096, payload);
    // Punch the middle 4 KiB (one block of a 4 KiB-block volume).
    try dev.discardRange(8192, 4096);
    try dev.flush(); // the barrier drains the discard like any write
    var back: [3 * 4096]u8 = undefined;
    try tf.readAt(&back, 4096);
    try expectPattern(back[0..4096], 4096);
    try testing.expectEqualSlices(u8, &@as([4096]u8, @splat(0)), back[4096 .. 2 * 4096]);
    try expectPattern(back[2 * 4096 ..], 4096 + 2 * 4096);
    const st = try tf.file.stat(testing.io);
    try testing.expectEqual(@as(u64, 16 * derive.MiB), st.size);
    // A zero-length discard is a no-op; the reclaim binding carries the class.
    try dev.discardRange(8192, 0);
    const target = reclaim.device_binding.target(dev);
    try testing.expectEqual(reclaim.Substrate.file_punch, target.substrate);
    try target.issue(target.ctx, 4096, 4096);
    try tf.readAt(back[0..4096], 4096);
    try testing.expectEqualSlices(u8, &@as([4096]u8, @splat(0)), back[0..4096]);
}

test "test_read_block_from_offset: write then read_block round-trips 4 MiB exactly; get_obj counts it" {
    var tf: TestFile = .{};
    try tf.create(16 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    const payload = try testing.allocator.alignedAlloc(u8, .fromByteUnits(nd.dma_align), nd.block_buf_size);
    defer testing.allocator.free(payload);
    fillPattern(payload, 8 * derive.MiB);
    try dev.writeBlock(8 * derive.MiB, payload);
    const get_before = stats.get_obj.load();
    const buf = try dev.readBlock(8 * derive.MiB, nd.block_buf_size);
    defer buf.release();
    try testing.expectEqual(nd.block_buf_size, buf.bytes().len);
    try expectPattern(buf.bytes(), 8 * derive.MiB);
    try testing.expectEqual(get_before + 1, stats.get_obj.load());
    // A probe read does the same I/O but is NOT counted (the 2026-07-26
    // counter-window flake class). A 4 KiB read rides the ranged slab, and
    // where the kernel granted the slab registration it rides ReadFixed.
    const fixed_before = stats.dev_fixed_buf_fills.load();
    const probe = try dev.probeReadBlock(8 * derive.MiB, 4096);
    defer probe.release();
    try testing.expectEqual(get_before + 1, stats.get_obj.load());
    const regs = dev.registrations();
    std.debug.print("lane 0 registrations: fixed_file={} fixed_buf={}\n", .{ regs.fixed_file, regs.fixed_buf });
    try testing.expectEqual(fixed_before + @as(u64, if (regs.fixed_buf) 1 else 0), stats.dev_fixed_buf_fills.load());
    // The 4 MiB read above came from the whole-block pool, never fixed.
    try testing.expectEqual(Buf.Kind.block, buf.kind);
    try testing.expectEqual(Buf.Kind.ranged, probe.kind);
}

test "test_concurrent_stress: 16 threads × 16 (write 4 MiB to one of 4 offsets + read back) complete without error" {
    var tf: TestFile = .{};
    try tf.create(16 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    const Stress = struct {
        fn run(d: *Device, seed: u64, failed: *std.atomic.Value(u32)) void {
            const payload = std.heap.page_allocator.alignedAlloc(u8, .fromByteUnits(nd.dma_align), nd.block_buf_size) catch {
                _ = failed.fetchAdd(1, .monotonic);
                return;
            };
            defer std.heap.page_allocator.free(payload);
            var i: u64 = 0;
            while (i < 16) : (i += 1) {
                const off = ((seed + i) % 4) * nd.block_buf_size;
                fillPattern(payload, seed * 1000 + i);
                d.writeBlock(off, payload) catch {
                    _ = failed.fetchAdd(1, .monotonic);
                    return;
                };
                const buf = d.readBlock(off, nd.block_buf_size) catch {
                    _ = failed.fetchAdd(1, .monotonic);
                    return;
                };
                defer buf.release();
                if (buf.bytes().len != nd.block_buf_size) _ = failed.fetchAdd(1, .monotonic);
            }
        }
    };
    var failed: std.atomic.Value(u32) = .init(0);
    var threads: [16]std.Thread = undefined;
    for (&threads, 0..) |*t, i| t.* = try std.Thread.spawn(.{}, Stress.run, .{ dev, @as(u64, i), &failed });
    for (threads) |t| t.join();
    try testing.expectEqual(@as(u32, 0), failed.load(.monotonic));
}

test "test_pooled_write_sources_take_aligned_dma_branch: a pool buffer is 4096-aligned and leaves the fallback counter unchanged" {
    var tf: TestFile = .{};
    try tf.create(8 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    const src = try acquireBlockBuf();
    defer src.release();
    try testing.expect(nd.isDmaAligned(src.bytes()));
    fillPattern(src.bytes(), 77);
    const before = stats.nvme_unaligned_write_fallbacks.load();
    try dev.writeBlock(0, src.bytes());
    try testing.expectEqual(before, stats.nvme_unaligned_write_fallbacks.load());
    const back = try dev.readBlock(0, nd.block_buf_size);
    defer back.release();
    try expectPattern(back.bytes(), 77);
}

test "test_unaligned_length_write_counts_fallback: 123 bytes at 4096 bump the counter by exactly 1, round-trip, and pad to 4 KiB" {
    var tf: TestFile = .{};
    try tf.create(1 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    var payload: [123]u8 = undefined;
    fillPattern(&payload, 9);
    const before = stats.nvme_unaligned_write_fallbacks.load();
    try dev.writeBlock(4096, &payload);
    try testing.expectEqual(before + 1, stats.nvme_unaligned_write_fallbacks.load());
    var back: [4096]u8 = undefined;
    try tf.readAt(&back, 4096);
    try expectPattern(back[0..123], 9);
    // The padded tail is WRITTEN as zeros (spec §2.1).
    for (back[123..]) |b| try testing.expectEqual(@as(u8, 0), b);
    // Nothing beyond the 4 KiB boundary moved.
    try testing.expectEqual(@as(u64, 1 * derive.MiB), try tf.file.length(testing.io));
}

test "test_drop_device_during_unaligned_writes_joins_cleanly: close while 8 threads write; each finishes Ok or Err; no hang" {
    var tf: TestFile = .{};
    try tf.create(8 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    const Writer = struct {
        fn run(d: *Device, seed: u64, ok: *std.atomic.Value(u32), errs: *std.atomic.Value(u32), unexpected: *std.atomic.Value(u32)) void {
            var payload: [123]u8 = undefined;
            var i: u64 = 0;
            while (i < 8) : (i += 1) {
                fillPattern(&payload, seed + i);
                if (d.writeBlock((seed * 8 + i) * 4096, &payload)) |_| {
                    _ = ok.fetchAdd(1, .monotonic);
                } else |e| {
                    _ = errs.fetchAdd(1, .monotonic);
                    // The closing door (QueueFull: backpressure, EAGAIN-class)
                    // or the exit drain (WorkerShutdown): nothing else.
                    if (e != error.QueueFull and e != error.WorkerShutdown) _ = unexpected.fetchAdd(1, .monotonic);
                }
            }
        }
    };
    var ok: std.atomic.Value(u32) = .init(0);
    var errs: std.atomic.Value(u32) = .init(0);
    var unexpected: std.atomic.Value(u32) = .init(0);
    var threads: [8]std.Thread = undefined;
    const t0 = clock.monotonicNs();
    for (&threads, 0..) |*t, i| t.* = try std.Thread.spawn(.{}, Writer.run, .{ dev, @as(u64, i), &ok, &errs, &unexpected });
    dev.close();
    for (threads) |t| t.join();
    const elapsed_s = (clock.monotonicNs() - t0) / std.time.ns_per_s;
    try testing.expectEqual(@as(u32, 64), ok.load(.monotonic) + errs.load(.monotonic));
    try testing.expectEqual(@as(u32, 0), unexpected.load(.monotonic));
    try testing.expect(elapsed_s < 30);
    // After close every submit is a refusal, never a hang; the class is
    // backpressure (the caller may retry elsewhere), not "bad argument".
    var p: [16]u8 = undefined;
    try testing.expectError(error.QueueFull, dev.writeBlock(0, &p));
    try testing.expectEqual(nd.ErrorClass.retryable, nd.classOf(error.QueueFull));
}

test "test_drop_after_unaligned_burst_does_not_hang: 32 sequential unaligned writes then close < 10 s" {
    var tf: TestFile = .{};
    try tf.create(8 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    var payload: [5000]u8 = undefined;
    var i: u64 = 0;
    while (i < 32) : (i += 1) {
        fillPattern(&payload, i);
        try dev.writeBlock(i * 8192, &payload);
    }
    const t0 = clock.monotonicNs();
    dev.close();
    try testing.expect((clock.monotonicNs() - t0) / std.time.ns_per_s < 10);
}

test "test_read_block_exceeds_pool_size: a 5 MiB pooled read is refused at the door, no I/O" {
    var tf: TestFile = .{};
    try tf.create(8 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    const fills = stats.dev_fills.load();
    try testing.expectError(error.ReadSizeExceedsPool, dev.readBlock(0, 5 * derive.MiB));
    try testing.expectEqual(fills, stats.dev_fills.load());
}

test "zero-length read: empty result, no SQE, no device op" {
    var tf: TestFile = .{};
    try tf.create(1 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    const fills = stats.dev_fills.load();
    const buf = try dev.readBlock(4096, 0);
    defer buf.release();
    try testing.expectEqual(@as(usize, 0), buf.bytes().len);
    try testing.expectEqual(fills, stats.dev_fills.load());
}

test "test_read_past_eof_errors_instead_of_recycled_garbage: a read at file_len is ShortRead (kernel returns 0)" {
    var tf: TestFile = .{};
    try tf.create(1 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    const ranged_before = pools().ranged.allocated_bytes.load();
    try testing.expectError(error.ShortRead, dev.readBlock(1 * derive.MiB, 4096));
    // The bounce slot went home.
    try testing.expectEqual(ranged_before, pools().ranged.allocated_bytes.load());
}

test "test_read_straddling_eof_errors_instead_of_garbage_tail: [len-2048, +4096) is ShortRead with got == 2048; file did not grow" {
    var tf: TestFile = .{};
    try tf.create(1 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    var head: [4096]u8 align(4096) = undefined;
    fillPattern(&head, 1);
    try dev.writeBlock(0, &head);
    const outcome = try dev.readBlockOutcome(1 * derive.MiB - 2048, 4096, true);
    switch (outcome) {
        .full => return error.TestUnexpectedResult,
        .short => |got| try testing.expectEqual(@as(usize, 2048), got),
    }
    try testing.expectError(error.ShortRead, dev.readBlock(1 * derive.MiB - 2048, 4096));
    try testing.expectEqual(@as(u64, 1 * derive.MiB), try tf.file.length(testing.io));
}

test "read_bounce_pool_routing_and_home_recycle: ≤ 64 KiB rides the slab, larger the block pool; release returns allocated_bytes exactly" {
    try initPools();
    const p = pools();
    const r0 = p.ranged.allocated_bytes.load();
    const b0 = p.block.allocated_bytes.load();
    const a = try acquireReadBuf(4096);
    try testing.expectEqual(Buf.Kind.ranged, a.kind);
    try testing.expect(p.ranged.contains(a.ptr, 4096));
    const b = try acquireReadBuf(64 * derive.KiB);
    try testing.expectEqual(Buf.Kind.ranged, b.kind);
    try testing.expectEqual(r0 + 2 * nd.ranged_buf_size, p.ranged.allocated_bytes.load());
    const c = try acquireReadBuf(64 * derive.KiB + 1);
    try testing.expectEqual(Buf.Kind.block, c.kind);
    const d = try acquireReadBuf(4 * derive.MiB);
    try testing.expectEqual(Buf.Kind.block, d.kind);
    try testing.expectEqual(b0 + 2 * nd.block_buf_size, p.block.allocated_bytes.load());
    // Every pooled pointer honours the DMA alignment contract.
    for ([_]Buf{ a, b, c, d }) |x| try testing.expect(derive.isAligned(@intFromPtr(x.ptr), nd.dma_align));
    const cached0 = p.block.cached_bytes.load();
    a.release();
    b.release();
    c.release();
    d.release();
    try testing.expectEqual(r0, p.ranged.allocated_bytes.load());
    try testing.expectEqual(b0, p.block.allocated_bytes.load());
    // The two released blocks are CACHED (resident, idle): the pool's own
    // retention, gauged; the idle trim at Red (min 0) unmaps them and the
    // next acquire is a miss again.
    try testing.expectEqual(cached0 + 2 * nd.block_buf_size, p.block.cached_bytes.load());
    const comp = p.block.component();
    try testing.expectEqualStrings("aligned_buf_pool", comp.name);
    try testing.expectEqual(16 * nd.block_buf_size, comp.floor);
    try testing.expectEqual(cached0 + 2 * nd.block_buf_size, comp.current());
    const misses0 = stats.aligned_pool_misses.load();
    comp.trim.?.call(comp.trim.?.ctx, mem_budget.trim_red_min_idle_ticks);
    try testing.expectEqual(@as(u64, 0), p.block.cached_bytes.load());
    const e = try acquireReadBuf(4 * derive.MiB);
    try testing.expectEqual(misses0 + 1, stats.aligned_pool_misses.load());
    e.release();
    // A Yellow trim (min 2) leaves an entry released in this tick.
    comp.trim.?.call(comp.trim.?.ctx, mem_budget.trim_yellow_min_idle_ticks);
    try testing.expectEqual(nd.block_buf_size, p.block.cached_bytes.load());
    // The shed face trims toward a target.
    comp.shed.?.call(comp.shed.?.ctx, 0);
    try testing.expectEqual(@as(u64, 0), p.block.cached_bytes.load());
    // The pinned ranged slab is registered attribution-only at its full length.
    try testing.expectEqual(p.ranged.len, p.ranged.slab_bytes.load());
}

test "write_lane_pool_spreads_by_offset_and_stays_byte_exact" {
    var tf: TestFile = .{};
    try tf.create(16 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    try testing.expectEqual(@as(u32, 1), dev.writeLanes());
    const grain: u64 = 1 * derive.MiB;
    try dev.setWriteLanes(4, grain);
    try testing.expectEqual(@as(u32, 4), dev.writeLanes());
    // Lane counts only grow.
    try dev.setWriteLanes(2, grain);
    try testing.expectEqual(@as(u32, 4), dev.writeLanes());
    try testing.expectEqual(@as(u32, 4), dev.laneCount());

    var wm_before: [8]Watermark = undefined;
    const before = dev.writeLaneWatermarks(&wm_before);
    const payload = try testing.allocator.alignedAlloc(u8, .fromByteUnits(nd.dma_align), grain);
    defer testing.allocator.free(payload);
    var blk: u64 = 0;
    while (blk < 8) : (blk += 1) {
        fillPattern(payload, blk);
        try dev.writeBlock(blk * grain, payload);
    }
    var wm_after: [8]Watermark = undefined;
    const after = dev.writeLaneWatermarks(&wm_after);
    try testing.expectEqual(@as(usize, 4), after.len);
    // 8 blocks at 1 MiB stride over 4 lanes: every lane's `submitted` moved
    // by exactly 2, and every submit completed.
    for (before, after) |b, a| {
        try testing.expectEqual(b.submitted + 2, a.submitted);
        try testing.expectEqual(a.submitted, a.completed);
    }
    // Two writes to block 5 (whole block, then a 4 KiB piece inside it)
    // move only lane (5 % 4) = 1: same block → same lane.
    fillPattern(payload, 5);
    try dev.writeBlock(5 * grain, payload);
    try dev.writeBlock(5 * grain + 4096, payload[4096..8192]);
    var wm_again: [8]Watermark = undefined;
    const again = dev.writeLaneWatermarks(&wm_again);
    for (after, again, 0..) |a, g, i| {
        const want: u64 = if (i == nd.writeLaneIndex(5 * grain, grain, 4)) 2 else 0;
        try testing.expectEqual(a.submitted + want, g.submitted);
    }
    try dev.flush();
    const back = try testing.allocator.alloc(u8, grain);
    defer testing.allocator.free(back);
    blk = 0;
    while (blk < 8) : (blk += 1) {
        try tf.readAt(back, blk * grain);
        try expectPattern(back, if (blk == 5) 5 else blk);
    }
}

test "flush_completes_no_earlier_than_writes_on_every_lane: a stalled write on lane 3 is drained before the barrier returns" {
    var tf: TestFile = .{};
    try tf.create(16 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    const grain: u64 = 1 * derive.MiB;
    try dev.setWriteLanes(4, grain);
    // Worker-side stall: lane 3 sleeps 800 ms before building the SQE, so
    // the barrier's drain has a real window to wait on.
    dev.setTestWriteStall(1, 800);
    const Writer = struct {
        fn run(d: *Device, payload: []const u8, g: u64, err: *?anyerror) void {
            d.writeBlock(3 * g, payload) catch |e| {
                err.* = e;
            };
        }
    };
    const payload = try testing.allocator.alignedAlloc(u8, .fromByteUnits(nd.dma_align), 4096);
    defer testing.allocator.free(payload);
    fillPattern(payload, 3);
    var werr: ?anyerror = null;
    const t = try std.Thread.spawn(.{}, Writer.run, .{ dev, payload, grain, &werr });
    // The writer has stamped lane 3's `submitted` once it enqueued; wait for
    // that stamp (not a sleep: the seam holds the window).
    var wm: [8]Watermark = undefined;
    while (dev.writeLaneWatermarks(&wm)[3].submitted == 0) std.atomic.spinLoopHint();
    const syncs_before = stats.data_device_syncs.load();
    try dev.flush();
    const now = dev.writeLaneWatermarks(&wm);
    try testing.expectEqual(@as(u64, 1), now[3].submitted);
    try testing.expectEqual(now[3].submitted, now[3].completed);
    try testing.expectEqual(syncs_before + 1, stats.data_device_syncs.load());
    t.join();
    try testing.expectEqual(@as(?anyerror, null), werr);
    var back: [4096]u8 = undefined;
    try tf.readAt(&back, 3 * grain);
    try expectPattern(&back, 3);
}

test "read_lane_pool_serves_exact_bytes_and_leaves_writes_on_lane_zero" {
    var tf: TestFile = .{};
    try tf.create(16 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    try testing.expectEqual(@as(u32, 1), dev.readLanes());
    try dev.setReadLanes(4);
    try testing.expectEqual(@as(u32, 4), dev.readLanes());
    try dev.setReadLanes(2);
    try testing.expectEqual(@as(u32, 4), dev.readLanes());
    const grain: u64 = 1 * derive.MiB;
    const payload = try testing.allocator.alignedAlloc(u8, .fromByteUnits(nd.dma_align), grain);
    defer testing.allocator.free(payload);
    var blk: u64 = 0;
    while (blk < 8) : (blk += 1) {
        fillPattern(payload, blk);
        try dev.writeBlock(blk * grain, payload);
    }
    try dev.flush();
    // Writes stayed on lane 0 (write_lanes == 1 ⇒ lane 0 byte-identical to
    // the pre-fanout posture).
    var wm: [8]Watermark = undefined;
    const w = dev.writeLaneWatermarks(&wm);
    try testing.expectEqual(@as(u64, 9), w[0].submitted); // 8 writes + 1 barrier
    for (w[1..]) |x| try testing.expectEqual(@as(u64, 0), x.submitted);
    const Reader = struct {
        fn run(d: *Device, g: u64, failed: *std.atomic.Value(u32)) void {
            var i: u64 = 0;
            while (i < 4) : (i += 1) {
                const b = (i * 3) % 8;
                const buf = d.readBlock(b * g, @intCast(g)) catch {
                    _ = failed.fetchAdd(1, .monotonic);
                    return;
                };
                defer buf.release();
                if (buf.bytes().len != g) _ = failed.fetchAdd(1, .monotonic);
                expectPattern(buf.bytes(), b) catch {
                    _ = failed.fetchAdd(1, .monotonic);
                };
            }
        }
    };
    var failed: std.atomic.Value(u32) = .init(0);
    var threads: [8]std.Thread = undefined;
    for (&threads) |*t| t.* = try std.Thread.spawn(.{}, Reader.run, .{ dev, grain, &failed });
    for (threads) |t| t.join();
    try testing.expectEqual(@as(u32, 0), failed.load(.monotonic));
}

test "fill-issue economy: 64 queued fills behind a stalled lane ride ≤ 4 submit enters; an idle lane wakes through the eventfd" {
    var tf: TestFile = .{};
    try tf.create(4 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    const wakes_before = stats.dev_wake_writes.load();
    const b0 = try dev.readBlock(0, 4096);
    b0.release();
    // The lane was parked in the ring; the arrival paid one eventfd write.
    try testing.expect(stats.dev_wake_writes.load() > wakes_before);

    // Stall the next read SQE for 150 ms: the other 63 arrivals queue behind
    // it and the pump drains them in the same pass — ONE enter carries them.
    dev.setTestReadStall(1, 150);
    const Gate = struct {
        ready: std.atomic.Value(u32) = .init(0),
        go: std.atomic.Value(bool) = .init(false),
        failed: std.atomic.Value(u32) = .init(0),
    };
    const Reader = struct {
        fn run(d: *Device, g: *Gate, i: u64) void {
            _ = g.ready.fetchAdd(1, .monotonic);
            while (!g.go.load(.acquire)) std.atomic.spinLoopHint();
            const buf = d.readBlock((i % 8) * 4096, 4096) catch {
                _ = g.failed.fetchAdd(1, .monotonic);
                return;
            };
            buf.release();
        }
    };
    var gate: Gate = .{};
    const enters_before = stats.dev_submit_enters.load();
    const fills_before = stats.dev_fills.load();
    var threads: [64]std.Thread = undefined;
    for (&threads, 0..) |*t, i| t.* = try std.Thread.spawn(.{}, Reader.run, .{ dev, &gate, @as(u64, i) });
    while (gate.ready.load(.monotonic) < threads.len) std.atomic.spinLoopHint();
    gate.go.store(true, .release);
    for (threads) |t| t.join();
    try testing.expectEqual(@as(u32, 0), gate.failed.load(.monotonic));
    try testing.expectEqual(fills_before + 64, stats.dev_fills.load());
    try testing.expect(stats.dev_submit_enters.load() - enters_before <= 4);
}

test "wake coalescing: N publishes between two lane passes cost ONE eventfd write — elision ≥ N−1" {
    var tf: TestFile = .{};
    try tf.create(1 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    // Warm: after this completion the lane consumed its wake, disarmed the
    // coalescer and parked.
    const warm = try dev.readBlock(0, 4096);
    warm.release();
    const seams = &dev.seams;
    seams.held.store(0, .release);
    seams.hold_before_park.store(1, .release);
    const Reader = struct {
        fn run(d: *Device, failed: *std.atomic.Value(u32)) void {
            const b = d.readBlock(0, 4096) catch {
                _ = failed.fetchAdd(1, .monotonic);
                return;
            };
            b.release();
        }
    };
    const Writes = struct {
        /// THIS device's lanes' completed eventfd writes: the process-wide
        /// `dev_wake_writes` also counts a lane an earlier test left.
        fn of(d: *Device) u64 {
            var n: u64 = 0;
            for (d.lanes) |l| if (l) |lane| {
                n += lane.park.written.load(.seq_cst);
            };
            return n;
        }
    };
    var failed: std.atomic.Value(u32) = .init(0);
    var threads: [9]std.Thread = undefined;
    var spawned: usize = 0;
    // A failed check below still releases the held pass and joins every
    // reader: the device's deinit would otherwise wait on the held lane.
    defer {
        seams.hold_before_park.store(0, .release);
        futex.wakeAll(&seams.hold_before_park);
        for (threads[0..spawned]) |t| t.join();
    }
    // The first arrival wakes the parked lane; the pass pumps it and holds
    // BEFORE its park.
    threads[0] = try std.Thread.spawn(.{}, Reader.run, .{ dev, &failed });
    spawned = 1;
    while (seams.held.load(.acquire) == 0) futex.wait(&seams.held, 0);
    const w0 = Writes.of(dev);
    const e0 = stats.dev_wakes_elided.load();
    const p0 = stats.dev_wake_writes.load() + e0;
    // N publishes between this pass and the next: the first pays the
    // write (the pass disarmed), every other elides.
    for (threads[1..]) |*t| {
        t.* = try std.Thread.spawn(.{}, Reader.run, .{ dev, &failed });
        spawned += 1;
    }
    const held_publishes: u64 = threads.len - 1;
    while (stats.dev_wake_writes.load() + stats.dev_wakes_elided.load() - p0 < held_publishes) std.atomic.spinLoopHint();
    try testing.expectEqual(w0 + 1, Writes.of(dev));
    seams.hold_before_park.store(0, .release);
    futex.wakeAll(&seams.hold_before_park);
    for (threads[0..spawned]) |t| t.join();
    spawned = 0;
    try testing.expectEqual(@as(u32, 0), failed.load(.monotonic));
    std.debug.print("\nmeasurement: {d} publishes between two lane passes: this device's eventfd writes {d}, elided (process-wide) {d}\n", .{ held_publishes, Writes.of(dev) - w0, stats.dev_wakes_elided.load() - e0 });
}

/// Counts allocation CALLS through to `inner` (the per-op allocation pin).
const CountingAllocator = struct {
    inner: std.mem.Allocator,
    allocs: std.atomic.Value(u64) = .init(0),

    const vtable: std.mem.Allocator.VTable = .{ .alloc = alloc, .resize = resize, .remap = remap, .free = free };

    fn allocator(self: *CountingAllocator) std.mem.Allocator {
        return .{ .ptr = self, .vtable = &vtable };
    }
    fn alloc(ctx: *anyopaque, len: usize, alignment: std.mem.Alignment, ret_addr: usize) ?[*]u8 {
        const self: *CountingAllocator = @ptrCast(@alignCast(ctx));
        _ = self.allocs.fetchAdd(1, .monotonic);
        return self.inner.rawAlloc(len, alignment, ret_addr);
    }
    fn resize(ctx: *anyopaque, memory: []u8, alignment: std.mem.Alignment, new_len: usize, ret_addr: usize) bool {
        const self: *CountingAllocator = @ptrCast(@alignCast(ctx));
        return self.inner.rawResize(memory, alignment, new_len, ret_addr);
    }
    fn remap(ctx: *anyopaque, memory: []u8, alignment: std.mem.Alignment, new_len: usize, ret_addr: usize) ?[*]u8 {
        const self: *CountingAllocator = @ptrCast(@alignCast(ctx));
        return self.inner.rawRemap(memory, alignment, new_len, ret_addr);
    }
    fn free(ctx: *anyopaque, memory: []u8, alignment: std.mem.Alignment, ret_addr: usize) void {
        const self: *CountingAllocator = @ptrCast(@alignCast(ctx));
        self.inner.rawFree(memory, alignment, ret_addr);
    }
};

test "a warm readBlock, writeBlock and flush allocate nothing (preallocated cells, pooled DMA buffers; charter §12)" {
    var tf: TestFile = .{};
    try tf.create(1 * derive.MiB);
    defer tf.destroy();
    var counting: CountingAllocator = .{ .inner = testing.allocator };
    const dev = try Device.open(counting.allocator(), tf.path);
    defer dev.deinit();
    var payload: [4096]u8 align(4096) = undefined;
    fillPattern(&payload, 21);
    // Warm the lane and the pools, then pin.
    try dev.writeBlock(0, &payload);
    const warm = try dev.readBlock(0, 4096);
    warm.release();
    try dev.flush();
    const before = counting.allocs.load(.monotonic);
    try dev.writeBlock(4096, &payload);
    const buf = try dev.readBlock(4096, 4096);
    try expectPattern(buf.bytes(), 21);
    buf.release();
    try dev.flush();
    try testing.expectEqual(before, counting.allocs.load(.monotonic));
}

test "a dead ring fails every in-flight op and every later request with RingFailed, balances the watermark, sends the DMA slots home, and the lane still joins cleanly" {
    var tf: TestFile = .{};
    try tf.create(1 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    const warm = try dev.readBlock(0, 4096);
    warm.release();
    const r0 = pools().ranged.allocated_bytes.load();
    // Read A sits IN the ring behind a 300 ms linked timeout (the slow seam).
    dev.setTestReadSlow(1, 300);
    const Slow = struct {
        fn run(d: *Device, got: *?anyerror) void {
            const b = d.readBlock(0, 4096) catch |e| {
                got.* = e;
                return;
            };
            b.release();
        }
    };
    var slow_err: ?anyerror = null;
    const submit_enters = stats.dev_submit_enters.load();
    const t = try std.Thread.spawn(.{}, Slow.run, .{ dev, &slow_err });
    // A's pass reached its enter: the scan is over, A is in the ring.
    while (stats.dev_submit_enters.load() == submit_enters) std.atomic.spinLoopHint();
    // The kill lands at the next pass top; read B's arrival is that pass:
    // A fails in the ring, B at the pump.
    dev.seams.kill_ring.store(1, .seq_cst);
    try testing.expectError(error.RingFailed, dev.readBlock(4096, 4096));
    t.join();
    try testing.expectEqual(@as(?anyerror, error.RingFailed), slow_err);
    // Nothing in flight, both bounce slots recycled (MEM-1 held to the end).
    try testing.expectEqual(@as(u32, 0), dev.overdueCensus(clock.monotonicNs(), 0).in_flight);
    try testing.expectEqual(r0, pools().ranged.allocated_bytes.load());
    // Write-class ops on the dead lane: RingFailed too, and the barrier's
    // watermark drain never waits on them (submitted == completed).
    var payload: [4096]u8 align(4096) = undefined;
    fillPattern(&payload, 3);
    try testing.expectError(error.RingFailed, dev.writeBlock(0, &payload));
    try testing.expectError(error.RingFailed, dev.flush());
    var wm: [1]Watermark = undefined;
    const w = dev.writeLaneWatermarks(&wm)[0];
    try testing.expectEqual(w.submitted, w.completed);
    try testing.expect(!dev.fenced());
    // `deinit` (deferred): the dead lane parks on its eventfd and the
    // closer's wake ends it — a join, never a hang.
}

test "user_data sentinels: the seam's linked timeout sits below the park primitive's two tags, every slot index below all three" {
    try testing.expect(nd.UD_SEAM < ring_park.reserved_ud);
    try testing.expect(ring_park.reserved_ud < ring_park.wake_ud);
    try testing.expect(@as(u64, nd.uring_req_queue_cap) < nd.UD_RESERVED);
}

test "slow-read seam: a fast fill completes while a linked-timeout slow fill holds the same lane (no head-of-line blocking)" {
    var tf: TestFile = .{};
    try tf.create(1 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    // Warm the lane so both reads below are queued, not the first arrival.
    const warm = try dev.readBlock(0, 4096);
    warm.release();
    dev.setTestReadSlow(1, 300);
    const Slow = struct {
        fn run(d: *Device, elapsed_ns: *u64, failed: *bool) void {
            const t0 = clock.monotonicNs();
            const b = d.readBlock(0, 4096) catch {
                failed.* = true;
                return;
            };
            b.release();
            elapsed_ns.* = clock.monotonicNs() - t0;
        }
    };
    var slow_ns: u64 = 0;
    var slow_failed = false;
    const t = try std.Thread.spawn(.{}, Slow.run, .{ dev, &slow_ns, &slow_failed });
    // The slow read must have claimed the seam before the fast one is
    // queued; wait for its SQE build (dev_fills moves) rather than sleeping.
    const fills = stats.dev_fills.load();
    while (stats.dev_fills.load() == fills) std.atomic.spinLoopHint();
    const t0 = clock.monotonicNs();
    const fast = try dev.readBlock(4096, 4096);
    fast.release();
    const fast_ns = clock.monotonicNs() - t0;
    t.join();
    try testing.expect(!slow_failed);
    try testing.expect(slow_ns >= 300 * std.time.ns_per_ms);
    try testing.expect(fast_ns < slow_ns);
}

test "a stalled read is an OVERDUE op in the census (count, oldest age) and never a per-op timeout: the caller stays parked, gets its data, and the census empties at the CQE" {
    var tf: TestFile = .{};
    try tf.create(4 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    const r0 = pools().ranged.allocated_bytes.load();
    try testing.expectEqual(@as(u32, 0), dev.overdueCensus(clock.monotonicNs(), 0).in_flight);
    const stall_ms: u64 = 300;
    dev.setTestReadStall(1, stall_ms);
    const Stalled = struct {
        fn run(d: *Device, elapsed_ns: *u64, failed: *bool) void {
            const t0 = clock.monotonicNs();
            const b = d.readBlock(0, 4096) catch {
                failed.* = true;
                return;
            };
            b.release();
            elapsed_ns.* = clock.monotonicNs() - t0;
        }
    };
    var elapsed_ns: u64 = 0;
    var failed = false;
    const t = try std.Thread.spawn(.{}, Stalled.run, .{ dev, &elapsed_ns, &failed });
    // The read is in flight (the worker is stalled ahead of its SQE); the
    // census names it, and with a bound it has aged past, as overdue.
    var census = dev.overdueCensus(clock.monotonicNs(), 0);
    while (census.in_flight == 0) : (census = dev.overdueCensus(clock.monotonicNs(), 0)) std.atomic.spinLoopHint();
    try testing.expectEqual(@as(u32, 1), census.in_flight);
    try testing.expectEqual(@as(u32, 1), census.overdue);
    try testing.expectEqual(@as(u32, 0), dev.overdueCensus(clock.monotonicNs(), 60 * std.time.ns_per_s).overdue);
    // The DMA slot is checked out while the op is in flight (MEM-1).
    try testing.expectEqual(r0 + nd.ranged_buf_size, pools().ranged.allocated_bytes.load());
    t.join();
    try testing.expect(!failed);
    try testing.expect(elapsed_ns >= stall_ms * std.time.ns_per_ms);
    try testing.expect(census.oldest_age_ns <= elapsed_ns);
    // Completed: nothing in flight, the slot recycled.
    try testing.expectEqual(@as(u32, 0), dev.overdueCensus(clock.monotonicNs(), 0).in_flight);
    try testing.expectEqual(r0, pools().ranged.allocated_bytes.load());
}

test "probeDirectIo: a path that cannot be opened O_DIRECT is refused LOUDLY; capacity is seek(End) on a file" {
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.DirectIoUnsupported, probeDirectIo("/nonexistent/sqz-nvme-no-such-volume"));
    try log.testing.expectAllFired();
    var tf: TestFile = .{};
    try tf.create(3 * derive.MiB + 4096);
    defer tf.destroy();
    try probeDirectIo(tf.path);
    try testing.expectEqual(@as(u64, 3 * derive.MiB + 4096), try capacityBytes(tf.path));
    // A regular file classifies as file-backed; the stats face names it.
    try testing.expectEqual(nd.WriteCacheClass.file_backed, probeWriteCache(tf.path));
    try testing.expectEqual(nd.WriteCacheClass.unknown, probeWriteCache("/nonexistent/sqz-nvme-no-such-volume"));
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    try testing.expectEqual(nd.WriteCacheClass.file_backed, dev.writeCache());
    var buf: [256]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try dev.writeStatsFaceJson(&w);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"data_volume_write_cache\":\"") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "=file-backed\"") != null);
    // Open itself refuses loudly on a bad path (no worker is ever spawned).
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.DirectIoUnsupported, Device.open(testing.allocator, "/nonexistent/sqz-nvme-no-such-volume"));
    try log.testing.expectAllFired();
}

test "node liveness: TTL-cached existence; a deleted node is seen after the TTL window" {
    var tf: TestFile = .{};
    try tf.create(1 * derive.MiB);
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    try testing.expect(dev.nodeExistsCached());
    tf.destroy();
    // Still fresh: the cache answers (within the 1 s TTL).
    try testing.expect(dev.nodeExistsCached());
    // Expire the cache deterministically and re-probe.
    dev.node_probe.at_ms.store(1, .monotonic);
    try testing.expect(!dev.nodeExistsCached());
}

test "fence latch: an injected EBADE write latches the fence, poisons custody, refuses every later DMA; reads are never gated" {
    var tf: TestFile = .{};
    try tf.create(4 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    defer clearCustodyPoisonForTest();
    var hook_fired: std.atomic.Value(u32) = .init(0);
    const Hook = struct {
        fn poison(ctx: ?*anyopaque, why: []const u8) void {
            const c: *std.atomic.Value(u32) = @ptrCast(@alignCast(ctx.?));
            _ = c.fetchAdd(1, .monotonic);
            std.debug.assert(std.mem.indexOf(u8, why, "reservation conflict") != null);
        }
    };
    const hooks = CustodyHooks{ .ctx = &hook_fired, .poison = Hook.poison, .authorize = null };
    installCustodyHooks(&hooks);
    defer installCustodyHooks(null);

    var payload: [4096]u8 align(4096) = undefined;
    fillPattern(&payload, 1);
    try dev.writeBlock(0, &payload);
    try testing.expect(!dev.fenced());
    const refusals_before = stats.data_dma_fence_refusals.load();

    // One injected EBADE: the ONE write-outcome funnel latches the fence.
    log.testing.expectErrorLogs(1);
    dev.setFailNextWritesErrno(1, .BADE);
    try testing.expectError(error.ReservationConflict, dev.writeBlock(0, &payload));
    try log.testing.expectAllFired();
    try testing.expect(dev.fenced());
    try testing.expectEqual(@as(u32, 1), hook_fired.load(.monotonic));
    // Sticky: every later write-class DMA is refused and counted.
    try testing.expectError(error.WriterGuardFenced, dev.writeBlock(0, &payload));
    try testing.expectError(error.WriterGuardFenced, dev.flush());
    try testing.expectEqual(refusals_before + 2, stats.data_dma_fence_refusals.load());
    // Reads are NOT fence-gated: a fenced holder reading its own device
    // corrupts nothing.
    const buf = try dev.readBlock(0, 4096);
    defer buf.release();
    try expectPattern(buf.bytes(), 1);
    // A classless injected failure passes through untouched — a transient
    // hiccup never self-inflicts a fail-stop (checked on a fresh device).
    clearCustodyPoisonForTest();
    const dev2 = try Device.open(testing.allocator, tf.path);
    defer dev2.deinit();
    dev2.setFailNextWritesErrno(1, .IO);
    try testing.expectError(error.InputOutput, dev2.writeBlock(0, &payload));
    try testing.expect(!dev2.fenced());
    try dev2.writeBlock(0, &payload);
}

test "fence signal: the installed per-volume latch halts the device on first poll; dismount close refuses in the EROFS class after the fence verdict" {
    var tf: TestFile = .{};
    try tf.create(4 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    defer clearCustodyPoisonForTest();
    const Sig = struct {
        var failed: std.atomic.Value(bool) = .init(false);
        fn poll(_: ?*anyopaque) bool {
            return failed.load(.monotonic);
        }
    };
    Sig.failed.store(false, .monotonic);
    const sig = FenceSignal{ .ctx = null, .poll = Sig.poll };
    dev.setFenceSignal(&sig);
    var payload: [4096]u8 align(4096) = undefined;
    fillPattern(&payload, 2);
    try dev.writeBlock(0, &payload);
    Sig.failed.store(true, .monotonic);
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.WriterGuardFenced, dev.writeBlock(0, &payload));
    try log.testing.expectAllFired();
    try testing.expect(dev.fenced());

    // Dismount class on a fresh device: judged AFTER the fence, so a clean
    // device refuses EROFS-class and counts it.
    clearCustodyPoisonForTest();
    const dev2 = try Device.open(testing.allocator, tf.path);
    defer dev2.deinit();
    try testing.expect(!dev2.dismounted());
    const d0 = stats.data_dma_dismount_refusals.load();
    dev2.closeAtDismount();
    try testing.expect(dev2.dismounted());
    try testing.expectError(error.DismountRefused, dev2.writeBlock(0, &payload));
    try testing.expectEqual(d0 + 1, stats.data_dma_dismount_refusals.load());
}

test "thread names: lanes are numbered process-wide as sqz-nvme<N>; the write phase histograms moved" {
    var tf: TestFile = .{};
    try tf.create(1 * derive.MiB);
    defer tf.destroy();
    const dev = try Device.open(testing.allocator, tf.path);
    defer dev.deinit();
    const n0 = dev.laneNumber(0);
    try dev.setReadLanes(2);
    try testing.expect(dev.laneNumber(1) > n0);
    var name_buf: thread_name.CommBuf = undefined;
    const name = thread_name.commName(&name_buf, laneThreadName(dev.laneNumber(1)));
    try testing.expectEqualStrings("sqz-nvme", thread_name.classify(name));
    // Phase histograms are exact-sum: dma ⊇ dev_queue + dev_service, both
    // moved by the writes of the earlier tests in this process.
    try testing.expect(stats.write_pipeline_dev_queue_ns.snapshot().count > 0);
    try testing.expect(stats.write_pipeline_dev_service_ns.snapshot().count > 0);
    // The caller census is level-triggered: nobody is parked now.
    try testing.expectEqual(@as(u64, 0), stats.device_lock_waiters.load());
}

test "loop device: capacity by seek(End), write-cache class from sysfs, aligned round-trip and barrier on a real block device" {
    // Needs the loop driver and root; skipped (and said so) elsewhere.
    const ctl_raw = linux.open("/dev/loop-control", .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
    if (linux.errno(ctl_raw) != .SUCCESS) return testkit.skip(@src(), .no_device, "no /dev/loop-control (the loop driver, root)");
    const ctl: linux.fd_t = @intCast(ctl_raw);
    defer _ = linux.close(ctl);
    const LOOP_CTL_GET_FREE = 0x4C82;
    const LOOP_SET_FD = 0x4C00;
    const LOOP_CLR_FD = 0x4C01;
    const n_raw = linux.ioctl(ctl, LOOP_CTL_GET_FREE, 0);
    if (linux.errno(n_raw) != .SUCCESS) return testkit.skip(@src(), .no_device, "LOOP_CTL_GET_FREE refused");
    var tf: TestFile = .{};
    try tf.create(8 * derive.MiB);
    defer tf.destroy();
    var dev_path_buf: [32]u8 = undefined;
    const dev_path = try std.mem.printSentinel(&dev_path_buf, "/dev/loop{d}", .{n_raw}, 0);
    const loop_raw = linux.open(dev_path, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
    if (linux.errno(loop_raw) != .SUCCESS) return testkit.skip(@src(), .no_device, "the free loop device node did not open");
    const loop_fd: linux.fd_t = @intCast(loop_raw);
    defer _ = linux.close(loop_fd);
    const backing_raw = linux.open(tf.path, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(backing_raw));
    const backing: linux.fd_t = @intCast(backing_raw);
    defer _ = linux.close(backing);
    if (linux.errno(linux.ioctl(loop_fd, LOOP_SET_FD, @intCast(backing))) != .SUCCESS) return testkit.skip(@src(), .no_device, "LOOP_SET_FD refused");
    defer _ = linux.ioctl(loop_fd, LOOP_CLR_FD, 0);

    try testing.expectEqual(@as(u64, 8 * derive.MiB), try capacityBytes(dev_path));
    const class = probeWriteCache(dev_path);
    std.debug.print("loop write_cache class: {s}\n", .{class.name()});
    try testing.expect(class == .write_back or class == .write_through);

    const dev = try Device.open(testing.allocator, dev_path);
    defer dev.deinit();
    const payload = try testing.allocator.alignedAlloc(u8, .fromByteUnits(nd.dma_align), 64 * derive.KiB);
    defer testing.allocator.free(payload);
    fillPattern(payload, 11);
    try dev.writeBlock(1 * derive.MiB, payload);
    try dev.flush();
    const buf = try dev.readBlock(1 * derive.MiB, 64 * derive.KiB);
    defer buf.release();
    try expectPattern(buf.bytes(), 11);
    // A block device is all-or-EIO past its end — never a short read.
    const past = dev.readBlock(8 * derive.MiB, 4096);
    if (past) |b| {
        b.release();
        return error.TestUnexpectedResult;
    } else |e| try testing.expect(e == error.ShortRead or e == error.InputOutput or e == error.InvalidArgument);
}
