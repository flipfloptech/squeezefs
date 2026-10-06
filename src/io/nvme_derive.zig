//! Portable half of the NVMe block-device worker (`docs/porting/nvme_dev.md`):
//! every pure derivation, codec-like classifier and lock-free primitive the
//! Linux-only `io/nvme_dev.zig` is built from. Nothing here touches a
//! syscall, so the tie tests run on every host.

const std = @import("std");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const counters = @import("../core/counters.zig");
const log = @import("../core/log.zig");

const testing = std.testing;

// ---------------------------------------------------------------------------
// Constants with their measured reasons (spec §9).
// ---------------------------------------------------------------------------

/// Per-lane request queue bound. "P1-6: bound io_uring request queue to
/// apply backpressure under overload" — a full queue refuses (`QueueFull`,
/// counted in `uring_queue_full`) instead of letting an overloaded lane grow
/// without bound. The queue also bounds the in-flight slot table, so a slot
/// index (the SQE `user_data`) is always `< uring_req_queue_cap`.
pub const uring_req_queue_cap: u32 = 4096;

/// Ring depth per lane (the reference's constant; the slot table is bounded
/// by the queue above, the SQ by this).
pub const ring_entries: u16 = 1024;

/// O_DIRECT / pooled-buffer contract (`POOLED_BUF_ALIGN`, pool.rs:28): the
/// kernel requires offset, length and buffer address aligned to the logical
/// block size; the code enforces 4096 on the buffer side only — offsets are
/// the caller's.
pub const dma_align: usize = 4096;

/// Whole-block pool slot (the striped block size, charter §6.1) and the
/// pooled read-size cap.
pub const block_buf_size: usize = 4 * derive.MiB;

/// Ranged read-bounce slot: "4 KiB ranged reads checking out 4 MiB backings
/// = 6 ms/op convoy (THP-zeroing + TLB storm)" (pool.rs:813); windows up to
/// this size ride the slab, larger ones the whole-block pool.
pub const ranged_buf_size: usize = 64 * derive.KiB;

/// SQ-full push retries before the worker refuses a request
/// ("Submission queue full"): each retry is a plain `submit()` that frees
/// the SQ independently of completions.
pub const sq_push_retries: u32 = 3;

/// Node-liveness probe TTL: a per-call `Path::exists()` was 0.76 statx/op
/// (the L3 transport-economy regression).
pub const node_probe_ttl_ms: u64 = 1000;

/// SQE `user_data` sentinels. Slot indices are small (`< uring_req_queue_cap`);
/// anything `>= UD_RESERVED` is never a request.
pub const UD_RESERVED: u64 = std.math.maxInt(u64) - 1;
/// The slow-read seam's linked TIMEOUT.
pub const UD_SEAM: u64 = std.math.maxInt(u64) - 1;
/// The armed eventfd READ that wakes a parked lane.
pub const UD_WAKE: u64 = std.math.maxInt(u64);

// ---------------------------------------------------------------------------
// Lane derivations (spec §1.1, §1.2).
// ---------------------------------------------------------------------------

/// Submission lanes per data device: `clamp(ceil(cpus / devices), 1, cpus)`.
///
/// WHY: one submitting thread per device = one blk-mq software queue = one
/// nvme-tcp connection = one RX softirq core; nvme-tcp RX copy ≈ 2.7 GB/s per
/// core ⇒ 8 namespaces × 1 core ≈ the measured 22 GB/s read wall.
/// Discriminator: 1 submitter/dev qd16→qd32 = 11.5→12.1 GB/s; 4 submitters
/// per device at the same in-flight = 16.7 GB/s (8 = 17.7). Writes: kernel
/// sequential writes walled at 35.31 GB/s = 7.06 GB/s × 5 namespaces while
/// fio with spread queues reached 49.7 GB/s. CEILING since 2026-08-14: a
/// fractional share rounds UP — a submission lane is cheap, the wall is not.
pub fn ioLanesFor(cpus: u32, data_devices: u32) u32 {
    const c: u64 = @max(cpus, 1);
    return @intCast(derive.clamp(derive.ceilDiv(cpus, @max(data_devices, 1)), 1, c));
}

pub const Lanes = struct { read: u32, write: u32 };

/// `explicit > derived`: `SQUEEZEFS_NVME_READ_LANES` / `_WRITE_LANES` win
/// verbatim (the gate already range-checked them to 1..=1024); absent
/// derives `ioLanesFor`.
pub fn resolveLanes(env: *const std.process.Environ.Map, cpus: u32, data_devices: u32) Lanes {
    const derived = ioLanesFor(cpus, data_devices);
    return .{
        .read = knobs.optIntKnob("SQUEEZEFS_NVME_READ_LANES", u32, env) orelse derived,
        .write = knobs.optIntKnob("SQUEEZEFS_NVME_WRITE_LANES", u32, env) orelse derived,
    };
}

/// Block-offset write-lane affinity: `(offset / grain) % lanes`.
///
/// WHY: same block → same lane → same queue FIFO → same connection, so two
/// DMAs naming one offset keep per-offset SUBMISSION order structurally
/// (completion order was never guaranteed: no IO_LINK/IO_DRAIN). Per-offset
/// serialization lives ABOVE the device layer.
pub fn writeLaneIndex(offset: u64, grain: u64, lanes: u32) u32 {
    return @intCast((offset / @max(grain, 1)) % @as(u64, @max(lanes, 1)));
}

// ---------------------------------------------------------------------------
// Pool geometry (pool.rs:319-327, 750-826).
// ---------------------------------------------------------------------------

/// Whole-block (4 MiB) pool capacity: `max(cores × 16, 64)`.
pub fn blockPoolCapacity(cores: u32) u32 {
    return @max(cores *| 16, 64);
}

/// Ranged (64 KiB) slab capacity: `max(cores × 64, 512)` — sized for
/// miss-path concurrency (hundreds of in-flight sub-block reads), not block
/// count; ≈ 92 MiB on a 23-CPU box.
pub fn rangedPoolCapacity(cores: u32) u32 {
    return @max(cores *| 64, 512);
}

pub const BouncePool = enum { ranged, block };

/// `read_bounce_pool(size)`: ≤ 64 KiB rides the registered ranged slab
/// (fixed-buffer eligible), larger the whole-block pool.
pub fn readBouncePool(size: usize) BouncePool {
    return if (size <= ranged_buf_size) .ranged else .block;
}

/// `(len + 4095) & !4095` — the bounce buffer length. The padded tail is
/// WRITTEN (zero-filled), so an unaligned-length write extends to the next
/// 4 KiB boundary.
pub fn paddedLen(len: usize) usize {
    return @intCast(derive.alignUp(len, dma_align));
}

/// The zero-copy branch condition: pointer AND length 4 KiB-aligned.
pub fn isDmaAligned(bytes: []const u8) bool {
    return derive.isAligned(@intFromPtr(bytes.ptr), dma_align) and derive.isAligned(bytes.len, dma_align);
}

// ---------------------------------------------------------------------------
// Write-cache class (write_cache.rs) — informational, never gates a mount.
// ---------------------------------------------------------------------------

pub const WriteCacheClass = enum(u8) {
    write_back = 0,
    write_through = 1,
    file_backed = 2,
    unknown = 3,

    /// `queue/write_cache` text, trimmed: exact match or `unknown` — never
    /// a guess.
    pub fn fromSysfs(text: []const u8) WriteCacheClass {
        const t = std.mem.trim(u8, text, " \t\r\n");
        if (std.mem.eql(u8, t, "write back")) return .write_back;
        if (std.mem.eql(u8, t, "write through")) return .write_through;
        return .unknown;
    }

    pub fn name(self: WriteCacheClass) []const u8 {
        return switch (self) {
            .write_back => "write-back",
            .write_through => "write-through",
            .file_backed => "file-backed",
            .unknown => "unknown",
        };
    }

    /// Conservative: only a declared write-through device pays nothing for
    /// a barrier; everything else is treated as volatile.
    pub fn isVolatile(self: WriteCacheClass) bool {
        return self != .write_through;
    }

    pub fn asU8(self: WriteCacheClass) u8 {
        return @backingInt(self);
    }

    pub fn fromU8(v: u8) WriteCacheClass {
        return std.enums.fromInt(WriteCacheClass, v) orelse .unknown;
    }
};

pub const SysfsPathBuf = [64]u8;
pub const SysfsQueueDir = enum { own, parent };

/// `/sys/dev/block/M:m/queue/write_cache`, or the parent disk's
/// (`/../queue`) for a partition, which has no `queue/` of its own.
pub fn sysfsWriteCachePath(buf: *SysfsPathBuf, major: u32, minor: u32, which: SysfsQueueDir) [:0]const u8 {
    // 64 bytes hold the longest form: two u32 majors/minors (10 digits each)
    // inside a 46-char template.
    return switch (which) {
        .own => std.mem.printSentinel(buf, "/sys/dev/block/{d}:{d}/queue/write_cache", .{ major, minor }, 0) catch unreachable,
        .parent => std.mem.printSentinel(buf, "/sys/dev/block/{d}:{d}/../queue/write_cache", .{ major, minor }, 0) catch unreachable,
    };
}

// ---------------------------------------------------------------------------
// Outcome classification (spec §1.6, §5, §12).
// ---------------------------------------------------------------------------

pub const ReadVerdict = union(enum) {
    full,
    /// Bytes the kernel actually returned.
    short: usize,
};

/// Exact-length contract (VL8 item 5): `got != size` is an error carrying
/// `got`, because a short read would otherwise surface recycled pool bytes
/// as data. File-backed substrates are where this happens (past-EOF); real
/// block devices are all-or-EIO.
pub fn finishRead(got: usize, size: usize) ReadVerdict {
    return if (got == size) .full else .{ .short = got };
}

/// Refusals: the request never reached the device.
pub const Refusal = error{
    /// Lane queue full or closed (backpressure); counted in `uring_queue_full`.
    QueueFull,
    /// The worker dropped its receiver (lane dead).
    WorkerClosed,
    /// Pooled read larger than the 4 MiB pool slot.
    ReadSizeExceedsPool,
    /// Fence latched / custody poisoned (errno class EIO).
    WriterGuardFenced,
    /// Dismount teardown completed (errno class EROFS).
    DismountRefused,
    /// O_DIRECT open failed — buffered device I/O is not a supported mode.
    DirectIoUnsupported,
};

/// Terminal: the device/kernel answered and the answer is final for this op.
pub const Terminal = error{
    DeviceIo,
    /// The kernel returned fewer bytes than asked (past-EOF on a file).
    ShortRead,
    /// EBADE — the block layer's `BLK_STS_RESV_CONFLICT`; latches the fence.
    ReservationConflict,
    NoSpace,
    /// Misaligned O_DIRECT shape or another EINVAL from the kernel.
    InvalidArgument,
    /// The worker could not push the SQE after `sq_push_retries`.
    SubmissionQueueFull,
    /// Teardown drained the request before it reached the ring.
    WorkerShutdown,
    OutOfMemory,
};

/// Retryable: nothing final happened. No timeout class: an op the device
/// never answers is OVERDUE (`OverdueCensus`), the watchdog's business
/// (charter §4), never a per-op error.
pub const Retryable = error{
    Interrupted,
    Again,
};

pub const DeviceError = Refusal || Terminal || Retryable;

pub const ErrorClass = enum { refusal, terminal, retryable };

pub fn classOf(e: DeviceError) ErrorClass {
    return switch (e) {
        error.QueueFull, error.WorkerClosed, error.ReadSizeExceedsPool, error.WriterGuardFenced, error.DismountRefused, error.DirectIoUnsupported => .refusal,
        error.DeviceIo, error.ShortRead, error.ReservationConflict, error.NoSpace, error.InvalidArgument, error.SubmissionQueueFull, error.WorkerShutdown, error.OutOfMemory => .terminal,
        error.Interrupted, error.Again => .retryable,
    };
}

/// `EBADE` (52): the block layer's mapping of `BLK_STS_RESV_CONFLICT`
/// (reservation.rs:462-469) — the ONE errno that fences.
pub fn isReservationConflict(e: std.os.linux.E) bool {
    return e == .BADE;
}

/// THE errno table, kernel → class (charter §12: errno mapping lives in
/// one table; `errnoFor` is its other half).
pub fn errnoToError(e: std.os.linux.E) DeviceError {
    return switch (e) {
        .BADE => error.ReservationConflict,
        .INVAL => error.InvalidArgument,
        .NOSPC => error.NoSpace,
        .INTR => error.Interrupted,
        .AGAIN => error.Again,
        .NOMEM => error.OutOfMemory,
        else => error.DeviceIo,
    };
}

/// Class → errno for the reply path: the fence class is EIO
/// (`WriterGuardFenced`, error.rs:361), dismount EROFS, refusals the
/// reference's `InvalidOperation` = EINVAL.
pub fn errnoFor(e: DeviceError) std.os.linux.E {
    return switch (e) {
        error.ReservationConflict => .BADE,
        error.WriterGuardFenced, error.DeviceIo, error.ShortRead, error.SubmissionQueueFull => .IO,
        error.DismountRefused => .ROFS,
        error.QueueFull, error.WorkerClosed, error.WorkerShutdown, error.ReadSizeExceedsPool, error.DirectIoUnsupported, error.InvalidArgument => .INVAL,
        error.NoSpace => .NOSPC,
        error.OutOfMemory => .NOMEM,
        error.Interrupted => .INTR,
        error.Again => .AGAIN,
    };
}

// ---------------------------------------------------------------------------
// The overdue census (charter §4: the watchdog logs overdue ops loudly;
// per-op timeout wrappers are banned; §1.8: a device that never answers
// fences the volume through the lattice).
// ---------------------------------------------------------------------------

/// The watchdog's view of one device's in-flight ops (`Device.overdueCensus`
/// folds every lane's submit stamps through `note`): how many are in
/// flight, how many have aged past the watchdog's bound, and the oldest's
/// age. Pure; the bound and the escalation are the watchdog's.
pub const OverdueCensus = struct {
    in_flight: u32 = 0,
    overdue: u32 = 0,
    oldest_age_ns: u64 = 0,

    pub fn note(self: *OverdueCensus, age_ns: u64, bound_ns: u64) void {
        self.in_flight += 1;
        if (age_ns >= bound_ns) self.overdue += 1;
        self.oldest_age_ns = @max(self.oldest_age_ns, age_ns);
    }
};

// ---------------------------------------------------------------------------
// Node-liveness probe cache (spec §3).
// ---------------------------------------------------------------------------

/// TTL-cached device-node liveness. Both words Relaxed: racing expirers
/// both re-probe and a torn `(seen, at)` pair self-heals within one TTL;
/// nothing is published through them.
pub const NodeProbe = struct {
    /// Coarse monotonic ms of the last probe; 0 = never probed.
    at_ms: std.atomic.Value(u64) = .init(0),
    seen: std.atomic.Value(bool) = .init(false),

    /// The cached verdict if it is fresh at `now_ms`, else null (re-probe).
    pub fn cached(self: *const NodeProbe, now_ms: u64) ?bool {
        const at = self.at_ms.load(.monotonic);
        if (at != 0 and now_ms -| at < node_probe_ttl_ms) return self.seen.load(.monotonic);
        return null;
    }

    /// `max(1)`: 0 is the never-probed sentinel; a probe inside the
    /// process's first millisecond must still record as probed.
    pub fn record(self: *NodeProbe, now_ms: u64, seen: bool) void {
        self.seen.store(seen, .monotonic);
        self.at_ms.store(@max(now_ms, 1), .monotonic);
    }
};

// ---------------------------------------------------------------------------
// Wake coalescer (wake_core.rs:90-102) — the Dekker pair with the queue.
// ---------------------------------------------------------------------------

/// Producer: publish the request (queue push) THEN `arm()`; only the arming
/// transition pays the eventfd write. Worker: consume the wake CQE →
/// `disarm()` → scan the queue. Both sides are SeqCst RMWs on purpose (the
/// reference's loom-verified protocol): the producer's push is ordered
/// before its arm and the worker's disarm before its scan, so a producer
/// that sees `armed == true` (and elides) is guaranteed that the pending
/// wake's consumer will scan after this push, and a worker whose scan found
/// the queue empty is guaranteed that any later push sees `armed == false`
/// and pays the write. A plain load+store would let both sides miss.
pub const WakeCoalescer = struct {
    armed: std.atomic.Value(bool) = .init(false),

    /// True when this caller must write the eventfd.
    pub fn arm(self: *WakeCoalescer) bool {
        return !self.armed.swap(true, .seq_cst);
    }

    pub fn disarm(self: *WakeCoalescer) void {
        _ = self.armed.swap(false, .seq_cst);
    }
};

// ---------------------------------------------------------------------------
// Bounded MPMC queue — the ONE implementation lives in `sync/mpmc.zig`; this
// is the lane's name for it (the caller → lane channel and the pool
// free-lists: producers `tryPush`, the consumer `tryPop`, nobody blocks —
// the single park is the ring's).
// ---------------------------------------------------------------------------

pub const BoundedMpmcQueue = @import("../sync/mpmc.zig").BoundedMpmc;

// ---------------------------------------------------------------------------
// Stats (spec §7). Counters are per-thread striped; histograms exact-sum;
// every "cannot happen" is a tripwire.
// ---------------------------------------------------------------------------

pub const stats = struct {
    /// Data-device READ ops completed through `readBlock` (not probes).
    pub var get_obj: counters.ShardedCounter = .{};
    /// `writeBlock` completions after `classifyDmaOutcome` passed.
    pub var put_obj: counters.ShardedCounter = .{};
    /// Lane queue refused (full or closed) on read, write or barrier submit.
    /// Expected 0 in healthy rows.
    pub var uring_queue_full: counters.ShardedCounter = .{};
    /// Writes that took the bounce-copy branch. Must stay 0 on aligned
    /// full-block workloads; only non-4 KiB-multiple payloads may move it.
    pub var nvme_unaligned_write_fallbacks: counters.ShardedCounter = .{};
    /// SQEs pushed (read/write/fsync), counted before the push attempt.
    pub var dev_fills: counters.ShardedCounter = .{};
    /// `io_uring_enter` calls: one per pass + each SQ-full `submit()`.
    pub var dev_enters: counters.ShardedCounter = .{};
    /// Passes whose enter carried ≥ 1 fresh fill (64 queued fills ride ≤ 4).
    pub var dev_submit_enters: counters.ShardedCounter = .{};
    /// Completion drains that resolved ≥ 1 request.
    pub var dev_wake_batches: counters.ShardedCounter = .{};
    /// eventfd writes paid vs elided by the coalescer.
    pub var dev_wake_writes: counters.ShardedCounter = .{};
    pub var dev_wakes_elided: counters.ShardedCounter = .{};
    /// Engagement gauge of the registered slab: reads that rode ReadFixed.
    pub var dev_fixed_buf_fills: counters.ShardedCounter = .{};
    /// `flush()` callers vs device Fsync ops actually issued.
    pub var data_device_sync_requests: counters.ShardedCounter = .{};
    pub var data_device_syncs: counters.ShardedCounter = .{};
    /// DMA refused because the fence latched / custody poisoned. Must stay
    /// 0 on healthy mounts.
    pub var data_dma_fence_refusals: counters.ShardedCounter = .{};
    /// DMA refused in the dismount class (EROFS); 0 on a serving mount.
    pub var data_dma_dismount_refusals: counters.ShardedCounter = .{};
    /// Whole-block pool reuse vs fresh mappings (moved by bounce allocs).
    pub var aligned_pool_hits: counters.ShardedCounter = .{};
    pub var aligned_pool_misses: counters.ShardedCounter = .{};
    /// Lock-wait census: callers currently parked on an unbounded device
    /// completion (writes, barriers) — the watchdog's "LOST DEVICE
    /// COMPLETION" instrument.
    pub var device_lock_waiters: counters.Gauge = .{};
    /// Read enqueue → SQE build (queue + slot wait) and SQE stamp → CQE.
    pub var read_fill_dev_queue_ns: counters.ShardedLatencyHistogram = .{};
    pub var read_fill_dev_service_ns: counters.ShardedLatencyHistogram = .{};
    /// Write twins (fsyncs record neither).
    pub var write_pipeline_dev_queue_ns: counters.ShardedLatencyHistogram = .{};
    pub var write_pipeline_dev_service_ns: counters.ShardedLatencyHistogram = .{};

    /// A CQE named a slot that holds no in-flight request.
    pub var cqe_orphan_slot: counters.Tripwire = .init("nvme_cqe_orphan_slot");
    /// The exit sweep found an in-flight slot after the loop proved
    /// `active_count == 0`.
    pub var exit_active_nonempty: counters.Tripwire = .init("nvme_exit_active_nonempty");
    /// The lane queue refused a push although a cell was held (cells and
    /// queue share one bound, so this cannot happen).
    pub var queue_push_with_cell: counters.Tripwire = .init("nvme_queue_push_refused_with_cell");
    /// A wake READ SQE could not be armed even after a flush.
    pub var wake_arm_failed: counters.Tripwire = .init("nvme_wake_arm_failed");
    /// A lane's `completed` watermark ran past its `submitted`.
    pub var watermark_underflow: counters.Tripwire = .init("nvme_watermark_completed_past_submitted");
};

fn writeHistJson(w: *std.Io.Writer, name: []const u8, queue: *const counters.ShardedLatencyHistogram, service: *const counters.ShardedLatencyHistogram) std.Io.Writer.Error!void {
    try w.print("\"{s}\":{{\"dev_queue\":", .{name});
    try queue.snapshot().writeJson(w);
    try w.writeAll(",\"dev_service\":");
    try service.snapshot().writeJson(w);
    try w.writeAll("}");
}

/// Process-wide families (every device shares them).
pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeAll("{");
    inline for (.{ "get_obj", "put_obj", "uring_queue_full", "nvme_unaligned_write_fallbacks", "dev_fills", "dev_enters", "dev_submit_enters", "dev_wake_batches", "dev_wake_writes", "dev_wakes_elided", "dev_fixed_buf_fills", "data_device_sync_requests", "data_device_syncs", "data_dma_fence_refusals", "data_dma_dismount_refusals", "aligned_pool_hits", "aligned_pool_misses" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    try w.print("\"device_lock_waiters\":{d},", .{stats.device_lock_waiters.load()});
    try writeHistJson(w, "read_fill_phase_ns", &stats.read_fill_dev_queue_ns, &stats.read_fill_dev_service_ns);
    try w.writeAll(",");
    try writeHistJson(w, "write_pipeline_phase_ns", &stats.write_pipeline_dev_queue_ns, &stats.write_pipeline_dev_service_ns);
    try w.writeAll("}");
}

// ---------------------------------------------------------------------------
// Fence latch, custody gate, dismount close (spec §5).
// ---------------------------------------------------------------------------

/// The per-volume `failed` latch the data plane shares with the reclaim
/// queue; bare devices (tests, offline tools) have none and never halt.
pub const FenceSignal = struct {
    ctx: ?*anyopaque,
    poll: *const fn (?*anyopaque) bool,
};

/// Custody hooks the data plane installs later (`data_custody`): `poison`
/// is told why the process-wide custody latch fell; `authorize` may add the
/// epoch check (`data_dma_epoch_refusals` lives there, not here).
pub const CustodyHooks = struct {
    ctx: ?*anyopaque,
    poison: *const fn (?*anyopaque, []const u8) void,
    authorize: ?*const fn (?*anyopaque) bool,
};

var custody_hooks: std.atomic.Value(?*const CustodyHooks) = .init(null);
/// Process-wide custody poison (the reference's `data_custody::POISONED`):
/// once a device fenced, NO data-plane DMA from this process is authorized
/// again — remount required.
var custody_poisoned: std.atomic.Value(bool) = .init(false);

pub fn installCustodyHooks(hooks: ?*const CustodyHooks) void {
    custody_hooks.store(hooks, .release);
}

/// A latch: only the false→true transition tells the data plane (every
/// later gated write re-derives the same verdict from the latch).
pub fn poisonCustody(why: []const u8) void {
    if (custody_poisoned.swap(true, .acq_rel)) return;
    if (custody_hooks.load(.acquire)) |h| h.poison(h.ctx, why);
}

/// `data_custody::authorize_dma`: poisoned ⇒ refused and counted.
pub fn authorizeDma() Refusal!void {
    if (custody_poisoned.load(.acquire)) {
        stats.data_dma_fence_refusals.inc();
        return error.WriterGuardFenced;
    }
    if (custody_hooks.load(.acquire)) |h| if (h.authorize) |auth| if (!auth(h.ctx)) {
        stats.data_dma_fence_refusals.inc();
        return error.WriterGuardFenced;
    };
}

/// Test seam: the poison latch is process-wide, so a suite that fenced one
/// device must clear it before the next device may write.
pub fn clearCustodyPoisonForTest() void {
    custody_poisoned.store(false, .release);
}

pub const Fence = struct {
    /// Sticky until remount. Relaxed fast-path load; the latch is an AcqRel
    /// swap so the first latcher (and only it) logs and poisons.
    halted: std.atomic.Value(bool) = .init(false),
    dismounted: std.atomic.Value(bool) = .init(false),
    /// Set once; later calls no-op.
    signal: std.atomic.Value(?*const FenceSignal) = .init(null),

    pub fn isHalted(self: *Fence, path: []const u8) bool {
        if (self.halted.load(.monotonic)) return true;
        const sig = self.signal.load(.acquire) orelse return false;
        if (!sig.poll(sig.ctx)) return false;
        if (!self.halted.swap(true, .acq_rel)) {
            log.err("data volume {s}: writer guard FENCED / volume fail-stopped — refusing ALL further data-plane DMA permanently (counted in data_dma_fence_refusals; remount required)", .{path});
        }
        return true;
    }
};

// ---------------------------------------------------------------------------
// Test seams (spec §2): SeqCst CAS-decrement counters, one relaxed load when
// disarmed.
// ---------------------------------------------------------------------------

pub const Seams = struct {
    fail_next_writes: std.atomic.Value(u32) = .init(0),
    /// `.SUCCESS` = classless injected failure (passes through untouched).
    fail_errno: std.atomic.Value(u32) = .init(0),
    stall_reads: std.atomic.Value(u32) = .init(0),
    stall_read_ms: std.atomic.Value(u64) = .init(0),
    stall_writes: std.atomic.Value(u32) = .init(0),
    stall_write_ms: std.atomic.Value(u64) = .init(0),
    slow_reads: std.atomic.Value(u32) = .init(0),
    slow_read_ms: std.atomic.Value(u64) = .init(0),
};

/// Seam CAS-decrement: true when this call consumed one armed count; one
/// relaxed load when disarmed.
pub fn takeArmed(counter: *std.atomic.Value(u32)) bool {
    var cur = counter.load(.monotonic);
    while (cur > 0) {
        cur = counter.cmpxchgWeak(cur, cur - 1, .seq_cst, .monotonic) orelse return true;
    }
    return false;
}

// ---------------------------------------------------------------------------
// Tests — the contract (spec §1.1, §1.2, §2.1, §3, §9, §10).
// ---------------------------------------------------------------------------

test "io_lanes_derivation_table: ceil(cpus / devices) clamped to [1, cpus]" {
    // Pinned table (spec §1.1, tests L549-571).
    try testing.expectEqual(@as(u32, 4), ioLanesFor(32, 8));
    try testing.expectEqual(@as(u32, 7), ioLanesFor(32, 5));
    try testing.expectEqual(@as(u32, 8), ioLanesFor(32, 4));
    try testing.expectEqual(@as(u32, 4), ioLanesFor(30, 8));
    try testing.expectEqual(@as(u32, 32), ioLanesFor(32, 1));
    try testing.expectEqual(@as(u32, 1), ioLanesFor(4, 8));
    try testing.expectEqual(@as(u32, 1), ioLanesFor(0, 0));
    // Formula restated: a fractional share rounds UP ("a submission lane is
    // cheap, the wall is not").
    try testing.expectEqual(derive.clamp(derive.ceilDiv(32, 5), 1, 32), ioLanesFor(32, 5));
    // Derivation-sweep tie (derivation_sweep_tests.rs:1500-1504): a 4-way
    // fleet share of 32 CPUs (effective parallelism 8) over 2 devices is a
    // quarter of the undivided lane count: 4 == 16 / 4.
    try testing.expectEqual(ioLanesFor(32, 2) / 4, ioLanesFor(8, 2));
}

test "resolveLanes: explicit knob wins verbatim, absent derives" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    const derived = resolveLanes(&env, 32, 4);
    try testing.expectEqual(@as(u32, 8), derived.read);
    try testing.expectEqual(@as(u32, 8), derived.write);
    try env.put("SQUEEZEFS_NVME_READ_LANES", "3");
    try env.put("SQUEEZEFS_NVME_WRITE_LANES", "1");
    const explicit = resolveLanes(&env, 32, 4);
    try testing.expectEqual(@as(u32, 3), explicit.read);
    try testing.expectEqual(@as(u32, 1), explicit.write);
}

test "write_lane_affinity_is_pure_and_block_stable" {
    const grain: u64 = 4 * derive.MiB;
    const lanes: u32 = 6;
    // Pure: same inputs, same lane.
    try testing.expectEqual(writeLaneIndex(5 * grain + 17, grain, lanes), writeLaneIndex(5 * grain + 17, grain, lanes));
    // Every offset inside a block maps to that block's lane.
    var rel: u64 = 0;
    while (rel < grain) : (rel += 4096) {
        try testing.expectEqual(writeLaneIndex(7 * grain, grain, lanes), writeLaneIndex(7 * grain + rel, grain, lanes));
    }
    // Six consecutive blocks cover all six lanes.
    var seen: [6]bool = @splat(false);
    var b: u64 = 0;
    while (b < lanes) : (b += 1) seen[writeLaneIndex(b * grain, grain, lanes)] = true;
    for (seen) |s| try testing.expect(s);
    // Degenerate inputs never divide by zero and never index out of range.
    try testing.expectEqual(writeLaneIndex(123, 1, 4), writeLaneIndex(123, 0, 4));
    try testing.expectEqual(@as(u32, 0), writeLaneIndex(std.math.maxInt(u64), grain, 1));
    try testing.expectEqual(@as(u32, 0), writeLaneIndex(42, grain, 0));
}

test "pool capacities restate pool.rs: max(cores×16, 64) blocks, max(cores×64, 512) ranged slots" {
    try testing.expectEqual(@as(u32, 64), blockPoolCapacity(1));
    try testing.expectEqual(@as(u32, 64), blockPoolCapacity(4));
    try testing.expectEqual(@as(u32, 512), blockPoolCapacity(32));
    try testing.expectEqual(@as(u32, 512), rangedPoolCapacity(1));
    try testing.expectEqual(@as(u32, 512), rangedPoolCapacity(8));
    try testing.expectEqual(@as(u32, 23 * 64), rangedPoolCapacity(23));
    // ≈ 92 MiB on a 23-CPU box (pool.rs:813).
    try testing.expectEqual(@as(u64, 23 * 64 * 64 * derive.KiB), @as(u64, rangedPoolCapacity(23)) * ranged_buf_size);
}

test "read_bounce_pool_routing: ≤ 64 KiB ranged slab, above it the whole-block pool" {
    try testing.expectEqual(BouncePool.ranged, readBouncePool(4096));
    try testing.expectEqual(BouncePool.ranged, readBouncePool(64 * derive.KiB));
    try testing.expectEqual(BouncePool.block, readBouncePool(64 * derive.KiB + 1));
    try testing.expectEqual(BouncePool.block, readBouncePool(4 * derive.MiB));
}

test "O_DIRECT alignment contract: 4 KiB on both ends; padded length rounds up" {
    try testing.expectEqual(@as(usize, 4096), paddedLen(1));
    try testing.expectEqual(@as(usize, 4096), paddedLen(123));
    try testing.expectEqual(@as(usize, 4096), paddedLen(4096));
    try testing.expectEqual(@as(usize, 8192), paddedLen(4097));
    try testing.expectEqual(@as(usize, 0), paddedLen(0));
    var slab: [3 * 4096]u8 align(4096) = undefined;
    try testing.expect(isDmaAligned(slab[0..8192]));
    try testing.expect(!isDmaAligned(slab[0..123]));
    try testing.expect(!isDmaAligned(slab[1..4097]));
}

test "write cache class: sysfs text is exact, never guessed; strings and u8 codec round-trip" {
    try testing.expectEqual(WriteCacheClass.write_back, WriteCacheClass.fromSysfs("write back\n"));
    try testing.expectEqual(WriteCacheClass.write_through, WriteCacheClass.fromSysfs("write through\n"));
    try testing.expectEqual(WriteCacheClass.write_through, WriteCacheClass.fromSysfs("  write through  "));
    try testing.expectEqual(WriteCacheClass.unknown, WriteCacheClass.fromSysfs(""));
    try testing.expectEqual(WriteCacheClass.unknown, WriteCacheClass.fromSysfs("writeback"));
    try testing.expectEqualStrings("write-back", WriteCacheClass.write_back.name());
    try testing.expectEqualStrings("write-through", WriteCacheClass.write_through.name());
    try testing.expectEqualStrings("file-backed", WriteCacheClass.file_backed.name());
    try testing.expectEqualStrings("unknown", WriteCacheClass.unknown.name());
    // Volatility is conservative: only a declared write-through device is
    // non-volatile.
    try testing.expect(WriteCacheClass.write_back.isVolatile());
    try testing.expect(!WriteCacheClass.write_through.isVolatile());
    try testing.expect(WriteCacheClass.file_backed.isVolatile());
    try testing.expect(WriteCacheClass.unknown.isVolatile());
    for ([_]WriteCacheClass{ .write_back, .write_through, .file_backed, .unknown }) |c| {
        try testing.expectEqual(c, WriteCacheClass.fromU8(c.asU8()));
    }
    try testing.expectEqual(WriteCacheClass.unknown, WriteCacheClass.fromU8(200));
}

test "sysfs queue dir candidates: the device's own queue, then the parent disk's" {
    var buf: SysfsPathBuf = undefined;
    try testing.expectEqualStrings("/sys/dev/block/259:3/queue/write_cache", sysfsWriteCachePath(&buf, 259, 3, .own));
    try testing.expectEqualStrings("/sys/dev/block/259:3/../queue/write_cache", sysfsWriteCachePath(&buf, 259, 3, .parent));
}

test "finishRead: exact-length contract — a short read is an error, never recycled bytes" {
    try testing.expectEqual(ReadVerdict{ .full = {} }, finishRead(4096, 4096));
    try testing.expectEqual(ReadVerdict{ .short = 0 }, finishRead(0, 4096));
    try testing.expectEqual(ReadVerdict{ .short = 2048 }, finishRead(2048, 4096));
    // The kernel never returns more than asked; if it did, that is a
    // short-read class too (the caller asked for `size`).
    try testing.expectEqual(ReadVerdict{ .short = 8192 }, finishRead(8192, 4096));
}

test "errno table: EBADE is the reservation-conflict (fence) class; classes are typed" {
    try testing.expect(isReservationConflict(.BADE));
    try testing.expect(!isReservationConflict(.IO));
    try testing.expectEqual(DeviceError.ReservationConflict, errnoToError(.BADE));
    try testing.expectEqual(DeviceError.DeviceIo, errnoToError(.IO));
    try testing.expectEqual(DeviceError.InvalidArgument, errnoToError(.INVAL));
    try testing.expectEqual(DeviceError.NoSpace, errnoToError(.NOSPC));
    try testing.expectEqual(DeviceError.Interrupted, errnoToError(.INTR));
    try testing.expectEqual(DeviceError.Again, errnoToError(.AGAIN));
    try testing.expectEqual(ErrorClass.terminal, classOf(error.ReservationConflict));
    try testing.expectEqual(ErrorClass.terminal, classOf(error.DeviceIo));
    try testing.expectEqual(ErrorClass.terminal, classOf(error.InvalidArgument));
    try testing.expectEqual(ErrorClass.retryable, classOf(error.Again));
    try testing.expectEqual(ErrorClass.refusal, classOf(error.QueueFull));
    try testing.expectEqual(ErrorClass.refusal, classOf(error.WriterGuardFenced));
    try testing.expectEqual(ErrorClass.refusal, classOf(error.DismountRefused));
    // The reply half: fence ⇒ EIO, dismount ⇒ EROFS, EBADE is 52 on the
    // generic table, refusals ⇒ EINVAL.
    try testing.expectEqual(std.os.linux.E.IO, errnoFor(error.WriterGuardFenced));
    try testing.expectEqual(std.os.linux.E.ROFS, errnoFor(error.DismountRefused));
    try testing.expectEqual(std.os.linux.E.BADE, errnoFor(error.ReservationConflict));
    try testing.expectEqual(std.os.linux.E.INVAL, errnoFor(error.QueueFull));
    // Every kernel errno round-trips through its class to a reply errno.
    for ([_]std.os.linux.E{ .BADE, .INVAL, .NOSPC, .INTR, .AGAIN, .NOMEM }) |e| try testing.expectEqual(e, errnoFor(errnoToError(e)));
}

test "overdue census: counts every in-flight op, those at or past the bound as overdue, and keeps the oldest age; nothing is timed out" {
    var c: OverdueCensus = .{};
    try testing.expectEqual(@as(u32, 0), c.in_flight);
    c.note(10, 100);
    c.note(100, 100);
    c.note(250, 100);
    try testing.expectEqual(@as(u32, 3), c.in_flight);
    try testing.expectEqual(@as(u32, 2), c.overdue);
    try testing.expectEqual(@as(u64, 250), c.oldest_age_ns);
    c.note(5, 100);
    try testing.expectEqual(@as(u32, 4), c.in_flight);
    try testing.expectEqual(@as(u32, 2), c.overdue);
    try testing.expectEqual(@as(u64, 250), c.oldest_age_ns);
}

test "node probe TTL: fresh within 1 s, re-probes after, 0 is the never-probed sentinel" {
    var p: NodeProbe = .{};
    try testing.expectEqual(@as(?bool, null), p.cached(0));
    p.record(0, true);
    // A probe inside the process's first millisecond still records as probed.
    try testing.expectEqual(@as(u64, 1), p.at_ms.load(.monotonic));
    try testing.expectEqual(@as(?bool, true), p.cached(500));
    try testing.expectEqual(@as(?bool, true), p.cached(1000));
    try testing.expectEqual(@as(?bool, null), p.cached(1001));
    p.record(5000, false);
    try testing.expectEqual(@as(?bool, false), p.cached(5999));
    try testing.expectEqual(@as(?bool, null), p.cached(6000));
}

test "wake coalescer: first arm pays, re-arms elide until the worker disarms" {
    var c: WakeCoalescer = .{};
    try testing.expect(c.arm());
    try testing.expect(!c.arm());
    try testing.expect(!c.arm());
    c.disarm();
    try testing.expect(c.arm());
}

test "fence: no signal never halts; a true signal latches once (one loud log) and stays sticky; dismount is a separate latch" {
    var f: Fence = .{};
    try testing.expect(!f.isHalted("/dev/test"));
    const Sig = struct {
        var fail: bool = false;
        fn poll(_: ?*anyopaque) bool {
            return fail;
        }
    };
    Sig.fail = false;
    const sig = FenceSignal{ .ctx = null, .poll = Sig.poll };
    _ = f.signal.cmpxchgStrong(null, &sig, .release, .monotonic);
    try testing.expect(!f.isHalted("/dev/test"));
    Sig.fail = true;
    log.testing.expectErrorLogs(1);
    try testing.expect(f.isHalted("/dev/test"));
    try testing.expect(f.isHalted("/dev/test"));
    try log.testing.expectAllFired();
    // Sticky: a signal that clears does not un-fence.
    Sig.fail = false;
    try testing.expect(f.isHalted("/dev/test"));
    try testing.expect(!f.dismounted.load(.acquire));
}

test "custody: poison latches once and tells the hook once; authorize refuses and counts while poisoned; the hook may veto" {
    defer clearCustodyPoisonForTest();
    defer installCustodyHooks(null);
    const Hook = struct {
        var fired: u32 = 0;
        var veto: bool = false;
        fn poison(_: ?*anyopaque, why: []const u8) void {
            fired += 1;
            std.debug.assert(why.len > 0);
        }
        fn authorize(_: ?*anyopaque) bool {
            return !veto;
        }
    };
    Hook.fired = 0;
    Hook.veto = false;
    const hooks = CustodyHooks{ .ctx = null, .poison = Hook.poison, .authorize = Hook.authorize };
    installCustodyHooks(&hooks);
    try authorizeDma();
    const r0 = stats.data_dma_fence_refusals.load();
    poisonCustody("test poison");
    poisonCustody("test poison again");
    try testing.expectEqual(@as(u32, 1), Hook.fired);
    try testing.expectError(error.WriterGuardFenced, authorizeDma());
    try testing.expectError(error.WriterGuardFenced, authorizeDma());
    try testing.expectEqual(r0 + 2, stats.data_dma_fence_refusals.load());
    clearCustodyPoisonForTest();
    try authorizeDma();
    // The data plane's own verdict (epoch mismatch) is a fence refusal too.
    Hook.veto = true;
    try testing.expectError(error.WriterGuardFenced, authorizeDma());
    try testing.expectEqual(r0 + 3, stats.data_dma_fence_refusals.load());
}

test "seams: CAS-decrement consumes exactly the armed count; disarmed is one relaxed load" {
    var seams: Seams = .{};
    try testing.expect(!takeArmed(&seams.stall_reads));
    seams.stall_reads.store(2, .seq_cst);
    try testing.expect(takeArmed(&seams.stall_reads));
    try testing.expect(takeArmed(&seams.stall_reads));
    try testing.expect(!takeArmed(&seams.stall_reads));
    try testing.expect(!takeArmed(&seams.fail_next_writes));
}

test "stats JSON names every family the spec lists" {
    var buf: [8192]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const json = w.buffered();
    for ([_][]const u8{ "get_obj", "put_obj", "uring_queue_full", "nvme_unaligned_write_fallbacks", "dev_fills", "dev_enters", "dev_submit_enters", "dev_wake_batches", "dev_wake_writes", "dev_wakes_elided", "dev_fixed_buf_fills", "data_device_sync_requests", "data_device_syncs", "data_dma_fence_refusals", "data_dma_dismount_refusals", "aligned_pool_hits", "aligned_pool_misses", "device_lock_waiters", "read_fill_phase_ns", "write_pipeline_phase_ns", "dev_queue", "dev_service" }) |key| {
        if (std.mem.indexOf(u8, json, key) == null) {
            std.debug.print("missing stats key {s}\n", .{key});
            return error.TestExpectedEqual;
        }
    }
    try testing.expect(json[0] == '{' and json[json.len - 1] == '}');
}

test "user_data sentinels: slot indices are small; anything ≥ UD_RESERVED is never a request" {
    try testing.expectEqual(std.math.maxInt(u64), UD_WAKE);
    try testing.expectEqual(std.math.maxInt(u64) - 1, UD_SEAM);
    try testing.expectEqual(UD_SEAM, UD_RESERVED);
    try testing.expect(UD_RESERVED > @as(u64, uring_req_queue_cap));
}
