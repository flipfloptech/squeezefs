//! Block reclaim: terminal frees, discard ELISION, the background
//! reclaimer and the debt drainer (charter §6.2 "Block reclaim";
//! `docs/porting/data_alloc_reclaim.md` §3, §10 Q4–Q7, §11 item 14).
//! PORTABLE core — the device commands reach a `Target` vtable (the Linux
//! binding over `io/nvme_dev.Device.discardRange` is `deviceTarget`); every
//! wait is on `std.Io` (the worker and the drainer are `io.concurrent`
//! tasks; the parks are `sync.Notify` deadline waits — no sleep is ever a
//! synchronization, Q6).
//!
//! The terminal-free ladder: `beginFree` (retire the incarnation) → tier
//! purge (ONE helper over all four read tiers, after the retire, before
//! reallocatable: a straggler validated fill either landed before the
//! purge or fails its own `still`) → the DURABILITY HOLD (below) → ELIDE on
//! a block device while `debt ≤
//! virgin_bytes` (`recordElidedDebt` + `finishFree`, ZERO device commands)
//! or ENQUEUE (the queue owns `finishFree`: a block becomes reallocatable
//! only after its discard landed, so a new owner's DMA can never race it).
//! WHY elision is correct: discard is substrate hygiene, never correctness
//! — unmapped blocks read zeros and reuse is guarded by write-before-publish
//! plus the incarnation word (RT:L2007). WHY bdev-only: on a raw block
//! device `fallocate(PUNCH_HOLE)` is `blkdev_issue_zeroout` — a full block
//! of Write-Zeroes per freed block (2.000× write amplification on the rig,
//! 1.85× at 98 % util in the field); regular-file backings keep the punch
//! because the host-FS space return is a REAL ENOSPC vector (KD-4.1). WHY
//! the watermark `debt ≤ virgin`: while the virgin tail covers the debt the
//! thin exposure is bounded by what fresh-minting would have consumed anyway
//! (KD-4.6, no constants).
//!
//! THE DURABILITY HOLD (charter §6.2; decision 0003 §BT): a block freed by
//! a metadata change returns to allocation — and to the device's discard —
//! only once the record that freed it is DURABLE (landed AND covered by a
//! completed barrier on every metadata member). Under the ack posture
//! (§5.2) a truncate, a punch or an unlink is answered at its RAM apply and
//! its window reaches stable media up to one flush interval later; a block
//! handed on at the apply took another file's bytes while the old file's
//! durable map still named it, and a power cut in between replayed that
//! map over them (the smoke row `A FREED BLOCK IS NOT REUSED BEFORE THE
//! RECORD THAT FREED IT IS DURABLE`: the truncated file read 4 MiB of the
//! next writer's bytes). ext4 holds its freed blocks until the transaction
//! that freed them commits; this is the same law. Mechanism: a terminal
//! free (after the retire and the purge, so stale readers fail as before)
//! is pushed on the OPEN list under the hold's lock; a release takes the
//! lock, and when the SEALED list is empty moves the open list there and
//! has the gate snapshot every member's reservation high-water mark — every
//! commit answered before the snapshot lies below it, and every free on the
//! sealed list was pushed after its commit's answer — then hands the sealed
//! list to the ladder's device side once each member's durable floor (the
//! landed floor of its last completed barrier) passes its mark. Releases
//! ride the producer's own free, the worker's manners tick while frees are
//! held, and the ENOSPC valve's drain, which FORCES a barrier first (a full
//! store never answers ENOSPC while frees wait for one). A held free is
//! supply in sight: `df` and the write admission count it free
//! (`BlockAllocator.durable_held`), the valve's `pending` sees it. A
//! non-holder's shipped free is held the same way before it ships. The
//! model: `free_hold_model.zig` (the seal outside the lock, and a release
//! on the LANDED floor, both fail).
//!
//! The queue: pop-ownership exactly-once (whoever pops processes — the
//! worker, the valve drain, the unmount drain; nothing is ever re-queued);
//! `population = len + processing` is what the cap governs (a pop opens no
//! room, a reclaimed RANGE does — the W-4 fix); PARK-DON'T-SPILL: an at-cap
//! enqueue parks on the room edge, registration-before-recheck, bounded by
//! `clamp(4 × batch ÷ measured drain rate, 50 ms, 1 s)`, and device
//! commands NEVER issue from the enqueue context (the inline at-cap arm
//! was a 12–22 ms synchronous fabric round trip on the handler lane). THE
//! MANNERS LAW: foreground device-byte movement + queue below cap ⇒ DEFER
//! (zero commands); at cap ⇒ drain (parked enqueues are foreground writers
//! too); idle ⇒ full width to empty (the field's 128 GiB backlog clears in
//! ≈ 12 s); thin supply (`queued × 2 ≥ free supply`) outranks manners and
//! latches the drain to EMPTY (the ratio is a trigger, never a per-pass
//! gate). WHY: the target-side deallocate service is the drain ceiling
//! (idle ≈ 2,700 cmd/s at width 32, ≈ 5,900 at 128; ≈ 1,700 under
//! foreground write load at ANY width), so aggressiveness under load buys
//! nothing and steals target CPU; the fresh row after a 128 GiB `rm` paid
//! −17 %. Lanes: demand-derived, grouped by device, consecutive slices of
//! the offset-sorted group so adjacency runs survive into each lane's
//! coalescer; one batch of 64 adjacent blocks is ONE device command.
//!
//! The reclaimer observes the writer-guard fence latch ONCE per batch
//! before any device command: a fenced zombie never discards (the
//! successor's recovery owns the accounting — the kill-9 posture); a
//! dismounted volume likewise (a discard after the teardown's own drain
//! would land on a block the next mount re-carved). Crash: the queue is
//! RAM-only space-return work; the only loss is un-returned thin space,
//! re-covered on reuse or by `trim --full`.
//!
//! The debt drainer is PACED TO THE DEVICE: its trim passes feed their
//! own completion-rate meter (`debt_meter`, the serial deallocate rate
//! the target answers), a pressure pass under a foreground stream issues
//! what the device completes in one manners tick (`debtBatchBlocks`, so a
//! pass's trim claim window holds free supply out of the carvable set for
//! ≈ one tick, never seconds) and rests one batch-time at that rate
//! (`debtPaceMs`) — the shipped `progressed ⇒ at once` looped hot behind
//! the write lane on a wrapped volume (the holder's virgin tail is 0 once
//! the frontier reaches the top, so every byte of debt is "past the
//! watermark" and the drain never deferred), and the fruitless backoff
//! was a fixed `50 << n` ladder capped at 1 Hz (rung 9 F17).

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const builtin = @import("builtin");
const Io = std.Io;
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;
const linux = std.os.linux;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");
const knobs = @import("../core/knobs.zig");
const errors = @import("../core/errors.zig");
const derive = @import("../core/derive.zig");
const jc = @import("../meta/journal_core.zig");
const mpmc = @import("../sync/mpmc.zig");
const sync_notify = @import("../sync/notify.zig");
const block_alloc = @import("block_alloc.zig");
const nvme_dev = if (builtin.os.tag == .linux) @import("../io/nvme_dev.zig") else struct {};

pub const BlockAllocator = block_alloc.BlockAllocator;
pub const InflightAllocGuard = block_alloc.InflightAllocGuard;

// ---------------------------------------------------------------------------
// Substrate classification (`free_reclaim_op(st_mode)`).
// ---------------------------------------------------------------------------

pub const Substrate = enum(u8) {
    /// A regular file: `fallocate(PUNCH_HOLE | KEEP_SIZE)`.
    file_punch,
    /// A block device: discard (NVMe DSM Deallocate — no payload, not
    /// write-bandwidth-accounted).
    bdev_discard,
    /// Anything else: never a zeroing write — the free is counted skipped.
    skip,
};

pub fn classifySubstrate(st_mode: u32) Substrate {
    if (linux.S.ISREG(st_mode)) return .file_punch;
    if (linux.S.ISBLK(st_mode)) return .bdev_discard;
    return .skip;
}

// ---------------------------------------------------------------------------
// Constants and derivations (BR:L186-305; BM-2026-09-05 §3).
// ---------------------------------------------------------------------------

/// The worker's deferred-drain re-evaluation cadence and the park bound's
/// floor: a producer that saw no room edge for one tick is waiting on a
/// drain the worker has not even re-judged yet.
pub const MANNERS_TICK_MS: u64 = 50;
/// The shipped posture; never regress below it (charter §1.5 floors).
pub const QUEUE_CAP_FLOOR_BLOCKS: u64 = 4096;
/// The derived ceiling never exceeds what an operator may set (the registry
/// row's range).
pub const QUEUE_CAP_REGISTRY_MAX_BLOCKS: u64 = 1 << 20;
/// Entries are bookkeeping only (the bytes live on the device): 0.1 % of
/// the budget is negligible by construction, never an R5 component.
pub const QUEUE_RAM_SHARE_DIVISOR: u64 = 1024;
/// A bound below the worker's guaranteed cadence could trip on a healthy
/// worker whose wake sat behind a saturated blocking pool.
pub const PARK_BOUND_FLOOR_MS: u64 = MANNERS_TICK_MS;
/// The shipped constant: never park LONGER than shipped; the cold (no
/// drain measured yet) value.
pub const PARK_BOUND_CEILING_MS: u64 = 1000;
/// A producer that saw no room edge in FOUR batch-times waits on a STALLED
/// drain, not a slow one: room is made per range, so a live drain at any
/// rate frees a batch within one batch-time.
pub const PARK_BOUND_BATCHES: u64 = 4;
/// The drain-rate window: two orders above a device command, re-learns a
/// slowed target in a few windows. The FIRST window is one manners tick —
/// a cold queue must learn inside the worker's cadence (p99.9 285 ms at a
/// 250 ms first window vs an 80 ms derived bound).
pub const DRAIN_RATE_WINDOW_MS: u64 = 250;
/// A single flat 50 ms tick is NOT idle: rows have sub-second lulls (fsync
/// barriers, per-file closes) the zero-mid-row-discards gate counts as
/// "during the row". 20 ticks = 1 s.
pub const IDLE_CONFIRM_TICKS: u64 = 20;

pub const DEFAULT_BATCH_BLOCKS: u32 = knobs.intDefault("SQUEEZEFS_RECLAIM_BATCH_BLOCKS", u32);
pub const DEFAULT_BATCH_MS: u64 = knobs.intDefault("SQUEEZEFS_RECLAIM_BATCH_MS", u64);
pub const DEFAULT_LANES_PER_DEV: u32 = knobs.intDefault("SQUEEZEFS_RECLAIM_LANES_PER_DEV", u32);

/// `room_ms = batch × 1000 ÷ drain_rate` (blocks/s); a cold queue reads
/// the ceiling.
pub fn roomMs(batch: u64, drain_rate: u64) u64 {
    if (drain_rate == 0) return PARK_BOUND_CEILING_MS;
    return @max(1, batch * 1000 / drain_rate);
}

/// The debt drainer's pressure batch: what the device completes in ONE
/// manners tick at its measured deallocate rate, `clamp(rate × tick, 1,
/// batch)` — a pass's trim claim window (free supply out of the carvable
/// set until the discards land) never outlives a tick. A cold meter (no
/// window rolled) issues the configured batch: the first window is one
/// tick, so it learns inside the first pass.
pub fn debtBatchBlocks(drain_rate: u64, batch: u32) u32 {
    if (drain_rate == 0) return batch;
    return @intCast(derive.clamp(drain_rate * MANNERS_TICK_MS / 1000, 1, batch));
}

/// The debt drainer's pace under foreground load, and after a fruitless
/// pass: one batch-time at the device's measured completion rate,
/// `clamp(room, tick, ceiling)` — issue a batch, rest a batch-time, so
/// the deallocate stream takes at most half the target's service under a
/// foreground stream (the aggressive drain bought nothing and cost the
/// foreground 17 % in the 128 GiB `rm` row) and a cold meter rests the
/// shipped ceiling until it learns.
pub fn debtPaceMs(batch: u64, drain_rate: u64) u64 {
    return derive.clamp(roomMs(batch, drain_rate), MANNERS_TICK_MS, PARK_BOUND_CEILING_MS);
}

/// A completion-rate meter in blocks/s: the window's `blocks × 1000 ÷ dt`
/// rolls into the α = ¼ EWMA; the FIRST window is one manners tick (a
/// cold meter learns inside the worker's cadence — p99.9 285 ms at a
/// 250 ms first window vs an 80 ms derived bound), every later one
/// `DRAIN_RATE_WINDOW_MS`. One per venue: the queue's lanes and the debt
/// drainer's trim passes complete at different widths.
const RateMeter = struct {
    rate: Atomic(u64) = .init(0),
    win_start_ms: Atomic(u64) = .init(0),
    win_blocks: Atomic(u64) = .init(0),
    first_done: Atomic(bool) = .init(false),

    /// Note `k` completions at `now_ms`; true when a window ROLLED (the
    /// rate moved — the caller rederives what hangs on it).
    fn note(self: *RateMeter, k: u64, now_ms: u64) bool {
        const win_start = self.win_start_ms.load(.monotonic);
        if (win_start == 0) {
            self.win_start_ms.store(@max(now_ms, 1), .monotonic);
            _ = self.win_blocks.fetchAdd(k, .monotonic);
            return false;
        }
        const blocks = self.win_blocks.fetchAdd(k, .monotonic) + k;
        const window = if (self.first_done.load(.monotonic)) DRAIN_RATE_WINDOW_MS else MANNERS_TICK_MS;
        if (now_ms - win_start < window) return false;
        if (self.win_start_ms.cmpxchgStrong(win_start, @max(now_ms, 1), .acq_rel, .monotonic) != null) return false;
        self.win_blocks.store(0, .monotonic);
        self.first_done.store(true, .monotonic);
        const rate = blocks * 1000 / @max(now_ms - win_start, 1);
        const prev = self.rate.load(.monotonic);
        self.rate.store(if (prev == 0) rate else ewmaQuarter(prev, rate), .monotonic);
        return true;
    }

    fn load(self: *const RateMeter) u64 {
        return self.rate.load(.monotonic);
    }
};

/// `clamp(4 × room, 50, 1000)`.
pub fn parkBoundMs(room_ms: u64) u64 {
    return derive.clamp(PARK_BOUND_BATCHES * room_ms, PARK_BOUND_FLOOR_MS, PARK_BOUND_CEILING_MS);
}

/// RAM one queued entry pins: the entry plus its queue slot (no device-path
/// heap string — the entry carries `vol: u16`, Q5).
pub fn entryRamBytes() u64 {
    return @sizeOf(Entry) + @sizeOf(u32);
}

/// `clamp(budget ÷ 1024 ÷ entry_ram, 4096, 2^20)`.
pub fn capCeiling(budget_bytes: u64) u64 {
    return derive.clamp(budget_bytes / QUEUE_RAM_SHARE_DIVISOR / entryRamBytes(), QUEUE_CAP_FLOOR_BLOCKS, QUEUE_CAP_REGISTRY_MAX_BLOCKS);
}

/// `clamp(arrival × room ÷ 1000, 4096, cap_ceiling(budget))` in u128 (an
/// arrival rate × a cold 1 s room overflows no word).
pub fn queueCap(arrival_rate: u64, room_ms: u64, budget_bytes: u64) u64 {
    const raw: u128 = @as(u128, arrival_rate) * room_ms / 1000;
    const ceiling = capCeiling(budget_bytes);
    return derive.clamp(@intCast(@min(raw, std.math.maxInt(u64))), QUEUE_CAP_FLOOR_BLOCKS, ceiling);
}

/// `explicit > derived`, verbatim (the gate refused out-of-range).
pub fn resolveQueueCapBlocks(explicit: ?u64, derived: u64) u64 {
    return explicit orelse derived;
}

pub fn resolveParkBoundMs(explicit: ?u64, derived: u64) u64 {
    return explicit orelse derived;
}

/// The drain-rate EWMA, α = ¼: `prev − prev/4 + rate/4` per window roll.
pub fn ewmaQuarter(prev: u64, rate: u64) u64 {
    return prev - prev / 4 + rate / 4;
}

/// Arrival peak-hold: the windowed rate or the previous peak decayed by ⅛
/// — a storm registers on its first window, a lull relaxes over ≈ 8.
pub fn peakHold(prev_peak: u64, windowed: u64) u64 {
    return @max(windowed, prev_peak - prev_peak / 8);
}

/// `n_lanes = clamp(ceil(demand ÷ batch), 1, lanes_per_dev)`: a shallow
/// queue stays one lane, a storm overlaps up to `lanes_per_dev` fabric
/// round trips per device.
pub fn laneCount(demand: u64, batch: u64, lanes_per_dev: u64) u64 {
    return derive.clamp(derive.ceilDiv(demand, @max(batch, 1)), 1, @max(lanes_per_dev, 1));
}

/// Consecutive slices of the sorted group: `ceil(len ÷ n_lanes)`.
pub fn chunkSize(group_len: u64, n_lanes: u64) u64 {
    return derive.ceilDiv(group_len, @max(n_lanes, 1));
}

/// KD-4.6: elide while the virgin tail covers the debt.
pub fn debtWithinWatermark(debt_bytes: u64, virgin_bytes: u64) bool {
    return debt_bytes <= virgin_bytes;
}

/// `elide_reclaim_for`: lever ON ∧ (block device ∨ the class-all seam).
pub fn elideReclaimFor(lever_on: bool, substrate: Substrate, class_all_seam: bool) bool {
    return lever_on and (substrate == .bdev_discard or class_all_seam);
}

/// Finding 40: any device with `queued × 2 ≥ free supply` reads pressure;
/// capacity-unbounded allocators never do.
pub fn supplyPressure(queued: u64, free_supply_blocks: u64, capacity_bounded: bool) bool {
    return capacity_bounded and queued * 2 >= free_supply_blocks;
}

// ---------------------------------------------------------------------------
// The manners law (inputs as a struct; a pure verdict).
// ---------------------------------------------------------------------------

pub const MannersInputs = struct {
    /// The device-byte signal moved since the previous worker decision.
    foreground_active: bool,
    /// `population ≥ cap`.
    at_cap: bool,
    queue_len: u64,
    /// Finding 40 on any device.
    supply_pressure: bool,
    /// The hysteresis: a pressure drain runs to EMPTY.
    pressure_latched: bool,
};

pub const Verdict = enum {
    /// Idle: full width to empty.
    drain_full_width,
    /// At cap, or under pressure: drain regardless of foreground.
    drain,
    /// Foreground + below cap + no pressure, queue non-empty: zero commands
    /// this tick.
    defer_,
    /// Nothing queued: park on the wake.
    park,
};

pub fn mannersVerdict(in: MannersInputs) Verdict {
    if (in.at_cap) return .drain;
    if (in.pressure_latched or in.supply_pressure) return .drain;
    if (in.foreground_active) return if (in.queue_len == 0) .park else .defer_;
    return if (in.queue_len == 0) .park else .drain_full_width;
}

// ---------------------------------------------------------------------------
// Coalescing.
// ---------------------------------------------------------------------------

pub const Range = struct { start: u64, len: u64, k: u32 };

/// Adjacent iff `start + len == next.offset` over OFFSET-sorted entries of
/// one device; counters stay per block when ranges merge (`blocks ÷
/// commands` is the live coalesce factor).
pub fn coalesceRanges(offsets: []const u64, size: u64, out: []Range) []Range {
    var n: usize = 0;
    for (offsets) |off| {
        if (n > 0 and out[n - 1].start + out[n - 1].len == off) {
            out[n - 1].len += size;
            out[n - 1].k += 1;
        } else {
            out[n] = .{ .start = off, .len = size, .k = 1 };
            n += 1;
        }
    }
    return out[0..n];
}

// ---------------------------------------------------------------------------
// Seams.
// ---------------------------------------------------------------------------

/// One device's discard face: the substrate class decides the command; the
/// issue path is the device's own lane (`deviceTarget`) or a test double.
pub const Target = struct {
    ctx: ?*anyopaque,
    substrate: Substrate,
    issue: *const fn (?*anyopaque, start: u64, len: u64) errors.Error!void,
};

/// The read tiers' ONE purge helper (charter §6.3) — the read path installs
/// it; absent, nothing is cached (rung 4 wave 1).
pub const TierPurge = struct {
    ctx: ?*anyopaque,
    purge: *const fn (?*anyopaque, vol: u16, idx: u64) void,
};

/// `fence_signal`: the volume's `failed` latch (the device fence AND the
/// meta set's `is_failed`, composed by the mount).
pub const FenceProbe = struct {
    ctx: ?*anyopaque,
    poll: *const fn (?*anyopaque) bool,
};

/// The device-activity signal: `write_through_bytes + patch_write_bytes +
/// staging_put_bytes + ranged_read_bytes + device_true_reads + hot_block_
/// misses` — DEVICE-byte movement, never op counts (a 1 Hz stats poller or
/// RAM-served reads hold nothing deferred). Wrapping sum, relaxed.
pub const ForegroundSignal = struct {
    ctx: ?*anyopaque,
    read: *const fn (?*anyopaque) u64,
};

/// Rung 6 (`sym_data_planes.md` §4.1; `DAR §11.7`): the terminal ladder's
/// step 1 on a grant-armed NON-holder — the free's DURABLE effect already
/// rode the displacing publish; the ACCOUNTING ladder whose durable home
/// is the holder's ledger SHIPS to the allocation holder. `ship` hands
/// the block to the shipper (`false` ⇒ no target reachable: leak-safe,
/// counted by the shipper); `authorityActive` is the holder's own
/// executor running the ladder locally with its accounting authority
/// (never ambiently active).
pub const FreeTarget = struct {
    ctx: *anyopaque,
    ship: *const fn (*anyopaque, vol_tag: u64, idx: u64) bool,
    authorityActive: *const fn (*anyopaque) bool,
};

/// The metadata set's durability as the hold sees it (module doc). `seal`
/// snapshots, into the gate's ONE sealed slot, every member's mark that
/// each commit answered before the call lies below — called only under the
/// hold's lock; `sealed` answers whether completed barriers cover that
/// snapshot; `force` pulls a barrier on every member and waits for it
/// (false: a member FAILED — nothing more becomes durable this mount).
pub const DurableGate = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        seal: *const fn (*anyopaque) void,
        sealed: *const fn (*anyopaque) bool,
        force: *const fn (*anyopaque) bool,
    };
};

// ---------------------------------------------------------------------------
// Stats (spec §7).
// ---------------------------------------------------------------------------

pub const stats = struct {
    pub var block_key_incarnation_refusals: counters.Tripwire = .init("block_key_incarnation_refusals");
    /// Batches dropped unrun because the fence probe read FAILED/fenced:
    /// the fail-stop working, so a counter (0 on a healthy mount), never a
    /// tripwire — the halt has no healthy arm (owner ruling 2026-10-19).
    pub var block_free_reclaim_fence_halts: counters.ShardedCounter = .{};
    pub var block_free_reclaim_dismount_halts: counters.Tripwire = .init("block_free_reclaim_dismount_halts");
    pub var block_free_reclaim_park_tick_wakes: counters.Tripwire = .init("block_free_reclaim_park_tick_wakes");
    pub var terminal_frees: counters.ShardedCounter = .{};
    pub var block_free_reclaim_queued: counters.ShardedCounter = .{};
    pub var block_free_reclaim_elided: counters.ShardedCounter = .{};
    pub var block_free_discards: counters.ShardedCounter = .{};
    pub var block_free_discard_bytes: counters.ShardedCounter = .{};
    pub var block_free_file_punches: counters.ShardedCounter = .{};
    pub var block_free_punch_bytes: counters.ShardedCounter = .{};
    pub var block_free_reclaim_skipped: counters.ShardedCounter = .{};
    pub var block_free_reclaim_batches: counters.ShardedCounter = .{};
    pub var block_free_reclaim_commands: counters.ShardedCounter = .{};
    pub var block_free_reclaim_sync_drains: counters.ShardedCounter = .{};
    pub var block_free_reclaim_cap_parks: counters.ShardedCounter = .{};
    pub var block_free_reclaim_cap_overflow: counters.ShardedCounter = .{};
    pub var block_free_reclaim_supply_drains: counters.ShardedCounter = .{};
    pub var block_free_reclaim_deferred_ticks: counters.ShardedCounter = .{};
    pub var block_free_trim_discards: counters.ShardedCounter = .{};
    pub var block_free_trim_bytes: counters.ShardedCounter = .{};
    pub var block_free_debt_pressure_drains: counters.ShardedCounter = .{};
    pub var block_free_debt_drain_passes: counters.ShardedCounter = .{};
    pub var block_free_debt_fruitless_passes: counters.ShardedCounter = .{};
    pub var block_free_reclaim_queue_bytes: counters.Gauge = .{};
    pub var block_free_elided_debt_bytes: counters.Gauge = .{};
    pub var block_free_reclaim_drain_rate: counters.Gauge = .{};
    pub var block_free_reclaim_arrival_rate: counters.Gauge = .{};
    pub var block_free_reclaim_queue_cap: counters.Gauge = .{};
    pub var block_free_reclaim_park_bound_ms: counters.Gauge = .{};
    /// The debt drainer's measured deallocate completion rate (blocks/s)
    /// and the pace it derives from it under foreground load.
    pub var block_free_debt_drain_rate: counters.Gauge = .{};
    pub var block_free_debt_pace_ms: counters.Gauge = .{};
    /// The durability hold: frees held until their records are durable,
    /// released, forced (a barrier pulled by the ENOSPC valve or the
    /// dismount drain), and the frees held now.
    pub var block_free_durable_holds: counters.ShardedCounter = .{};
    pub var block_free_durable_releases: counters.ShardedCounter = .{};
    pub var block_free_durable_forced: counters.ShardedCounter = .{};
    pub var block_free_durable_held: counters.Gauge = .{};
    /// The ENOSPC valve found frees gathered UPSTREAM of the hold (a
    /// filesystem's corpses awaiting their destroy, `Upstream`) and kicked
    /// their destroy.
    pub var block_free_upstream_kicks: counters.ShardedCounter = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeAll("{");
    inline for (.{ "terminal_frees", "block_free_reclaim_queued", "block_free_reclaim_elided", "block_free_discards", "block_free_discard_bytes", "block_free_file_punches", "block_free_punch_bytes", "block_free_reclaim_skipped", "block_free_reclaim_batches", "block_free_reclaim_commands", "block_free_reclaim_sync_drains", "block_free_reclaim_cap_parks", "block_free_reclaim_cap_overflow", "block_free_reclaim_supply_drains", "block_free_reclaim_deferred_ticks", "block_free_trim_discards", "block_free_trim_bytes", "block_free_debt_pressure_drains", "block_free_debt_drain_passes", "block_free_debt_fruitless_passes", "block_free_reclaim_fence_halts", "block_free_durable_holds", "block_free_durable_releases", "block_free_durable_forced", "block_free_upstream_kicks" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    inline for (.{ "block_free_reclaim_queue_bytes", "block_free_elided_debt_bytes", "block_free_reclaim_drain_rate", "block_free_reclaim_arrival_rate", "block_free_reclaim_queue_cap", "block_free_reclaim_park_bound_ms", "block_free_debt_drain_rate", "block_free_debt_pace_ms", "block_free_durable_held" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    inline for (.{ "block_key_incarnation_refusals", "block_free_reclaim_dismount_halts" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, counters.tripwireCount(name) });
    }
    try w.print("\"block_free_reclaim_park_tick_wakes\":{d}}}", .{counters.tripwireCount("block_free_reclaim_park_tick_wakes")});
}

// ---------------------------------------------------------------------------
// Configuration.
// ---------------------------------------------------------------------------

pub const Config = struct {
    batch_blocks: u32 = DEFAULT_BATCH_BLOCKS,
    batch_ms: u64 = DEFAULT_BATCH_MS,
    /// `SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS`: explicit wins verbatim.
    explicit_cap_blocks: ?u64 = null,
    lanes_per_dev: u32 = DEFAULT_LANES_PER_DEV,
    /// `SQUEEZEFS_RECLAIM_CAP_PARK_MS`: explicit wins verbatim; 0 = never park.
    explicit_park_bound_ms: ?u64 = null,
    /// `SQUEEZEFS_DISCARD_ELISION`.
    elision: bool = true,
    /// `SQUEEZEFS_TEST_RECLAIM_STALL_MS`.
    test_stall_ms: u64 = 0,
    /// `SQUEEZEFS_TEST_SHIP_EVERY_FREE`: a non-holder ships its nonterminal
    /// releases too (the data-loss shape the fleet's merged census reports).
    test_ship_every_free: bool = false,
    /// Test seam: every substrate elides (the file-backed rigs).
    elision_class_all: bool = false,
    /// Test seam: the debt drainer's clock (its quiet window, its trim
    /// meter, its pace deadlines; null = `clock.monotonicNs`). Under it a
    /// deadline park ends only on a wake: the row's time passes when the
    /// row advances it, never on the venue's wall clock.
    test_debt_clock: ?*const fn () u64 = null,
    /// The R5 budget: the cap ceiling's input.
    budget_bytes: u64,
    /// The write pipeline governor's BDP peak ÷ block: the cap's seed
    /// before the first arrival sample (an UPPER bound — fresh writes
    /// displace nothing).
    displacement_seed: u64 = 0,
    read_only: bool = false,
    /// Allocators (data volumes) this reclaimer may be told about.
    vol_capacity: u16 = 64,

    pub fn fromEnv(env: *const std.process.Environ.Map, budget_bytes: u64) Config {
        return .{
            .batch_blocks = knobs.intKnob("SQUEEZEFS_RECLAIM_BATCH_BLOCKS", u32, env),
            .batch_ms = knobs.intKnob("SQUEEZEFS_RECLAIM_BATCH_MS", u64, env),
            .explicit_cap_blocks = knobs.optIntKnob("SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS", u64, env),
            .lanes_per_dev = knobs.intKnob("SQUEEZEFS_RECLAIM_LANES_PER_DEV", u32, env),
            .explicit_park_bound_ms = knobs.optIntKnob("SQUEEZEFS_RECLAIM_CAP_PARK_MS", u64, env),
            .elision = knobs.boolKnob("SQUEEZEFS_DISCARD_ELISION", env),
            .test_stall_ms = knobs.intKnob("SQUEEZEFS_TEST_RECLAIM_STALL_MS", u64, env),
            .test_ship_every_free = knobs.boolKnob("SQUEEZEFS_TEST_SHIP_EVERY_FREE", env),
            .budget_bytes = budget_bytes,
        };
    }
};

// ---------------------------------------------------------------------------
// Entries and the queue.
// ---------------------------------------------------------------------------

/// One queued terminal free. The in-flight registration is fsck visibility
/// for the whole queue residency; `size` is the block's bytes.
pub const Entry = struct {
    guard: InflightAllocGuard,
    size: u32,
    vol: u16,
};

/// A free the durability hold keeps: the ladder's device side, or a
/// non-holder's ship, once its record is durable. The registration is
/// fsck visibility for the hold's residency, as for the queue's.
const Held = struct {
    entry: Entry,
    ship: bool,
};

const VolRow = struct {
    alloc: *BlockAllocator,
    target: Target,
    /// Rung 6: the allocation holder the frees ship to (null = this mount
    /// decides locally — the single-writer class and the holder itself).
    free_target: ?FreeTarget = null,
    queued: Atomic(u64) = .init(0),
    /// This volume ever elided a free: the debt drainer's target set.
    debt_target: Atomic(bool) = .init(false),
    /// The device answered a reclaim command `NotSupported` (a disk without
    /// discard): every later command would too, so the volume stops issuing
    /// them for this mount and says so once. Monotonic: a flag with no paired
    /// data — a lane that misses the store issues one more refused command.
    unsupported: Atomic(bool) = .init(false),
};

pub const FreeOutcome = enum {
    /// Refcount hit 0; the block's discard was elided into debt or queued.
    terminal,
    nonterminal,
    /// A stale incarnation, an untracked free, a SHARED hold: nothing freed.
    refused,
};

pub const Reclaimer = struct {
    gpa: Allocator,
    cfg: Config,
    vols: []?VolRow,
    /// The FIFO of cell indices + the cell pool (one bound) + the overflow
    /// past the cap ceiling (alloc_core's shape, mirrored).
    q: mpmc.BoundedMpmc(u32),
    cells: []Entry,
    free_cells: mpmc.BoundedMpmc(u32),
    overflow_lock: jc.SpinLock = .{},
    overflow: std.ArrayList(Entry) = .empty,
    overflow_front: usize = 0,
    /// Bumped BEFORE the push — an upper bound a pop can never underflow.
    len: Atomic(u64) align(std.atomic.cache_line) = .init(0),
    /// Reserved BEFORE the pop (`population = len + processing`).
    processing: Atomic(u64) = .init(0),
    pressure_latched: Atomic(bool) = .init(false),
    halted: Atomic(bool) = .init(false),
    dismounted: Atomic(bool) = .init(false),
    fence_probe: ?FenceProbe = null,
    tier_purge: ?TierPurge = null,
    fg_signal: ?ForegroundSignal = null,
    fg_last: u64 = 0,
    cap: Atomic(u64) = .init(0),
    park_bound_ms: Atomic(u64) = .init(0),
    /// The worker's arrival wake (ends an idle park only — never a
    /// deferral tick, so two manners decisions are never microseconds
    /// apart under an arrival storm), the cap-wake (a parked producer
    /// pulls a deferred worker out of its tick NOW) and the room edge.
    wake: sync_notify.Notify = .init,
    cap_wake: sync_notify.Notify = .init,
    /// The debt drainer's own wake (the first elided free of a volume).
    debt_wake: sync_notify.Notify = .init,
    room: sync_notify.Notify = .init,
    stop: Atomic(bool) = .init(false),
    worker: ?Io.Future(void) = null,
    drainer: ?Io.Future(void) = null,
    // ---- rates ----
    arrivals: Atomic(u64) = .init(0),
    arr_sample: u64 = 0,
    arr_sample_ms: u64 = 0,
    arrival_peak: u64 = 0,
    /// The queue venue's completion rate (the lanes' aggregate).
    drain_meter: RateMeter = .{},
    /// Entries finished (any venue): the valve's progress predicate.
    finished_total: Atomic(u64) = .init(0),
    // ---- the debt drainer ----
    /// When the foreground signal last stood still (0 = not yet judged).
    debt_quiet_since_ns: u64 = 0,
    debt_fg_last: u64 = 0,
    /// The trim venue's completion rate (one pass's serial commands).
    debt_meter: RateMeter = .{},
    // ---- the durability hold (module doc) ----
    /// Null = every free is durable at its answer (no metadata set behind
    /// this reclaimer: the portable contracts, the offline tools' scratch).
    durable_gate: ?DurableGate = null,
    hold_lock: jc.SpinLock = .{},
    /// Frees pushed since the last seal (no mark covers them yet).
    hold_open: std.ArrayList(Held) = .empty,
    /// Frees the gate's sealed marks cover.
    hold_sealed: std.ArrayList(Held) = .empty,
    /// Both lists' population (the valve's and the parks' supply in sight).
    held: Atomic(u64) = .init(0),
    /// Frees gathered upstream of the hold (`Upstream`); null = none bound.
    upstream: Atomic(?*const Upstream) = .init(null),

    pub fn init(gpa: Allocator, cfg: Config) Allocator.Error!*Reclaimer {
        const self = try gpa.create(Reclaimer);
        errdefer gpa.destroy(self);
        const ceiling: usize = @intCast(capCeiling(cfg.budget_bytes));
        const cells = try gpa.alloc(Entry, ceiling);
        errdefer gpa.free(cells);
        var q = try mpmc.BoundedMpmc(u32).init(gpa, ceiling);
        errdefer q.deinit(gpa);
        var free_cells = try mpmc.BoundedMpmc(u32).init(gpa, ceiling);
        errdefer free_cells.deinit(gpa);
        var i: u32 = 0;
        while (i < ceiling) : (i += 1) assert(free_cells.tryPush(i));
        const vols = try gpa.alloc(?VolRow, cfg.vol_capacity);
        errdefer gpa.free(vols);
        @memset(vols, null);
        self.* = .{ .gpa = gpa, .cfg = cfg, .vols = vols, .q = q, .cells = cells, .free_cells = free_cells };
        const seed = cfg.displacement_seed;
        self.cap.store(resolveQueueCapBlocks(cfg.explicit_cap_blocks, queueCap(seed, roomMs(cfg.batch_blocks, 0), cfg.budget_bytes)), .monotonic);
        self.park_bound_ms.store(resolveParkBoundMs(cfg.explicit_park_bound_ms, parkBoundMs(roomMs(cfg.batch_blocks, 0))), .monotonic);
        self.publishGauges();
        return self;
    }

    /// `stop` first (the tasks must have been joined).
    pub fn deinit(self: *Reclaimer) void {
        assert(self.worker == null and self.drainer == null);
        assert(self.held.load(.acquire) == 0);
        self.hold_open.deinit(self.gpa);
        self.hold_sealed.deinit(self.gpa);
        self.overflow.deinit(self.gpa);
        self.q.deinit(self.gpa);
        self.free_cells.deinit(self.gpa);
        self.gpa.free(self.cells);
        self.gpa.free(self.vols);
        self.gpa.destroy(self);
    }

    /// Register a data volume's allocator and its discard face.
    pub fn registerTarget(self: *Reclaimer, vol: u16, alloc: *BlockAllocator, target: Target) void {
        assert(self.vols[vol] == null);
        self.vols[vol] = .{ .alloc = alloc, .target = target };
    }

    pub fn setFenceProbe(self: *Reclaimer, p: FenceProbe) void {
        self.fence_probe = p;
    }

    /// Rung 6: install (or clear) the free target of a data volume.
    pub fn setFreeTarget(self: *Reclaimer, vol: u16, t: ?FreeTarget) void {
        self.vols[vol].?.free_target = t;
    }

    /// Install the durability hold's gate (before `start`).
    pub fn setDurableGate(self: *Reclaimer, g: DurableGate) void {
        assert(self.worker == null);
        self.durable_gate = g;
    }

    pub fn setTierPurge(self: *Reclaimer, p: TierPurge) void {
        self.tier_purge = p;
    }

    pub fn setForegroundSignal(self: *Reclaimer, s: ForegroundSignal) void {
        self.fg_signal = s;
        self.fg_last = s.read(s.ctx);
        self.debt_fg_last = self.fg_last;
    }

    /// The allocator's "reclaim still owed" conjunct.
    pub fn spacePending(self: *Reclaimer) block_alloc.SpacePending {
        const S = struct {
            fn pending(ctx: ?*anyopaque, for_ino: ?u64) bool {
                const r: *Reclaimer = @ptrCast(@alignCast(ctx.?));
                return r.pendingFor(for_ino);
            }
        };
        return .{ .ctx = self, .pending = &S.pending };
    }

    // ---- the ladder (§3.1) ----

    /// `free_block_verdict`: the terminal-free ladder for one block. `stamp`
    /// is the map entry's lifetime stamp (`STAMP_NONE` = unstamped). The
    /// terminal path's device command, if any, is the QUEUE's — never
    /// issued here (park-don't-spill).
    pub fn freeBlock(self: *Reclaimer, io: Io, vol: u16, idx: u64, stamp: u64) FreeOutcome {
        const row = if (self.vols[vol]) |*r| r else return .refused;
        const alloc = row.alloc;
        // Step 2: a key naming a DEAD lifetime refuses — the §6.3 hazard's
        // destructive face (must-stay-0).
        if (!alloc.liveIncarnation(idx, stamp)) {
            stats.block_key_incarnation_refusals.fire("terminal free of a block under a stale lifetime stamp");
            return .refused;
        }
        // Step 1 (rung 6): a non-holder SHIPS its TERMINAL frees to the
        // holder (charter §5.5) — "the pre-branch shape was a silent local
        // leak". A reference its own RAM count still covers past this one
        // is released HERE: the block's other references live in this
        // writer's slot trees, which the holder cannot read while they are
        // leased, so its untracked arm would seed ONE reference and free a
        // block with live tenants (a joiner's pack seal punched its own
        // tenants at the holder). The last reference, a SHARED block and an
        // untracked one ship. The holder's own executor (authority active)
        // runs the ladder here.
        if (row.free_target) |t| if (!t.authorityActive(t.ctx)) {
            if (!self.cfg.test_ship_every_free and alloc.releaseUnlessLast(idx)) return .nonterminal;
            // The holder reallocates what it is shipped at once: the ship
            // waits here until this mount's freeing record is durable.
            if (self.durable_gate != null) {
                self.hold(io, .{ .entry = .{ .guard = InflightAllocGuard.register(alloc, idx), .size = @intCast(alloc.blockSize()), .vol = vol }, .ship = true });
                return .nonterminal;
            }
            return if (t.ship(t.ctx, alloc.volTag(), idx)) .nonterminal else .refused;
        };
        // Step 5: `begin_free` (the SHARED gate and the untracked gate
        // live inside it).
        const verdict = alloc.beginFree(idx) catch return .refused;
        switch (verdict) {
            .nonterminal => return .nonterminal,
            .refused => return .refused,
            .terminal => {},
        }
        stats.terminal_frees.inc();
        // Step 6: the tier purge — after the retire, before reallocatable.
        if (self.tier_purge) |p| p.purge(p.ctx, vol, idx);
        const e: Entry = .{ .guard = InflightAllocGuard.register(alloc, idx), .size = @intCast(alloc.blockSize()), .vol = vol };
        // Step 6½: the durability hold — reallocatable (or discarded) only
        // once the record that freed it is durable (module doc).
        if (self.durable_gate != null) {
            alloc.holdForDurability();
            self.hold(io, .{ .entry = e, .ship = false });
            return .terminal;
        }
        _ = self.deviceSide(io, e, true);
        return .terminal;
    }

    /// Steps 7–8, the free's device side: ELIDED (debt + `finishFree`, zero
    /// device commands) or QUEUED (the queue owns `finishFree`). The
    /// drainer parks without a deadline once a pass finds no debt, so every
    /// 0 → > 0 transition of a volume's debt wakes it (the one-shot "first
    /// elided free of the volume" wake left it asleep for whole legs
    /// whenever reuse cancelled every debt block between two passes: 0 trim
    /// discards on the aging store's later legs). `notifyOne` keeps one
    /// permit, so a wake landing mid-pass is the next park's immediate
    /// return. `can_park` = an at-cap enqueue parks (the producer's form;
    /// the worker and the synchronous drain would wait on themselves).
    /// True = elided.
    fn deviceSide(self: *Reclaimer, io: Io, entry: Entry, can_park: bool) bool {
        const row = &self.vols[entry.vol].?;
        const alloc = row.alloc;
        if (elideReclaimFor(self.cfg.elision, row.target.substrate, self.cfg.elision_class_all)) {
            var g = entry.guard;
            const was_clear = alloc.elidedDebtBytes() == 0;
            alloc.recordElidedDebt(g.idx);
            alloc.finishFree(g.idx);
            g.deinit();
            alloc.supply_edge.notifyWaiters(io);
            stats.block_free_reclaim_elided.inc();
            stats.block_free_elided_debt_bytes.set(self.totalDebtBytes());
            row.debt_target.store(true, .release);
            if (was_clear) self.debt_wake.notifyOne(io);
            return true;
        }
        self.enqueueParking(io, entry, can_park);
        return false;
    }

    // ---- the durability hold (module doc) ----

    /// Push a free on the open list; release what is durable already. An
    /// allocator too dry to grow the list forces the barrier inline: the
    /// free's record was answered, so a barrier pulled now covers it.
    fn hold(self: *Reclaimer, io: Io, h: Held) void {
        self.hold_lock.lock();
        // Read under the lock `dropHeld` takes after the stop latched: a
        // free arriving behind the teardown is dropped here, never left.
        if (self.stop.load(.acquire)) {
            self.hold_lock.unlock();
            return self.dropOne(h);
        }
        const pushed = if (self.hold_open.append(self.gpa, h)) true else |_| false;
        // Counted under the lock: a release that takes it sees the count.
        if (pushed) _ = self.held.fetchAdd(1, .acq_rel);
        self.hold_lock.unlock();
        stats.block_free_durable_holds.inc();
        if (!pushed) {
            stats.block_free_durable_forced.inc();
            if (self.durable_gate.?.vtable.force(self.durable_gate.?.ctx)) {
                _ = self.held.fetchAdd(1, .acq_rel);
                self.releaseOne(io, h, true);
            } else self.dropOne(h);
            return;
        }
        stats.block_free_durable_held.set(self.held.load(.acquire));
        _ = self.releaseDurable(io, true);
        // The worker ticks while frees are held (its idle park has none).
        self.wake.notifyOne(io);
    }

    /// Hand every free whose record is durable to its device side; seal the
    /// open list when the sealed one is empty. Returns the frees released.
    fn releaseDurable(self: *Reclaimer, io: Io, can_park: bool) u64 {
        const gate = self.durable_gate orelse return 0;
        var released: u64 = 0;
        var chunk: [64]Held = undefined;
        while (self.held.load(.acquire) != 0) {
            var n: usize = 0;
            self.hold_lock.lock();
            if (self.hold_sealed.items.len == 0 and self.hold_open.items.len != 0) {
                // The swap and the snapshot under ONE hold of the lock: every
                // free on the list was pushed (under it) after its commit's
                // answer, so the marks taken now cover each one's record.
                std.mem.swap(std.ArrayList(Held), &self.hold_open, &self.hold_sealed);
                gate.vtable.seal(gate.ctx);
            }
            if (self.hold_sealed.items.len != 0 and gate.vtable.sealed(gate.ctx)) {
                n = @min(chunk.len, self.hold_sealed.items.len);
                const rest = self.hold_sealed.items.len - n;
                @memcpy(chunk[0..n], self.hold_sealed.items[rest..]);
                self.hold_sealed.shrinkRetainingCapacity(rest);
            }
            self.hold_lock.unlock();
            if (n == 0) break;
            for (chunk[0..n]) |h| self.releaseOne(io, h, can_park);
            released += n;
        }
        if (released != 0) stats.block_free_durable_held.set(self.held.load(.acquire));
        return released;
    }

    /// The ENOSPC valve's and the dismount's form: every free held now is
    /// sealed under fresh marks, a barrier is pulled on every member and
    /// waited for, and the sealed frees are released.
    fn forceDurable(self: *Reclaimer, io: Io) u64 {
        const gate = self.durable_gate orelse return 0;
        if (self.held.load(.acquire) == 0) return 0;
        self.hold_lock.lock();
        // Fresh marks cover the sealed frees as well (marks only grow).
        const merged = if (self.hold_sealed.appendSlice(self.gpa, self.hold_open.items)) true else |_| false;
        if (merged) {
            self.hold_open.clearRetainingCapacity();
            gate.vtable.seal(gate.ctx);
        }
        self.hold_lock.unlock();
        stats.block_free_durable_forced.inc();
        if (!gate.vtable.force(gate.ctx)) return 0;
        return self.releaseDurable(io, false);
    }

    fn releaseOne(self: *Reclaimer, io: Io, h: Held, can_park: bool) void {
        _ = self.held.fetchSub(1, .acq_rel);
        stats.block_free_durable_releases.inc();
        const row = &self.vols[h.entry.vol].?;
        if (h.ship) {
            var g = h.entry.guard;
            defer g.deinit();
            // A writer plane torn down meanwhile ships nothing (it forced its
            // held frees first, `forceHeld`): the block is durably
            // unreferenced and the holder's next derivation returns it.
            const t = row.free_target orelse return;
            _ = t.ship(t.ctx, row.alloc.volTag(), g.idx);
            return;
        }
        row.alloc.releaseFromDurability();
        // An elided free returned its block now: the valve's progress.
        if (self.deviceSide(io, h.entry, can_park)) _ = self.finished_total.fetchAdd(1, .acq_rel);
    }

    /// A free whose record never became durable this mount (a FAILED
    /// member, the teardown): its block stays in limbo, unreallocated — the
    /// next mount derives the free space from the durable refs.
    fn dropOne(self: *Reclaimer, h: Held) void {
        var g = h.entry.guard;
        if (!h.ship) self.vols[h.entry.vol].?.alloc.releaseFromDurability();
        g.deinit();
    }

    fn dropHeld(self: *Reclaimer) void {
        self.hold_lock.lock();
        defer self.hold_lock.unlock();
        for ([_]*std.ArrayList(Held){ &self.hold_open, &self.hold_sealed }) |list| {
            for (list.items) |h| self.dropOne(h);
            _ = self.held.fetchSub(list.items.len, .acq_rel);
            list.clearRetainingCapacity();
        }
        stats.block_free_durable_held.set(self.held.load(.acquire));
    }

    /// A writer plane's teardown: every held free made durable and shipped
    /// before its free target goes.
    pub fn forceHeld(self: *Reclaimer, io: Io) u64 {
        return self.forceDurable(io);
    }

    pub fn heldFrees(self: *const Reclaimer) u64 {
        return self.held.load(.acquire);
    }

    fn totalDebtBytes(self: *Reclaimer) u64 {
        var sum: u64 = 0;
        for (self.vols) |row| if (row) |r| {
            sum += r.alloc.elidedDebtBytes();
        };
        return sum;
    }

    // ---- enqueue / park (§3.2) ----

    pub fn population(self: *const Reclaimer) u64 {
        return self.len.load(.acquire) + self.processing.load(.acquire);
    }

    pub fn queueLen(self: *const Reclaimer) u64 {
        return self.len.load(.acquire);
    }

    fn enqueueParking(self: *Reclaimer, io: Io, entry: Entry, can_park: bool) void {
        var e = entry;
        if (self.cfg.read_only) {
            // A reader's discard is the one hazard that destroys data.
            log.err("reclaim: a read-only mount freed block {d} on vol {d}; halting every device reclaim (the entry is dropped)", .{ e.guard.idx, e.vol });
            self.haltDeviceReclaims();
            e.guard.deinit();
            return;
        }
        stats.block_free_reclaim_queued.inc();
        _ = self.arrivals.fetchAdd(1, .monotonic);
        if (can_park and self.population() >= self.cap.load(.monotonic) and !self.fenceHalted()) self.parkAtCap(io);
        stats.block_free_reclaim_queue_bytes.add(e.size);
        if (self.vols[e.vol]) |*row| _ = row.queued.fetchAdd(1, .monotonic);
        _ = self.len.fetchAdd(1, .acq_rel);
        self.push(e);
        self.wake.notifyOne(io);
    }

    fn push(self: *Reclaimer, e: Entry) void {
        if (self.free_cells.tryPop()) |idx| {
            self.cells[idx] = e;
            if (self.q.tryPush(idx)) return;
            // The cell pool's ring holds every cell and every enqueuer takes:
            // a refused push is a taker mid-pop, never a full ring
            // (decision 0003 §AI.4).
            self.free_cells.pushSpin(idx);
        }
        // Past the cell pool (the cap ceiling): the overflow deque.
        self.overflow_lock.lock();
        defer self.overflow_lock.unlock();
        self.overflow.append(self.gpa, e) catch {
            // The process allocator is dry: the entry's free is lost to
            // RAM, never to the device — recovered by `trim --full`.
            log.err("reclaim: out of memory parking a free past the queue ceiling; block {d} vol {d} stays un-discarded until trim --full", .{ e.guard.idx, e.vol });
            var g = e.guard;
            g.deinit();
            _ = self.len.fetchSub(1, .acq_rel);
        };
    }

    fn pop(self: *Reclaimer) ?Entry {
        if (self.q.tryPop()) |idx| {
            const e = self.cells[idx];
            self.free_cells.pushSpin(idx);
            return e;
        }
        self.overflow_lock.lock();
        defer self.overflow_lock.unlock();
        if (self.overflow_front < self.overflow.items.len) {
            const e = self.overflow.items[self.overflow_front];
            self.overflow_front += 1;
            if (self.overflow_front == self.overflow.items.len) {
                self.overflow.clearRetainingCapacity();
                self.overflow_front = 0;
            }
            return e;
        }
        return null;
    }

    /// Park on the room edge until `population < cap` or the fence, bounded
    /// (re-read LIVE each wake: a cold 1 s park collapses to the derived
    /// bound the moment the first window rolls). Registration BEFORE the
    /// re-check (PERF-13); a wake before the deadline that finds no room
    /// is a tick wake (the lost-wake tripwire ≈ 0 while room flows).
    fn parkAtCap(self: *Reclaimer, io: Io) void {
        stats.block_free_reclaim_cap_parks.inc();
        self.wake.notifyOne(io);
        self.cap_wake.notifyOne(io);
        const park_start = clock.monotonicNs();
        while (true) {
            var n = self.room.notified();
            _ = n.enable(io);
            if (self.population() < self.cap.load(.monotonic) or self.fenceHalted()) {
                n.cancel(io);
                return;
            }
            const bound_ms = self.park_bound_ms.load(.monotonic);
            const deadline = park_start + bound_ms * std.time.ns_per_ms;
            const now = clock.monotonicNs();
            if (bound_ms == 0 or now >= deadline) {
                // Soft overflow: the entry goes in past the cap, RAM-bounded;
                // conservation by the valve/unmount/idle drains.
                n.cancel(io);
                stats.block_free_reclaim_cap_overflow.inc();
                return;
            }
            // The tick is the backstop under the deadline, never the transport.
            const tick_deadline = @min(deadline, now + MANNERS_TICK_MS * std.time.ns_per_ms);
            switch (n.waitUntilUncancelable(io, tick_deadline)) {
                .notified => {},
                .deadline => if (tick_deadline < deadline and self.population() >= self.cap.load(.monotonic)) {
                    stats.block_free_reclaim_park_tick_wakes.fire("an at-cap park woke on its tick with no room edge");
                },
            }
        }
    }

    /// `take_batch`: `processing += 1` BEFORE the pop; pop-ownership is
    /// exactly-once.
    fn takeBatch(self: *Reclaimer, out: []Entry) usize {
        var n: usize = 0;
        while (n < out.len) {
            _ = self.processing.fetchAdd(1, .acq_rel);
            const e = self.pop() orelse {
                _ = self.processing.fetchSub(1, .acq_rel);
                break;
            };
            _ = self.len.fetchSub(1, .acq_rel);
            if (self.vols[e.vol]) |*row| _ = row.queued.fetchSub(1, .monotonic);
            out[n] = e;
            n += 1;
        }
        return n;
    }

    // ---- the fence latch (§3.5) ----

    /// Sticky; the first observation of the probe latches, logs and fires
    /// the room edge (parked producers break on the fence).
    pub fn fenceHalted(self: *Reclaimer) bool {
        if (self.halted.load(.monotonic)) return true;
        const p = self.fence_probe orelse return false;
        if (!p.poll(p.ctx)) return false;
        if (!self.halted.swap(true, .acq_rel)) {
            log.err("reclaim: the volume is FAILED or its writer guard FENCED — halting every device reclaim permanently (a fenced zombie never discards; the successor's recovery owns the accounting)", .{});
        }
        return true;
    }

    /// The teardown/crash-analog arm.
    pub fn haltDeviceReclaims(self: *Reclaimer) void {
        self.halted.store(true, .release);
    }

    pub fn closeAtDismount(self: *Reclaimer) void {
        self.dismounted.store(true, .release);
    }

    /// `!fence_halted ∧ (len > 0 ∨ processing > 0 ∨ held > 0 ∨ upstream)`.
    pub fn pending(self: *Reclaimer) bool {
        return self.pendingFor(null);
    }

    /// `pending` as a mint FOR `for_ino` sees it: an upstream that owns
    /// `for_ino` is no supply for it (`Upstream.owns`; decision 0003 §CW).
    pub fn pendingFor(self: *Reclaimer, for_ino: ?u64) bool {
        return !self.fenceHalted() and (self.len.load(.acquire) > 0 or self.processing.load(.acquire) > 0 or self.held.load(.acquire) > 0 or self.upstreamPending(for_ino));
    }

    /// Frees on their way to this reclaimer that no word here counts yet:
    /// the filesystem's corpse gather — unlinked files whose destroy waits
    /// for a full batch or the reclaim tick (`kv_fs.gatherCorpse`, up to
    /// one 5 s tick). They are supply in sight: `pending` sees them (the
    /// latch stays inert, a `.visible_supply` mint parks), and the ENOSPC
    /// valve kicks their destroy before it judges — a full store never
    /// answers ENOSPC while a delete's blocks wait for a tick (the perf
    /// matrix's `rm` + lay-down: three of four reservations refused with
    /// the rm's 64 blocks gathered). The kick never waits: the destroy
    /// runs on the filesystem's own fiber, and its frees enter the hold.
    /// Save to a mint for one of the gather's OWN corpses (`owns`).
    pub const Upstream = struct {
        ctx: *anyopaque,
        pending: *const fn (*anyopaque) bool,
        kick: *const fn (*anyopaque) void,
        /// `ino`'s corpse is this upstream's — gathered, or inside a destroy
        /// pass. The pass settles its corpses' write custody BEFORE it
        /// frees anything (`kv_fs.destroyCorpses` (1)): a mint for one of
        /// them parked on the upstream waited on itself — the 1 s wall per
        /// parked block and candidate volume, "the plane is broken", the
        /// latch inert and every other mint parked behind it meanwhile
        /// (decision 0003 §CW).
        owns: *const fn (*anyopaque, ino: u64) bool,
    };

    /// Bound once the filesystem is up, unbound before it is torn down
    /// (the mount's serve path).
    pub fn bindUpstream(self: *Reclaimer, u: ?*const Upstream) void {
        self.upstream.store(u, .release);
    }

    fn upstreamPending(self: *Reclaimer, for_ino: ?u64) bool {
        const u = self.upstream.load(.acquire) orelse return false;
        if (!u.pending(u.ctx)) return false;
        const ino = for_ino orelse return true;
        return !u.owns(u.ctx, ino);
    }

    /// The valve's first act: a gathered upstream's destroy, kicked.
    pub fn kickUpstream(self: *Reclaimer) void {
        const u = self.upstream.load(.acquire) orelse return;
        if (!u.pending(u.ctx)) return;
        stats.block_free_upstream_kicks.inc();
        u.kick(u.ctx);
    }

    pub fn finishedTotal(self: *const Reclaimer) u64 {
        return self.finished_total.load(.acquire);
    }

    // ---- processing (§3.2 process_entries / issue_range) ----

    /// The lane body: fence check ONCE before any device command; group by
    /// device (the entries are one device's slice already — the dispatcher
    /// grouped), sort, coalesce, per range: issue → `finishFree` each →
    /// `processing −= k` → room edge. Entries left unprocessed on any exit
    /// are reconciled (the `BatchGuard`).
    fn processEntries(self: *Reclaimer, io: Io, entries: []Entry, venue: Venue) void {
        if (entries.len == 0) return;
        if (self.cfg.test_stall_ms != 0) stallMs(io, self.cfg.test_stall_ms);
        if (self.fenceHalted()) {
            stats.block_free_reclaim_fence_halts.inc();
            log.info("reclaim: a batch of {d} entries dropped unrun on a FAILED/fenced volume (the successor's recovery owns them)", .{entries.len});
            self.dropUnrun(io, entries);
            return;
        }
        if (self.dismounted.load(.acquire)) {
            stats.block_free_reclaim_dismount_halts.fire("a reclaim batch ran after the dismount teardown");
            self.dropUnrun(io, entries);
            return;
        }
        std.mem.sort(Entry, entries, {}, entryLess);
        var i: usize = 0;
        while (i < entries.len) {
            const vol = entries[i].vol;
            var j = i;
            while (j < entries.len and entries[j].vol == vol) : (j += 1) {}
            self.processGroup(io, entries[i..j], venue);
            i = j;
        }
    }

    fn entryLess(_: void, a: Entry, b: Entry) bool {
        if (a.vol != b.vol) return a.vol < b.vol;
        return a.guard.idx < b.guard.idx;
    }

    /// One device's offset-sorted slice.
    fn processGroup(self: *Reclaimer, io: Io, group: []Entry, venue: Venue) void {
        const row = if (self.vols[group[0].vol]) |*r| r else {
            self.dropUnrun(io, group);
            return;
        };
        const size = row.alloc.blockSize();
        var offsets_buf: [256]u64 = undefined;
        var ranges_buf: [256]Range = undefined;
        var at: usize = 0;
        while (at < group.len) {
            const n = @min(group.len - at, offsets_buf.len);
            for (group[at .. at + n], 0..) |e, k| offsets_buf[k] = row.alloc.offsetOf(e.guard.idx);
            const ranges = coalesceRanges(offsets_buf[0..n], size, &ranges_buf);
            var consumed: usize = at;
            for (ranges) |r| {
                self.issueRange(row, r, venue);
                // Reallocatable only NOW: a new owner's DMA can never race
                // the discard that just landed (or was refused).
                for (group[consumed .. consumed + r.k]) |*e| {
                    row.alloc.finishFree(e.guard.idx);
                    e.guard.deinit();
                }
                consumed += r.k;
                _ = self.processing.fetchSub(r.k, .acq_rel);
                stats.block_free_reclaim_queue_bytes.sub(r.len);
                _ = self.finished_total.fetchAdd(r.k, .acq_rel);
                self.noteDrained(r.k);
                // Room flows at the drain rate, not in pass-sized bursts; the
                // allocator's supply edge (the ENOSPC park's wake) rides it.
                self.room.notifyWaiters(io);
                row.alloc.supply_edge.notifyWaiters(io);
            }
            at += n;
        }
    }

    const Venue = enum { queue, trim };

    /// ONE device command per range; counters stay PER BLOCK; a refused
    /// command is `reclaim_skipped += k`, NEVER a zeroing-write fallback.
    fn issueRange(self: *Reclaimer, row: *VolRow, r: Range, venue: Venue) void {
        _ = self;
        if (row.target.substrate == .skip or row.unsupported.load(.monotonic)) {
            stats.block_free_reclaim_skipped.add(r.k);
            return;
        }
        stats.block_free_reclaim_commands.inc();
        row.target.issue(row.target.ctx, r.start, r.len) catch |e| {
            if (e == error.NotSupported) {
                if (!row.unsupported.swap(true, .monotonic))
                    log.warn("reclaim: vol {d}'s device does not support {t} (NotSupported) — reclaim stops issuing it on this mount; frees count skipped (space not returned — never a zeroing write)", .{ row.alloc.cfg.vol, row.target.substrate });
            } else {
                log.warn("reclaim: vol {d} refused a {t} of {d} bytes at {d} ({t}); {d} block(s) counted skipped (space not returned — never a zeroing write)", .{ row.alloc.cfg.vol, row.target.substrate, r.len, r.start, e, r.k });
            }
            stats.block_free_reclaim_skipped.add(r.k);
            return;
        };
        switch (row.target.substrate) {
            .file_punch => {
                stats.block_free_file_punches.add(r.k);
                stats.block_free_punch_bytes.add(r.len);
            },
            .bdev_discard => {
                stats.block_free_discards.add(r.k);
                stats.block_free_discard_bytes.add(r.len);
            },
            .skip => unreachable,
        }
        if (venue == .trim) {
            stats.block_free_trim_discards.add(r.k);
            stats.block_free_trim_bytes.add(r.len);
        }
    }

    /// Fenced/dismounted: drop WITHOUT `finishFree` (the successor's
    /// recovery owns the accounting); the guards deregister; the
    /// bookkeeping reconciles.
    fn dropUnrun(self: *Reclaimer, io: Io, entries: []Entry) void {
        var bytes: u64 = 0;
        for (entries) |*e| {
            bytes += e.size;
            e.guard.deinit();
        }
        _ = self.processing.fetchSub(entries.len, .acq_rel);
        stats.block_free_reclaim_queue_bytes.sub(bytes);
        self.room.notifyWaiters(io);
    }

    // ---- rates and the rederivation (§3.3) ----

    /// Per range of the queue venue: the drain meter's window rolls into
    /// the EWMA; the bound and the cap rederive; a SHRINK of the bound and
    /// a RAISE of the cap are room edges.
    fn noteDrained(self: *Reclaimer, k: u32) void {
        // The caller fires the room edge right after this (per range).
        if (self.drain_meter.note(k, clock.monotonicNs() / std.time.ns_per_ms)) _ = self.rederive();
    }

    /// Per range of the TRIM venue: the debt meter learns the device's
    /// deallocate completion rate the drainer paces itself by.
    fn noteTrimmed(self: *Reclaimer, k: u32) void {
        if (self.debt_meter.note(k, self.debtNowNs() / std.time.ns_per_ms)) stats.block_free_debt_drain_rate.set(self.debt_meter.load());
    }

    /// `sample_arrivals`: once per worker pass; the arrival rate is a
    /// peak-hold over `DRAIN_RATE_WINDOW_MS` windows.
    fn sampleArrivals(self: *Reclaimer, io: Io) void {
        const now_ms = clock.monotonicNs() / std.time.ns_per_ms;
        const total = self.arrivals.load(.monotonic);
        if (self.arr_sample_ms == 0) {
            self.arr_sample_ms = now_ms;
            self.arr_sample = total;
            return;
        }
        const dt = now_ms - self.arr_sample_ms;
        if (dt < DRAIN_RATE_WINDOW_MS) return;
        const rate = (total - self.arr_sample) * 1000 / dt;
        self.arrival_peak = peakHold(self.arrival_peak, rate);
        self.arr_sample_ms = now_ms;
        self.arr_sample = total;
        if (self.rederive()) self.room.notifyWaiters(io);
    }

    /// True when the rederivation is itself a room edge: the bound SHRANK
    /// (a cold 1 s park collapses to the derived bound) or the cap ROSE.
    fn rederive(self: *Reclaimer) bool {
        const room = roomMs(self.cfg.batch_blocks, self.drain_meter.load());
        const bound = resolveParkBoundMs(self.cfg.explicit_park_bound_ms, parkBoundMs(room));
        const old_bound = self.park_bound_ms.swap(bound, .monotonic);
        const arrival = if (self.arrival_peak != 0) self.arrival_peak else self.cfg.displacement_seed;
        const cap = resolveQueueCapBlocks(self.cfg.explicit_cap_blocks, queueCap(arrival, room, self.cfg.budget_bytes));
        const old_cap = self.cap.swap(cap, .monotonic);
        self.publishGauges();
        return bound < old_bound or cap > old_cap;
    }

    fn publishGauges(self: *Reclaimer) void {
        stats.block_free_reclaim_drain_rate.set(self.drain_meter.load());
        stats.block_free_reclaim_arrival_rate.set(self.arrival_peak);
        stats.block_free_reclaim_queue_cap.set(self.cap.load(.monotonic));
        stats.block_free_reclaim_park_bound_ms.set(self.park_bound_ms.load(.monotonic));
    }

    pub fn queueCapBlocks(self: *const Reclaimer) u64 {
        return self.cap.load(.monotonic);
    }

    pub fn parkBound(self: *const Reclaimer) u64 {
        return self.park_bound_ms.load(.monotonic);
    }

    pub fn drainRate(self: *const Reclaimer) u64 {
        return self.drain_meter.load();
    }

    pub fn debtDrainRate(self: *const Reclaimer) u64 {
        return self.debt_meter.load();
    }

    // ---- the worker (§3.2 ensure_worker) ----

    pub fn start(self: *Reclaimer, io: Io) Io.ConcurrentError!void {
        assert(self.worker == null);
        self.worker = try io.concurrent(workerMain, .{ self, io });
        self.drainer = try io.concurrent(drainerMain, .{ self, io });
    }

    /// Latch the stop, wake both tasks, join them. The unmount's own drain
    /// (`drainSync`) runs BEFORE this; what is still queued afterwards is
    /// RAM-only work the next mount re-covers.
    pub fn stopAndJoin(self: *Reclaimer, io: Io) void {
        self.stop.store(true, .release);
        self.wake.notifyWaiters(io);
        self.cap_wake.notifyWaiters(io);
        self.debt_wake.notifyWaiters(io);
        self.room.notifyWaiters(io);
        if (self.worker) |*w| {
            w.await(io);
            self.worker = null;
        }
        if (self.drainer) |*d| {
            d.await(io);
            self.drainer = null;
        }
        // What the dismount's forced drain could not make durable (a FAILED
        // member) stays unreallocated, its registrations dropped while the
        // allocators are still open.
        self.dropHeld();
    }

    fn foregroundActive(self: *Reclaimer) bool {
        const s = self.fg_signal orelse return false;
        const now = s.read(s.ctx);
        const moved = now != self.fg_last;
        self.fg_last = now;
        return moved;
    }

    fn anySupplyPressure(self: *Reclaimer) bool {
        for (self.vols) |row| if (row) |r| {
            if (supplyPressure(r.queued.load(.monotonic), r.alloc.freeSupplyBlocks(), true)) return true;
        };
        return false;
    }

    /// Wait on a wake, deadline-bounded; false when the stop latched.
    fn parkWake(self: *Reclaimer, io: Io, which: *sync_notify.Notify, deadline_ns: u64) bool {
        var n = which.notified();
        _ = n.enable(io);
        if (self.stop.load(.acquire)) {
            n.cancel(io);
            return false;
        }
        _ = n.waitUntilUncancelable(io, deadline_ns);
        return !self.stop.load(.acquire);
    }

    fn workerMain(self: *Reclaimer, io: Io) void {
        while (!self.stop.load(.acquire)) {
            _ = self.releaseDurable(io, false);
            // Park until an arrival (or a parked producer's cap-wake); a
            // held free ticks the park until its record is durable.
            if (self.len.load(.acquire) == 0) {
                const deadline = if (self.held.load(.acquire) != 0) clock.monotonicNs() + MANNERS_TICK_MS * std.time.ns_per_ms else std.math.maxInt(u64);
                if (!self.parkWake(io, &self.wake, deadline)) return;
                if (self.len.load(.acquire) == 0) continue;
            }
            // The accumulation window: displaced blocks of one stream arrive
            // back-to-back; a short wait lets them coalesce (only an at-cap
            // producer cuts it short).
            if (self.cfg.batch_ms != 0 and !self.parkWake(io, &self.cap_wake, clock.monotonicNs() + self.cfg.batch_ms * std.time.ns_per_ms)) return;
            while (!self.stop.load(.acquire)) {
                _ = self.releaseDurable(io, false);
                self.sampleArrivals(io);
                const at_cap = self.population() >= self.cap.load(.monotonic);
                const pressure = self.anySupplyPressure();
                const verdict = mannersVerdict(.{
                    .foreground_active = self.foregroundActive(),
                    .at_cap = at_cap,
                    .queue_len = self.len.load(.acquire),
                    .supply_pressure = pressure,
                    .pressure_latched = self.pressure_latched.load(.monotonic),
                });
                switch (verdict) {
                    .park => break,
                    .defer_ => {
                        // One manners tick; only an at-cap producer ends it
                        // early (ordinary arrivals must not, or two decisions
                        // land microseconds apart and read "idle").
                        stats.block_free_reclaim_deferred_ticks.inc();
                        if (!self.parkWake(io, &self.cap_wake, clock.monotonicNs() + MANNERS_TICK_MS * std.time.ns_per_ms)) return;
                        continue;
                    },
                    .drain => {
                        if (pressure and !self.pressure_latched.swap(true, .monotonic)) stats.block_free_reclaim_supply_drains.inc();
                    },
                    .drain_full_width => {},
                }
                const take: usize = @as(usize, self.cfg.batch_blocks) * self.cfg.lanes_per_dev;
                const batch = self.gpa.alloc(Entry, take) catch {
                    // Allocator dry: drain serially in a stack batch.
                    var small: [64]Entry = undefined;
                    const n = self.takeBatch(&small);
                    if (n == 0) break;
                    self.processEntries(io, small[0..n], .queue);
                    continue;
                };
                defer self.gpa.free(batch);
                const n = self.takeBatch(batch);
                if (n == 0) {
                    self.pressure_latched.store(false, .monotonic);
                    break;
                }
                self.dispatchLanes(io, batch[0..n]);
            }
        }
    }

    /// Group by device, sort by offset, `n_lanes` consecutive slices per
    /// device, one `io.concurrent` lane each, await every lane (the worker
    /// never outruns its submissions). A lane is never cancelled mid-batch:
    /// a cancel of the worker is observed after the join.
    fn dispatchLanes(self: *Reclaimer, io: Io, batch: []Entry) void {
        std.mem.sort(Entry, batch, {}, entryLess);
        const prev = io.swapCancelProtection(.blocked);
        defer _ = io.swapCancelProtection(prev);
        var group: Io.Group = .init;
        var spawned_any = false;
        var i: usize = 0;
        while (i < batch.len) {
            var j = i;
            while (j < batch.len and batch[j].vol == batch[i].vol) : (j += 1) {}
            const g = batch[i..j];
            const n_lanes = laneCount(g.len, self.cfg.batch_blocks, self.cfg.lanes_per_dev);
            const chunk: usize = @intCast(chunkSize(g.len, n_lanes));
            var at: usize = 0;
            while (at < g.len) : (at += chunk) {
                const slice = g[at..@min(at + chunk, g.len)];
                stats.block_free_reclaim_batches.inc();
                group.concurrent(io, laneMain, .{ self, io, slice }) catch {
                    // No concurrency available: run it inline on the worker.
                    self.processEntries(io, slice, .queue);
                    continue;
                };
                spawned_any = true;
            }
            i = j;
        }
        if (spawned_any) group.await(io) catch |err| switch (err) {
            error.Canceled => unreachable, // protection is blocked above
        };
    }

    fn laneMain(self: *Reclaimer, io: Io, slice: []Entry) void {
        self.processEntries(io, slice, .queue);
    }

    // ---- the synchronous drains (§3.2 drain_sync) ----

    /// Drain until `len == 0 ∧ processing == 0`: pop-and-process in this
    /// task, and wait on the ROOM edge for in-flight entries other venues
    /// own (the in-flight set's completion IS the edge — Q6: no 1 ms poll,
    /// no 30 s constant). Returns the entries THIS drain processed; counts
    /// `sync_drains` only when ≥ 1 (contract 8).
    pub fn drainSync(self: *Reclaimer, io: Io) u64 {
        // Held frees first, under a pulled barrier: a full store never
        // answers ENOSPC while frees wait for one (module doc).
        var processed: u64 = self.forceDurable(io);
        var batch: [64]Entry = undefined;
        while (true) {
            const n = self.takeBatch(&batch);
            if (n > 0) {
                self.processEntries(io, batch[0..n], .queue);
                processed += n;
                continue;
            }
            if (self.processing.load(.acquire) == 0 and self.len.load(.acquire) == 0) break;
            if (self.fenceHalted()) break;
            var w = self.room.notified();
            _ = w.enable(io);
            if (self.processing.load(.acquire) == 0 and self.len.load(.acquire) == 0) {
                w.cancel(io);
                break;
            }
            // The tick is a lost-wake backstop only (the edge fires per range).
            _ = w.waitUntilUncancelable(io, clock.monotonicNs() + MANNERS_TICK_MS * std.time.ns_per_ms);
        }
        if (processed > 0) stats.block_free_reclaim_sync_drains.inc();
        return processed;
    }

    // ---- the debt drainer (§3.4) ----

    /// Foreground moving ⇒ defer; else quiet for `IDLE_CONFIRM_TICKS` ticks
    /// of WALL time (a paced pass is not a tick: twenty paced passes could
    /// span twenty seconds).
    fn drainerMustDefer(self: *Reclaimer) bool {
        const now = self.debtNowNs();
        if (self.fg_signal) |s| {
            const cur = s.read(s.ctx);
            if (cur != self.debt_fg_last) {
                self.debt_fg_last = cur;
                self.debt_quiet_since_ns = now;
                return true;
            }
        }
        if (self.debt_quiet_since_ns == 0) self.debt_quiet_since_ns = now;
        return now - self.debt_quiet_since_ns < IDLE_CONFIRM_TICKS * MANNERS_TICK_MS * std.time.ns_per_ms;
    }

    const DebtPass = struct {
        verdict: enum { idle, progressed, deferred, fruitless },
        /// The foreground was moving (the pass was a PRESSURE drain).
        foreground: bool,
        /// Blocks this pass issued (the pace's batch term).
        blocks: u64,
    };

    fn debtNowNs(self: *const Reclaimer) u64 {
        return if (self.cfg.test_debt_clock) |now| now() else clock.monotonicNs();
    }

    /// One pass over every debt target: `debt == 0` ⇒ skip; foreground ∧
    /// within the watermark ⇒ defer (the row pays zero discards); foreground
    /// ∧ past it ⇒ a counted pressure drain of the device-paced batch
    /// (`debtBatchBlocks`); idle ⇒ ONE configured batch. The verdict keys
    /// on what was RECLAIMED (finding 12: a fully grace-held backlog looped
    /// hot at ≈ 5,000 jobs/s on an idle daemon at 88 °C).
    fn debtPass(self: *Reclaimer, io: Io) DebtPass {
        if (self.fenceHalted()) return .{ .verdict = .idle, .foreground = false, .blocks = 0 };
        stats.block_free_debt_drain_passes.inc();
        const must_defer = self.drainerMustDefer();
        var outstanding: u64 = 0;
        var blocks: u64 = 0;
        var deferred = false;
        for (self.vols, 0..) |*slot, vol| {
            const row = if (slot.*) |*r| r else continue;
            if (!row.debt_target.load(.acquire)) continue;
            const debt = row.alloc.elidedDebtBytes();
            if (debt == 0) continue;
            outstanding += debt;
            const within = debtWithinWatermark(debt, row.alloc.virginBytes());
            if (must_defer and within) {
                deferred = true;
                continue;
            }
            var batch = self.cfg.batch_blocks;
            if (must_defer) {
                stats.block_free_debt_pressure_drains.inc();
                batch = debtBatchBlocks(self.debt_meter.load(), self.cfg.batch_blocks);
            }
            blocks += self.drainDebtSync(io, @intCast(vol), batch, false).blocks;
        }
        stats.block_free_elided_debt_bytes.set(self.totalDebtBytes());
        const verdict: @FieldType(DebtPass, "verdict") = if (outstanding == 0) .idle else if (blocks > 0) .progressed else if (deferred) .deferred else .fruitless;
        return .{ .verdict = verdict, .foreground = must_defer, .blocks = blocks };
    }

    /// The drainer's cadence: idle ⇒ park on the first elided free; a
    /// quiet progressed pass ⇒ the next at once (full width to empty); a
    /// PRESSURE pass that progressed ⇒ one batch-time at the device's
    /// measured completion rate (`debtPaceMs` — never a hot loop under a
    /// foreground stream); deferred ⇒ re-judge next tick; fruitless (every
    /// claimable block raced away) ⇒ one batch-time, then look again.
    fn drainerMain(self: *Reclaimer, io: Io) void {
        while (!self.stop.load(.acquire)) {
            const pass = self.debtPass(io);
            const now = self.debtNowNs();
            const deadline: u64 = switch (pass.verdict) {
                .idle => std.math.maxInt(u64),
                .progressed => blk: {
                    if (!pass.foreground) break :blk now;
                    const pace = debtPaceMs(pass.blocks, self.debt_meter.load());
                    stats.block_free_debt_pace_ms.set(pace);
                    break :blk now + pace * std.time.ns_per_ms;
                },
                .deferred => now + MANNERS_TICK_MS * std.time.ns_per_ms,
                .fruitless => blk: {
                    stats.block_free_debt_fruitless_passes.inc();
                    const pace = debtPaceMs(self.cfg.batch_blocks, self.debt_meter.load());
                    stats.block_free_debt_pace_ms.set(pace);
                    break :blk now + pace * std.time.ns_per_ms;
                },
            };
            if (deadline == now) continue;
            const wall_deadline = if (self.cfg.test_debt_clock == null) deadline else std.math.maxInt(u64);
            if (!self.parkWake(io, &self.debt_wake, wall_deadline)) return;
        }
    }

    pub const Drained = struct { blocks: u64, bytes: u64 };

    /// `drain_debt_sync`: candidates = the debt batch (or, `full`, the whole
    /// free list); per window: claim each for trim (losers owe nothing —
    /// reused or a racing trim) → sort → coalesce → issue (Trim venue) →
    /// return each from the window (the registrations drop after). A
    /// window is pending supply, never a deficit (the claim window law).
    pub fn drainDebtSync(self: *Reclaimer, io: Io, vol: u16, max: u32, full: bool) Drained {
        const row = if (self.vols[vol]) |*r| r else return .{ .blocks = 0, .bytes = 0 };
        const alloc = row.alloc;
        var out: Drained = .{ .blocks = 0, .bytes = 0 };
        if (self.fenceHalted()) return out;
        var idxs_buf: [256]u64 = undefined;
        const window: usize = @min(max, idxs_buf.len);
        var from: u64 = 0;
        while (true) {
            var n: usize = 0;
            if (full) {
                const free = alloc.freeBlockIndices(self.gpa) catch return out;
                defer self.gpa.free(free);
                for (free) |idx| {
                    if (idx < from) continue;
                    if (n == window) break;
                    idxs_buf[n] = idx;
                    n += 1;
                }
                if (n > 0) from = idxs_buf[n - 1] + 1;
            } else {
                n = alloc.takeDebtBatch(idxs_buf[0..window]);
            }
            if (n == 0) break;
            var guards: [256]?InflightAllocGuard = undefined;
            var claimed: [256]u64 = undefined;
            var c: usize = 0;
            for (idxs_buf[0..n]) |idx| {
                if (full) alloc.cancelElidedDebt(idx);
                if (alloc.claimFreeForTrim(idx)) |g| {
                    guards[c] = g;
                    claimed[c] = idx;
                    c += 1;
                } else if (!full) {
                    // A loser owes nothing; the debt was already cancelled by
                    // the claim that beat us, or belongs to a racing trim.
                    alloc.cancelElidedDebt(idx);
                }
            }
            if (c > 0) {
                for (claimed[0..c]) |idx| alloc.cancelElidedDebt(idx);
                var offsets: [256]u64 = undefined;
                for (claimed[0..c], 0..) |idx, k| offsets[k] = alloc.offsetOf(idx);
                var ranges_buf: [256]Range = undefined;
                const ranges = coalesceRanges(offsets[0..c], alloc.blockSize(), &ranges_buf);
                for (ranges) |r| {
                    self.issueRange(row, r, .trim);
                    self.noteTrimmed(r.k);
                    out.blocks += r.k;
                    out.bytes += r.len;
                }
                for (claimed[0..c], 0..) |idx, k| alloc.returnFromTrim(idx, &guards[k].?);
                // The window closed: the trim-return edge a parked allocation waits on.
                alloc.supply_edge.notifyWaiters(io);
            }
            if (!full) break; // ONE paced batch per pass
            if (n < window) break;
        }
        return out;
    }

    /// `trim --full`: refuse if fenced (a fenced holder issues NO
    /// destructive command); every registered target: the whole free list
    /// once (`full`) or the debt until outstanding is 0. The free list is
    /// the durable truth and the debt tracker only its incremental record,
    /// so a full trim restores substrate hygiene after debt was lost to a
    /// crash/unmount (KD-4.9).
    pub fn trim(self: *Reclaimer, io: Io, full: bool) error{Fenced}!Drained {
        if (self.fenceHalted()) return error.Fenced;
        var total: Drained = .{ .blocks = 0, .bytes = 0 };
        for (self.vols, 0..) |*slot, vol| {
            const row = if (slot.*) |*r| r else continue;
            if (!full and !row.debt_target.load(.acquire)) continue;
            if (full) {
                const d = self.drainDebtSync(io, @intCast(vol), self.cfg.batch_blocks, true);
                total.blocks += d.blocks;
                total.bytes += d.bytes;
            } else {
                while (row.alloc.elidedDebtBytes() > 0) {
                    const d = self.drainDebtSync(io, @intCast(vol), self.cfg.batch_blocks, false);
                    if (d.blocks == 0 and row.alloc.elidedDebtBytes() > 0) break;
                    total.blocks += d.blocks;
                    total.bytes += d.bytes;
                }
            }
        }
        stats.block_free_elided_debt_bytes.set(self.totalDebtBytes());
        return total;
    }
};

/// The test stall seam, as a deadline wait on a private notify (never a
/// bare sleep as synchronization).
fn stallMs(io: Io, ms: u64) void {
    var n: sync_notify.Notify = .init;
    var w = n.notified();
    _ = w.enable(io);
    _ = w.waitUntilUncancelable(io, clock.monotonicNs() + ms * std.time.ns_per_ms);
}

// ---------------------------------------------------------------------------
// The Linux binding: a `Target` over `nvme_dev.Device`.
// ---------------------------------------------------------------------------

pub const device_binding = if (builtin.os.tag == .linux) struct {
    fn issue(ctx: ?*anyopaque, start: u64, len: u64) errors.Error!void {
        const dev: *nvme_dev.Device = @ptrCast(@alignCast(ctx.?));
        return dev.discardRange(start, len);
    }

    /// The device classified its substrate ONCE at open (`Device.reclaimOp`,
    /// Q5): no `stat` per batch, no re-open per group.
    pub fn target(dev: *nvme_dev.Device) Target {
        return .{ .ctx = dev, .substrate = dev.reclaimOp(), .issue = &issue };
    }
} else struct {};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const enospc_mod = @import("enospc.zig");

test "classify_block_device_discards_never_zero_writes / regular_file_punches / anything_else_skips" {
    try testing.expectEqual(Substrate.bdev_discard, classifySubstrate(linux.S.IFBLK | 0o600));
    try testing.expectEqual(Substrate.file_punch, classifySubstrate(linux.S.IFREG | 0o644));
    try testing.expectEqual(Substrate.skip, classifySubstrate(linux.S.IFCHR | 0o600));
    try testing.expectEqual(Substrate.skip, classifySubstrate(linux.S.IFDIR | 0o755));
}

test "reclaim_room_latency_is_batch_over_measured_drain_rate; cold reads the ceiling" {
    try testing.expectEqual(@as(u64, 35), roomMs(64, 1800)); // the fleet: 64 × 1000 ÷ 1800
    try testing.expectEqual(@as(u64, 10), roomMs(64, 5900)); // idle catch-up
    try testing.expectEqual(PARK_BOUND_CEILING_MS, roomMs(64, 0));
    try testing.expectEqual(@as(u64, 1), roomMs(1, 100_000));
}

test "the debt drainer's pace is the device's: batch = clamp(rate × tick, 1, batch) (cold ⇒ the configured batch); pace = clamp(batch ÷ rate, tick, ceiling) (cold ⇒ the ceiling); the meter rolls one tick first, then the window, into the ¼ EWMA" {
    // 1,280 blocks/s × 50 ms = 64; 100 × 50 ms = 5; 10 × 50 ms = 0 ⇒ 1; 100,000 × 50 ms ⇒ 64 (the cap).
    try testing.expectEqual(@as(u32, 64), debtBatchBlocks(1280, 64));
    try testing.expectEqual(@as(u32, 5), debtBatchBlocks(100, 64));
    try testing.expectEqual(@as(u32, 1), debtBatchBlocks(10, 64));
    try testing.expectEqual(@as(u32, 64), debtBatchBlocks(100_000, 64));
    try testing.expectEqual(@as(u32, 64), debtBatchBlocks(0, 64));
    // 64 blocks at 1,280/s = 50 ms (the tick floor); 64 at 100/s = 640 ms; 64 at 10/s ⇒ 6,400 ⇒ the 1 s ceiling; 5 at 100/s = 50 ms; cold ⇒ ceiling.
    try testing.expectEqual(MANNERS_TICK_MS, debtPaceMs(64, 1280));
    try testing.expectEqual(@as(u64, 640), debtPaceMs(64, 100));
    try testing.expectEqual(PARK_BOUND_CEILING_MS, debtPaceMs(64, 10));
    try testing.expectEqual(MANNERS_TICK_MS, debtPaceMs(5, 100));
    try testing.expectEqual(PARK_BOUND_CEILING_MS, debtPaceMs(64, 0));
    // The meter: the first window is one tick; 64 blocks over 50 ms = 1,280/s; the next window is 250 ms; 64 over 250 ms = 256/s ⇒ EWMA 1280 − 320 + 64 = 1024.
    var m: RateMeter = .{};
    try testing.expect(!m.note(64, 1000)); // opens the window
    try testing.expect(!m.note(0, 1049)); // 49 ms: not yet
    try testing.expect(m.note(0, 1050));
    try testing.expectEqual(@as(u64, 1280), m.load());
    try testing.expect(!m.note(64, 1299)); // 249 ms into the 250 ms window
    try testing.expect(m.note(0, 1300));
    try testing.expectEqual(@as(u64, 1024), m.load());
}

test "reclaim_park_bound_derives_from_the_drains_room_latency: clamp(4 × room, 50, 1000)" {
    try testing.expectEqual(@as(u64, 140), parkBoundMs(roomMs(64, 1800))); // the fleet's 140 ms vs the pinned 1,000
    try testing.expectEqual(@as(u64, 80), parkBoundMs(roomMs(8, 400))); // the A-B-B-A rig: 4 × 8 ÷ 400 s = 80 ms
    try testing.expectEqual(PARK_BOUND_FLOOR_MS, parkBoundMs(1));
    try testing.expectEqual(PARK_BOUND_CEILING_MS, parkBoundMs(roomMs(64, 0)));
    try testing.expectEqual(PARK_BOUND_FLOOR_MS, MANNERS_TICK_MS);
}

test "reclaim_queue_cap_derives_from_displacement_rate_and_room_latency; the shipped floor; the budget ceiling" {
    const budget: u64 = 32 * derive.GiB;
    // At 4 MiB blocks on the fleet: 4,750 × 35 ms ≈ 166 ≪ 4096 — the cap was never the lever.
    try testing.expectEqual(QUEUE_CAP_FLOOR_BLOCKS, queueCap(4750, 35, budget));
    try testing.expectEqual(@as(u64, 100_000), queueCap(100_000, 1000, budget));
    try testing.expectEqual(capCeiling(budget), queueCap(std.math.maxInt(u64), 1000, budget));
    try testing.expectEqual(budget / QUEUE_RAM_SHARE_DIVISOR / entryRamBytes(), capCeiling(budget));
    try testing.expectEqual(QUEUE_CAP_FLOOR_BLOCKS, capCeiling(0)); // never regress below shipped
    try testing.expectEqual(QUEUE_CAP_REGISTRY_MAX_BLOCKS, capCeiling(std.math.maxInt(u64) / 2));
    try testing.expectEqual(@as(u64, @sizeOf(Entry) + 4), entryRamBytes());
}

test "reclaim_knobs_explicit_wins_verbatim_over_derived" {
    try testing.expectEqual(@as(u64, 4096), resolveQueueCapBlocks(4096, 200_000));
    try testing.expectEqual(@as(u64, 200_000), resolveQueueCapBlocks(null, 200_000));
    try testing.expectEqual(@as(u64, 1000), resolveParkBoundMs(1000, 140));
    try testing.expectEqual(@as(u64, 0), resolveParkBoundMs(0, 140));
    try testing.expectEqual(@as(u64, 140), resolveParkBoundMs(null, 140));
    try testing.expectEqual(@as(u32, 64), DEFAULT_BATCH_BLOCKS);
    try testing.expectEqual(@as(u64, 2), DEFAULT_BATCH_MS);
    try testing.expectEqual(@as(u32, 32), DEFAULT_LANES_PER_DEV);
}

test "rates: the EWMA α = ¼ and the arrival peak-hold decay by ⅛" {
    try testing.expectEqual(@as(u64, 400), ewmaQuarter(400, 400));
    try testing.expectEqual(@as(u64, 500), ewmaQuarter(400, 800));
    try testing.expectEqual(@as(u64, 800), peakHold(0, 800)); // a storm registers at once
    try testing.expectEqual(@as(u64, 700), peakHold(800, 0)); // a lull decays by ⅛
    try testing.expectEqual(@as(u64, 900), peakHold(800, 900));
}

test "lanes: clamp(ceil(demand ÷ batch), 1, lanes_per_dev); adjacency-preserving chunking; coalescing is per adjacent run" {
    try testing.expectEqual(@as(u64, 1), laneCount(10, 64, 32));
    try testing.expectEqual(@as(u64, 2), laneCount(65, 64, 32));
    try testing.expectEqual(@as(u64, 32), laneCount(1_000_000, 64, 32));
    try testing.expectEqual(@as(u64, 33), chunkSize(65, 2));
    var buf: [8]Range = undefined;
    const r = coalesceRanges(&.{ 0, 4096, 8192, 20480, 24576, 40960 }, 4096, &buf);
    try testing.expectEqual(@as(usize, 3), r.len);
    try testing.expectEqual(Range{ .start = 0, .len = 12288, .k = 3 }, r[0]);
    try testing.expectEqual(Range{ .start = 20480, .len = 8192, .k = 2 }, r[1]);
    try testing.expectEqual(Range{ .start = 40960, .len = 4096, .k = 1 }, r[2]);
}

test "the manners law verdict table" {
    try testing.expectEqual(Verdict.defer_, mannersVerdict(.{ .foreground_active = true, .at_cap = false, .queue_len = 5, .supply_pressure = false, .pressure_latched = false }));
    try testing.expectEqual(Verdict.drain, mannersVerdict(.{ .foreground_active = true, .at_cap = true, .queue_len = 5, .supply_pressure = false, .pressure_latched = false }));
    try testing.expectEqual(Verdict.drain_full_width, mannersVerdict(.{ .foreground_active = false, .at_cap = false, .queue_len = 5, .supply_pressure = false, .pressure_latched = false }));
    try testing.expectEqual(Verdict.park, mannersVerdict(.{ .foreground_active = false, .at_cap = false, .queue_len = 0, .supply_pressure = false, .pressure_latched = false }));
    try testing.expectEqual(Verdict.park, mannersVerdict(.{ .foreground_active = true, .at_cap = false, .queue_len = 0, .supply_pressure = false, .pressure_latched = false }));
    // Thin supply outranks manners; the latch keeps draining to empty.
    try testing.expectEqual(Verdict.drain, mannersVerdict(.{ .foreground_active = true, .at_cap = false, .queue_len = 5, .supply_pressure = true, .pressure_latched = false }));
    try testing.expectEqual(Verdict.drain, mannersVerdict(.{ .foreground_active = true, .at_cap = false, .queue_len = 5, .supply_pressure = false, .pressure_latched = true }));
    try testing.expect(supplyPressure(10, 20, true));
    try testing.expect(!supplyPressure(9, 20, true));
    try testing.expect(!supplyPressure(10, 20, false));
}

test "watermark_is_debt_versus_virgin_tail; elision is bdev-only unless the class-all seam" {
    try testing.expect(debtWithinWatermark(10, 10));
    try testing.expect(!debtWithinWatermark(11, 10));
    try testing.expect(elideReclaimFor(true, .bdev_discard, false));
    try testing.expect(!elideReclaimFor(true, .file_punch, false));
    try testing.expect(elideReclaimFor(true, .file_punch, true));
    try testing.expect(!elideReclaimFor(false, .bdev_discard, true));
    try testing.expect(!elideReclaimFor(true, .skip, false));
}

// ---- the fixture: an allocator, a recording target, a reclaimer ----

const Recorder = struct {
    lock: jc.SpinLock = .{},
    commands: std.ArrayList(Range) = .empty,
    gpa: Allocator,
    refuse: bool = false,
    issued: Atomic(u64) = .init(0),
    refused: Atomic(u64) = .init(0),

    fn issue(ctx: ?*anyopaque, start: u64, len: u64) errors.Error!void {
        const self: *Recorder = @ptrCast(@alignCast(ctx.?));
        if (self.refuse) {
            _ = self.refused.fetchAdd(1, .monotonic);
            return error.NotSupported;
        }
        self.lock.lock();
        defer self.lock.unlock();
        self.commands.append(self.gpa, .{ .start = start, .len = len, .k = 0 }) catch return error.OutOfMemory;
        _ = self.issued.fetchAdd(1, .monotonic);
    }

    fn target(self: *Recorder, substrate: Substrate) Target {
        return .{ .ctx = self, .substrate = substrate, .issue = &issue };
    }

    fn deinit(self: *Recorder) void {
        self.commands.deinit(self.gpa);
    }
};

const Fixture = struct {
    alloc: *BlockAllocator,
    rec: Recorder,
    r: *Reclaimer,

    fn init(cfg: Config, substrate: Substrate, blocks: u64) !*Fixture {
        const f = try testing.allocator.create(Fixture);
        errdefer testing.allocator.destroy(f);
        f.alloc = try BlockAllocator.init(testing.allocator, .{ .vol_tag = 1, .vol = 0, .block_size = 4096, .capacity_bytes = blocks * 4096 });
        errdefer f.alloc.deinit();
        f.rec = .{ .gpa = testing.allocator };
        f.r = try Reclaimer.init(testing.allocator, cfg);
        f.r.registerTarget(0, f.alloc, f.rec.target(substrate));
        f.alloc.space_pending = f.r.spacePending();
        return f;
    }

    fn deinit(self: *Fixture) void {
        self.r.deinit();
        self.alloc.deinit();
        self.rec.deinit();
        testing.allocator.destroy(self);
    }

    fn mintPublished(self: *Fixture, n: usize) !void {
        var i: usize = 0;
        while (i < n) : (i += 1) {
            const m = try self.alloc.allocateBlock();
            self.alloc.publishBlock(m.idx);
        }
    }
};

const test_budget: u64 = 4 * derive.GiB;

test "terminal_free_on_file_backing_is_one_counted_punch; nonterminal_free_reclaims_and_counts_nothing; queued + elided ≡ terminal frees" {
    const io = testing.io;
    var f = try Fixture.init(.{ .budget_bytes = test_budget, .batch_ms = 0 }, .file_punch, 16);
    defer f.deinit();
    try f.mintPublished(4);
    const queued0 = stats.block_free_reclaim_queued.load();
    const elided0 = stats.block_free_reclaim_elided.load();
    const terminal0 = stats.terminal_frees.load();
    const punches0 = stats.block_free_file_punches.load();
    const commands0 = stats.block_free_reclaim_commands.load();
    // A pinned block's free is nonterminal: nothing queued, nothing issued.
    try testing.expectEqual(block_alloc.PinVerdict.pinned, f.alloc.pinBlockValidated(1));
    try testing.expectEqual(FreeOutcome.nonterminal, f.r.freeBlock(io, 0, 1, f.alloc.stampOf(1)));
    try testing.expectEqual(@as(u64, 0), f.r.population());
    // A terminal free queues (file backing: never elided) and the drain is ONE punch.
    try testing.expectEqual(FreeOutcome.terminal, f.r.freeBlock(io, 0, 2, f.alloc.stampOf(2)));
    try testing.expectEqual(@as(u64, 1), f.r.population());
    try testing.expectEqual(block_alloc.State.limbo, f.alloc.stateOf(2)); // not reallocatable until the punch lands
    try testing.expect(f.alloc.inflightContains(2));
    try testing.expectEqual(@as(u64, 4096), stats.block_free_reclaim_queue_bytes.load());
    try testing.expectEqual(@as(u64, 1), f.r.drainSync(io));
    try testing.expectEqual(block_alloc.State.free, f.alloc.stateOf(2));
    try testing.expect(!f.alloc.inflightContains(2));
    try testing.expectEqual(punches0 + 1, stats.block_free_file_punches.load());
    try testing.expectEqual(commands0 + 1, stats.block_free_reclaim_commands.load());
    try testing.expectEqual(@as(usize, 1), f.rec.commands.items.len);
    try testing.expectEqual(Range{ .start = 2 * 4096, .len = 4096, .k = 0 }, f.rec.commands.items[0]);
    try testing.expectEqual(@as(u64, 0), stats.block_free_reclaim_queue_bytes.load());
    try testing.expect(!f.r.pending());
    // The ledger identity.
    try testing.expectEqual(stats.terminal_frees.load() - terminal0, (stats.block_free_reclaim_queued.load() - queued0) + (stats.block_free_reclaim_elided.load() - elided0));
    // A stale stamp refuses, loud.
    const refusals = counters.tripwireCount("block_key_incarnation_refusals");
    stats.block_key_incarnation_refusals.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(FreeOutcome.refused, f.r.freeBlock(io, 0, 3, f.alloc.stampOf(3) + 1));
    try log.testing.expectAllFired();
    try testing.expectEqual(refusals + 1, counters.tripwireCount("block_key_incarnation_refusals"));
    try testing.expectEqual(block_alloc.State.allocated, f.alloc.stateOf(3));
}

test "a NON-holder ships only its TERMINAL frees (charter §5.5): a release its RAM count still covers past this reference stays local (nonterminal, nothing shipped — the holder cannot read the references a live lessee's slot trees carry); the last reference ships and the local word stays for the shipper's verdict" {
    const io = testing.io;
    var f = try Fixture.init(.{ .budget_bytes = test_budget, .batch_ms = 0 }, .file_punch, 16);
    defer f.deinit();
    try f.mintPublished(4);
    const Holder = struct {
        shipped: [8]u64 = undefined,
        n: usize = 0,
        fn ship(ctx: *anyopaque, _: u64, idx: u64) bool {
            const h: *@This() = @ptrCast(@alignCast(ctx));
            h.shipped[h.n] = idx;
            h.n += 1;
            return true;
        }
        fn active(_: *anyopaque) bool {
            return false;
        }
    };
    var holder: Holder = .{};
    f.r.setFreeTarget(0, .{ .ctx = &holder, .ship = &Holder.ship, .authorityActive = &Holder.active });
    // Block 1 carries three references (a pack's pin + two tenants).
    try testing.expect(f.alloc.incrementRefcount(1));
    try testing.expect(f.alloc.incrementRefcount(1));
    try testing.expectEqual(FreeOutcome.nonterminal, f.r.freeBlock(io, 0, 1, f.alloc.stampOf(1)));
    try testing.expectEqual(FreeOutcome.nonterminal, f.r.freeBlock(io, 0, 1, f.alloc.stampOf(1)));
    try testing.expectEqual(@as(usize, 0), holder.n);
    try testing.expectEqual(@as(?u32, 1), f.alloc.refcount(1));
    try testing.expectEqual(block_alloc.State.allocated, f.alloc.stateOf(1));
    // The last reference ships; the local word is the shipper's to retire
    // on the holder's verdict, never this reclaimer's.
    try testing.expectEqual(FreeOutcome.nonterminal, f.r.freeBlock(io, 0, 1, f.alloc.stampOf(1)));
    try testing.expectEqualSlices(u64, &.{1}, holder.shipped[0..holder.n]);
    try testing.expectEqual(@as(?u32, 1), f.alloc.refcount(1));
    try testing.expectEqual(@as(u64, 0), f.r.population());
}

test "elided_terminal_free_is_reallocatable_with_zero_device_commands; the lever off restores the queued reclaim verbatim" {
    const io = testing.io;
    var f = try Fixture.init(.{ .budget_bytes = test_budget, .batch_ms = 0 }, .bdev_discard, 16);
    defer f.deinit();
    try f.mintPublished(4);
    const elided0 = stats.block_free_reclaim_elided.load();
    try testing.expectEqual(FreeOutcome.terminal, f.r.freeBlock(io, 0, 1, f.alloc.stampOf(1)));
    try testing.expectEqual(elided0 + 1, stats.block_free_reclaim_elided.load());
    try testing.expectEqual(@as(u64, 0), f.r.population());
    try testing.expectEqual(@as(usize, 0), f.rec.commands.items.len);
    try testing.expectEqual(block_alloc.State.free, f.alloc.stateOf(1));
    try testing.expectEqual(@as(u64, 4096), f.alloc.elidedDebtBytes());
    try testing.expectEqual(@as(u64, 1), (try f.alloc.allocateBlock()).idx); // reallocatable at once
    try testing.expectEqual(@as(u64, 0), f.alloc.elidedDebtBytes()); // claim cancels the debt
    // Lever off: the same substrate queues.
    f.r.cfg.elision = false;
    try testing.expectEqual(FreeOutcome.terminal, f.r.freeBlock(io, 0, 2, f.alloc.stampOf(2)));
    try testing.expectEqual(@as(u64, 1), f.r.population());
    const discards0 = stats.block_free_discards.load();
    try testing.expectEqual(@as(u64, 1), f.r.drainSync(io));
    try testing.expectEqual(@as(usize, 1), f.rec.commands.items.len);
    try testing.expectEqual(discards0 + 1, stats.block_free_discards.load());
}

/// The metadata set as the durability hold sees it: `hwm` is the mark
/// every commit answered so far lies below, `durable` the floor completed
/// barriers cover; `force` is a barrier pulled now.
const FakeGate = struct {
    hwm: u64 = 0,
    durable: u64 = 0,
    sealed_mark: u64 = 0,
    seals: u64 = 0,
    forces: u64 = 0,

    fn gate(self: *FakeGate) DurableGate {
        return .{ .ctx = self, .vtable = &vtable };
    }

    const vtable: DurableGate.VTable = .{ .seal = &seal, .sealed = &sealed, .force = &force };

    fn seal(ctx: *anyopaque) void {
        const g: *FakeGate = @ptrCast(@alignCast(ctx));
        g.sealed_mark = g.hwm;
        g.seals += 1;
    }

    fn sealed(ctx: *anyopaque) bool {
        const g: *FakeGate = @ptrCast(@alignCast(ctx));
        return g.durable >= g.sealed_mark;
    }

    fn force(ctx: *anyopaque) bool {
        const g: *FakeGate = @ptrCast(@alignCast(ctx));
        g.forces += 1;
        g.durable = g.hwm;
        return true;
    }
};

test "the durability hold (charter §6.2, decision 0003 §BT): a terminal free stays in limbo — supply in sight, never reallocated — until the barrier covering its record completes; a later free rides the next seal; the release is the ladder's device side; the dismount/valve drain pulls the barrier; a non-holder's ship waits the same way" {
    const io = testing.io;
    var f = try Fixture.init(.{ .budget_bytes = test_budget, .batch_ms = 0 }, .bdev_discard, 16);
    defer f.deinit();
    var g: FakeGate = .{};
    f.r.setDurableGate(g.gate());
    try f.mintPublished(4);
    const elided0 = stats.block_free_reclaim_elided.load();
    const holds0 = stats.block_free_durable_holds.load();
    const releases0 = stats.block_free_durable_releases.load();
    const forced0 = stats.block_free_durable_forced.load();
    const reachable0 = f.alloc.reachableFreeBlocks();
    // The freeing commit's window was reserved (mark 1) and answered; its
    // barrier has not completed.
    g.hwm = 1;
    try testing.expectEqual(FreeOutcome.terminal, f.r.freeBlock(io, 0, 1, f.alloc.stampOf(1)));
    try testing.expectEqual(block_alloc.State.limbo, f.alloc.stateOf(1));
    try testing.expect(f.alloc.inflightContains(1));
    try testing.expectEqual(@as(u64, 1), f.r.heldFrees());
    try testing.expectEqual(holds0 + 1, stats.block_free_durable_holds.load());
    try testing.expectEqual(elided0, stats.block_free_reclaim_elided.load());
    try testing.expectEqual(@as(u64, 0), f.alloc.elidedDebtBytes()); // nothing discards it either
    // Supply in sight: `df` counts it free, the valve's predicate sees it.
    try testing.expectEqual(reachable0 + 1, f.alloc.reachableFreeBlocks());
    try testing.expect(f.r.pending());
    // A mint meanwhile is never the held block.
    try testing.expectEqual(@as(u64, 4), (try f.alloc.allocateBlock()).idx);
    f.alloc.publishBlock(4);
    // The barrier covering mark 1 completes; the next free (mark 2) releases
    // the sealed one and is held itself.
    g.durable = 1;
    g.hwm = 2;
    try testing.expectEqual(FreeOutcome.terminal, f.r.freeBlock(io, 0, 2, f.alloc.stampOf(2)));
    try testing.expectEqual(block_alloc.State.free, f.alloc.stateOf(1));
    try testing.expect(!f.alloc.inflightContains(1));
    try testing.expectEqual(block_alloc.State.limbo, f.alloc.stateOf(2));
    try testing.expectEqual(@as(u64, 1), f.r.heldFrees());
    try testing.expectEqual(releases0 + 1, stats.block_free_durable_releases.load());
    try testing.expectEqual(elided0 + 1, stats.block_free_reclaim_elided.load());
    try testing.expectEqual(@as(u64, 1), (try f.alloc.allocateBlock()).idx);
    f.alloc.publishBlock(1);
    // The synchronous drain pulls the barrier: everything held is released.
    _ = f.r.drainSync(io);
    try testing.expectEqual(forced0 + 1, stats.block_free_durable_forced.load());
    try testing.expectEqual(@as(u64, 1), g.forces);
    try testing.expectEqual(block_alloc.State.free, f.alloc.stateOf(2));
    try testing.expectEqual(@as(u64, 0), f.r.heldFrees());
    try testing.expect(!f.r.pending());
    try testing.expectEqual(reachable0, f.alloc.reachableFreeBlocks()); // block 4 minted, block 2 returned
    // A non-holder's last reference ships only once its record is durable.
    const Holder = struct {
        n: usize = 0,
        fn ship(ctx: *anyopaque, _: u64, _: u64) bool {
            const h: *@This() = @ptrCast(@alignCast(ctx));
            h.n += 1;
            return true;
        }
        fn active(_: *anyopaque) bool {
            return false;
        }
    };
    var holder: Holder = .{};
    f.r.setFreeTarget(0, .{ .ctx = &holder, .ship = &Holder.ship, .authorityActive = &Holder.active });
    g.hwm = 3;
    try testing.expectEqual(FreeOutcome.nonterminal, f.r.freeBlock(io, 0, 3, f.alloc.stampOf(3)));
    try testing.expectEqual(@as(usize, 0), holder.n);
    try testing.expectEqual(@as(u64, 1), f.r.heldFrees());
    g.durable = 3;
    _ = f.r.releaseDurable(io, false);
    try testing.expectEqual(@as(usize, 1), holder.n);
    try testing.expectEqual(@as(u64, 0), f.r.heldFrees());
    try testing.expect(!f.alloc.inflightContains(3));
}

test "a full store whose frees wait for their records' barrier never answers ENOSPC: the valve's drain pulls the barrier and the mint lands on the released block (charter §6.2)" {
    const io = testing.io;
    const enospc = @import("enospc.zig");
    var f = try Fixture.init(.{ .budget_bytes = test_budget, .batch_ms = 0 }, .bdev_discard, 8);
    defer f.deinit();
    var g: FakeGate = .{};
    f.r.setDurableGate(g.gate());
    try f.mintPublished(8);
    try testing.expectError(error.StorageFull, f.alloc.allocateBlock());
    g.hwm = 7;
    try testing.expectEqual(FreeOutcome.terminal, f.r.freeBlock(io, 0, 5, f.alloc.stampOf(5)));
    try testing.expectEqual(@as(u64, 1), f.r.heldFrees());
    try testing.expectError(error.StorageFull, f.alloc.allocateBlock());
    const m = try enospc.allocateWithValve(f.alloc, io, enospc.Valve.fromReclaimer(f.r));
    try testing.expectEqual(@as(u64, 5), m.idx);
    try testing.expectEqual(@as(u64, 1), g.forces);
    f.alloc.publishBlock(5);
    // With nothing held the valve's verdict stands: terminal, no barrier.
    try testing.expectError(error.StorageFull, enospc.allocateWithValve(f.alloc, io, enospc.Valve.fromReclaimer(f.r)));
    try testing.expectEqual(@as(u64, 1), g.forces);
}

/// A filesystem's corpse gather as the reclaimer's upstream: `kick` hands
/// the destroy to another thread (the cadence's fiber in the product),
/// which frees `idx` through the reclaimer and empties the gather.
const FakeGather = struct {
    r: *Reclaimer,
    alloc: *BlockAllocator,
    idx: ?u64,
    /// The corpse's file.
    ino: u64 = 7,
    gathered: Atomic(bool) = .init(true),
    kicked: Atomic(bool) = .init(false),
    kicks: Atomic(u32) = .init(0),
    /// The settle's verdict: the corpse's own `.visible_supply` mint,
    /// made by `settlingDestroyer` before the free.
    settled: ?block_alloc.AllocError!block_alloc.Minted = null,

    fn upstream(self: *FakeGather) Reclaimer.Upstream {
        return .{ .ctx = self, .pending = &pending, .kick = &kick, .owns = &owns };
    }

    fn pending(ctx: *anyopaque) bool {
        const self: *FakeGather = @ptrCast(@alignCast(ctx));
        return self.gathered.load(.acquire);
    }

    fn owns(ctx: *anyopaque, ino: u64) bool {
        const self: *FakeGather = @ptrCast(@alignCast(ctx));
        return ino == self.ino and self.gathered.load(.acquire);
    }

    fn kick(ctx: *anyopaque) void {
        const self: *FakeGather = @ptrCast(@alignCast(ctx));
        _ = self.kicks.fetchAdd(1, .acq_rel);
        self.kicked.store(true, .release);
    }

    /// The destroy: once kicked, the corpse's block freed (into the hold),
    /// then the gather empty — a pass's frees precede its corpses leaving
    /// the count (`kv_fs.drainGather`).
    fn destroyer(self: *FakeGather) void {
        while (!self.kicked.load(.acquire)) std.atomic.spinLoopHint();
        if (self.idx) |idx| _ = self.r.freeBlock(testing.io, 0, idx, self.alloc.stampOf(idx));
        self.gathered.store(false, .release);
    }

    /// `kv_fs.destroyCorpses`' order: the corpse's parked custody settles
    /// FIRST — its upload's mint, `.visible_supply`, for the corpse's own
    /// file — and only then the destroy frees.
    fn settlingDestroyer(self: *FakeGather) void {
        while (!self.kicked.load(.acquire)) std.atomic.spinLoopHint();
        self.settled = enospc_mod.allocateBlockGraceBounded(self.alloc, testing.io, enospc_mod.Valve.fromReclaimer(self.r), .visible_supply, null, self.ino);
        if (self.idx) |idx| _ = self.r.freeBlock(testing.io, 0, idx, self.alloc.stampOf(idx));
        self.gathered.store(false, .release);
    }
};

test "a full store with a delete's corpses gathered UPSTREAM of the hold never answers ENOSPC (decision 0003 §CR): the gather is supply in sight — the latch stays inert, the valve kicks the destroy, a `.visible_supply` mint parks until its frees land and mints the freed block; a gather whose destroy frees nothing is judged at the wall; nothing upstream ⇒ terminal at once" {
    const io = testing.io;
    const enospc = @import("enospc.zig");
    var f = try Fixture.init(.{ .budget_bytes = test_budget, .batch_ms = 0 }, .bdev_discard, 8);
    defer f.deinit();
    var g: FakeGate = .{};
    f.r.setDurableGate(g.gate());
    try f.mintPublished(8);
    try testing.expectError(error.StorageFull, f.alloc.allocateBlock());
    try testing.expect(f.alloc.freshSupplyExhausted());
    // The rm's corpse holds block 6; its destroy waits for the tick.
    g.hwm = 3;
    var gather: FakeGather = .{ .r = f.r, .alloc = f.alloc, .idx = 6 };
    const up = gather.upstream();
    f.r.bindUpstream(&up);
    defer f.r.bindUpstream(null);
    try testing.expect(f.r.pending());
    try testing.expect(!f.alloc.freshSupplyExhausted()); // inert: supply in sight
    const kicks0 = stats.block_free_upstream_kicks.load();
    const t = try std.Thread.spawn(.{}, FakeGather.destroyer, .{&gather});
    const m = try enospc.allocateBlockGraceBounded(f.alloc, io, enospc.Valve.fromReclaimer(f.r), .visible_supply, null, 8);
    t.join();
    try testing.expectEqual(@as(u64, 6), m.idx);
    f.alloc.publishBlock(6);
    try testing.expect(gather.kicks.load(.acquire) >= 1);
    try testing.expect(stats.block_free_upstream_kicks.load() > kicks0);
    try testing.expect(!f.r.pending());
    // A gather whose destroy frees nothing (an inline file's corpse): the
    // park is the wall's, then the loud refusal.
    log.testing.expectErrorLogs(1);
    var empty: FakeGather = .{ .r = f.r, .alloc = f.alloc, .idx = null };
    empty.kicked.store(true, .release);
    const up2 = empty.upstream();
    f.r.bindUpstream(&up2);
    try testing.expectError(error.StorageFull, enospc.allocateBlockGraceBounded(f.alloc, io, enospc.Valve.fromReclaimer(f.r), .visible_supply, null, 8));
    try log.testing.expectAllFired();
    // Nothing upstream: terminal at once.
    f.r.bindUpstream(null);
    try testing.expectError(error.StorageFull, enospc.allocateBlockGraceBounded(f.alloc, io, enospc.Valve.fromReclaimer(f.r), .visible_supply, null, 8));
    try testing.expect(f.alloc.freshSupplyExhausted());
}

test "a gather's own corpse is no supply for its own bytes (decision 0003 §CW): the destroy settles the corpse's parked custody before it frees, so the settle's `.visible_supply` mint FOR the corpse refuses at once — no park, no wall, no loud line — while a live file's mint parks on the same gather and lands the corpse's free; `pendingFor` leaves the owned corpse out, `pending` does not" {
    const io = testing.io;
    var f = try Fixture.init(.{ .budget_bytes = test_budget, .batch_ms = 0 }, .bdev_discard, 8);
    defer f.deinit();
    var g: FakeGate = .{};
    f.r.setDurableGate(g.gate());
    try f.mintPublished(8);
    try testing.expectError(error.StorageFull, f.alloc.allocateBlock());
    g.hwm = 3;
    // Ino 7's corpse holds block 6 and a parked block its destroy settles.
    var gather: FakeGather = .{ .r = f.r, .alloc = f.alloc, .idx = 6, .ino = 7 };
    const up = gather.upstream();
    f.r.bindUpstream(&up);
    defer f.r.bindUpstream(null);
    try testing.expect(f.r.pending());
    try testing.expect(f.r.pendingFor(8));
    try testing.expect(!f.r.pendingFor(7));
    try testing.expect(f.alloc.freshSupplyVisibleFor(8));
    try testing.expect(!f.alloc.freshSupplyVisibleFor(7));
    const walls0 = enospc_mod.stats.alloc_pressure_park_wall_refusals.load();
    const t = try std.Thread.spawn(.{}, FakeGather.settlingDestroyer, .{&gather});
    // The live file (ino 8) parks on the gather; the corpse's settle never
    // does, so the destroy frees block 6 and the live mint lands it.
    const m = try enospc_mod.allocateBlockGraceBounded(f.alloc, io, enospc_mod.Valve.fromReclaimer(f.r), .visible_supply, null, 8);
    t.join();
    try testing.expectEqual(@as(u64, 6), m.idx);
    f.alloc.publishBlock(6);
    try testing.expectError(error.StorageFull, gather.settled.?);
    try testing.expectEqual(walls0, enospc_mod.stats.alloc_pressure_park_wall_refusals.load());
    try testing.expect(!f.r.pending());
}

test "trim_drains_debt_with_counted_commands_and_returns_offsets; stale debt never discards an owned block; a fenced daemon never trims" {
    const io = testing.io;
    var f = try Fixture.init(.{ .budget_bytes = test_budget, .batch_ms = 0 }, .bdev_discard, 16);
    defer f.deinit();
    try f.mintPublished(6);
    for ([_]u64{ 1, 2, 3, 5 }) |idx| try testing.expectEqual(FreeOutcome.terminal, f.r.freeBlock(io, 0, idx, f.alloc.stampOf(idx)));
    try testing.expectEqual(@as(u64, 4 * 4096), f.alloc.elidedDebtBytes());
    // Block 3 is re-owned before the trim: its debt was cancelled by the claim.
    try testing.expectEqual(@as(u64, 1), (try f.alloc.allocateBlock()).idx);
    const trim0 = stats.block_free_trim_discards.load();
    const d = try f.r.trim(io, false);
    try testing.expectEqual(@as(u64, 3), d.blocks); // 2, 3, 5
    try testing.expectEqual(@as(u64, 3 * 4096), d.bytes);
    try testing.expectEqual(trim0 + 3, stats.block_free_trim_discards.load());
    try testing.expectEqual(@as(usize, 2), f.rec.commands.items.len); // [2,3] coalesced + [5]
    try testing.expectEqual(Range{ .start = 2 * 4096, .len = 2 * 4096, .k = 0 }, f.rec.commands.items[0]);
    try testing.expectEqual(@as(u64, 0), f.alloc.elidedDebtBytes());
    try testing.expectEqual(@as(u64, 0), f.alloc.trimClaimed());
    try testing.expectEqual(block_alloc.State.free, f.alloc.stateOf(2));
    try testing.expectEqual(block_alloc.State.allocated, f.alloc.stateOf(1)); // the owned block was never discarded
    // --full discards the WHOLE free list (6..15 virgin are not free-listed; 2,3,5 are).
    f.rec.commands.clearRetainingCapacity();
    const full = try f.r.trim(io, true);
    try testing.expectEqual(@as(u64, 3), full.blocks);
    // Fenced: refused, no command.
    const Fence = struct {
        fn yes(_: ?*anyopaque) bool {
            return true;
        }
    };
    f.r.setFenceProbe(.{ .ctx = null, .poll = &Fence.yes });
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Fenced, f.r.trim(io, true));
    try log.testing.expectAllFired();
}

test "fenced_guard_halts_worker_batch_reclaims: a fenced batch drops without finish_free; dismount likewise" {
    const io = testing.io;
    var f = try Fixture.init(.{ .budget_bytes = test_budget, .batch_ms = 0 }, .file_punch, 16);
    defer f.deinit();
    try f.mintPublished(4);
    try testing.expectEqual(FreeOutcome.terminal, f.r.freeBlock(io, 0, 1, f.alloc.stampOf(1)));
    try testing.expectEqual(FreeOutcome.terminal, f.r.freeBlock(io, 0, 2, f.alloc.stampOf(2)));
    const Fence = struct {
        var armed: bool = false;
        fn poll(_: ?*anyopaque) bool {
            return armed;
        }
    };
    Fence.armed = true;
    f.r.setFenceProbe(.{ .ctx = null, .poll = &Fence.poll });
    const halts = stats.block_free_reclaim_fence_halts.load();
    log.testing.expectErrorLogs(1); // the halt latch's one line; the batch's halt is a counted outcome, never a tripwire
    try testing.expectEqual(@as(u64, 2), f.r.drainSync(io));
    try log.testing.expectAllFired();
    try testing.expectEqual(halts + 1, stats.block_free_reclaim_fence_halts.load());
    try testing.expectEqual(@as(usize, 0), f.rec.commands.items.len);
    try testing.expectEqual(block_alloc.State.limbo, f.alloc.stateOf(1)); // the successor's recovery owns it
    try testing.expect(!f.alloc.inflightContains(1));
    try testing.expectEqual(@as(u64, 0), f.r.population());
    try testing.expect(!f.r.pending());
    Fence.armed = false;
    // Dismount: a batch after the teardown drops likewise.
    var g = try Fixture.init(.{ .budget_bytes = test_budget, .batch_ms = 0 }, .file_punch, 16);
    defer g.deinit();
    try g.mintPublished(2);
    try testing.expectEqual(FreeOutcome.terminal, g.r.freeBlock(io, 0, 1, g.alloc.stampOf(1)));
    g.r.closeAtDismount();
    const dh = counters.tripwireCount("block_free_reclaim_dismount_halts");
    stats.block_free_reclaim_dismount_halts.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(@as(u64, 1), g.r.drainSync(io));
    try log.testing.expectAllFired();
    try testing.expectEqual(dh + 1, counters.tripwireCount("block_free_reclaim_dismount_halts"));
    try testing.expectEqual(@as(usize, 0), g.rec.commands.items.len);
}

test "terminal_free_queues_and_background_worker_reclaims; adjacent_displaced_ranges_coalesce_into_fewer_device_commands; clean stop drains" {
    const io = testing.io;
    var f = try Fixture.init(.{ .budget_bytes = test_budget, .batch_ms = 0, .batch_blocks = 64, .lanes_per_dev = 4 }, .file_punch, 256);
    defer f.deinit();
    try f.mintPublished(200);
    try f.r.start(io);
    var i: u64 = 0;
    while (i < 128) : (i += 1) try testing.expectEqual(FreeOutcome.terminal, f.r.freeBlock(io, 0, i, f.alloc.stampOf(i)));
    // The worker drains everything; the room edge is our wait.
    var spins: u32 = 0;
    while (f.r.population() != 0 and spins < 20_000) : (spins += 1) {
        var w = f.r.room.notified();
        _ = w.enable(io);
        if (f.r.population() == 0) {
            w.cancel(io);
            break;
        }
        _ = w.waitUntilUncancelable(io, clock.monotonicNs() + MANNERS_TICK_MS * std.time.ns_per_ms);
    }
    f.r.stopAndJoin(io);
    try testing.expectEqual(@as(u64, 0), f.r.population());
    try testing.expectEqual(@as(u64, 128), f.alloc.freeBlocksCount());
    // 128 adjacent blocks landed in far fewer commands than blocks (contract 11).
    try testing.expect(f.rec.commands.items.len < 128);
    var bytes: u64 = 0;
    for (f.rec.commands.items) |c| bytes += c.len;
    try testing.expectEqual(@as(u64, 128 * 4096), bytes);
    try testing.expect(stats.block_free_reclaim_batches.load() >= 1);
    try testing.expectEqual(@as(u64, 0), stats.block_free_reclaim_queue_bytes.load());
}

test "at_cap_enqueue_parks_until_drain_relieves; a 0 ms bound soft-overflows at once and counts it; the valve's progress counter moves" {
    const io = testing.io;
    // Explicit cap 1 so the second enqueue is at cap; park bound 0 ⇒ overflow immediately.
    var f = try Fixture.init(.{ .budget_bytes = test_budget, .batch_ms = 0, .explicit_cap_blocks = 1, .explicit_park_bound_ms = 0 }, .file_punch, 16);
    defer f.deinit();
    try f.mintPublished(4);
    try testing.expectEqual(@as(u64, 1), f.r.queueCapBlocks());
    const parks0 = stats.block_free_reclaim_cap_parks.load();
    const over0 = stats.block_free_reclaim_cap_overflow.load();
    try testing.expectEqual(FreeOutcome.terminal, f.r.freeBlock(io, 0, 1, f.alloc.stampOf(1)));
    try testing.expectEqual(FreeOutcome.terminal, f.r.freeBlock(io, 0, 2, f.alloc.stampOf(2)));
    try testing.expectEqual(parks0 + 1, stats.block_free_reclaim_cap_parks.load());
    try testing.expectEqual(over0 + 1, stats.block_free_reclaim_cap_overflow.load());
    try testing.expectEqual(@as(u64, 2), f.r.population());
    const finished0 = f.r.finishedTotal();
    try testing.expectEqual(@as(u64, 2), f.r.drainSync(io));
    try testing.expectEqual(finished0 + 2, f.r.finishedTotal());
    // A real park ends on the room edge made by a concurrent drain, never the quantum.
    var g = try Fixture.init(.{ .budget_bytes = test_budget, .batch_ms = 0, .explicit_cap_blocks = 1, .explicit_park_bound_ms = 60_000 }, .file_punch, 16);
    defer g.deinit();
    try g.mintPublished(4);
    try testing.expectEqual(FreeOutcome.terminal, g.r.freeBlock(io, 0, 1, g.alloc.stampOf(1)));
    const Drainer = struct {
        fn run(io_: Io, r: *Reclaimer) void {
            // Wait until the producer is parked, then drain to make room.
            while (r.room.waiters(io_) == 0) std.Thread.yield() catch {};
            _ = r.drainSync(io_);
        }
    };
    var fut = io.concurrent(Drainer.run, .{ io, g.r }) catch |err| switch (err) {
        error.ConcurrencyUnavailable => return testkit.skip(@src(), .no_concurrency, "the executor refused a unit of concurrency"),
    };
    const t0 = clock.monotonicNs();
    try testing.expectEqual(FreeOutcome.terminal, g.r.freeBlock(io, 0, 2, g.alloc.stampOf(2)));
    const parked_ns = clock.monotonicNs() - t0;
    fut.await(io);
    try testing.expect(parked_ns < 10 * std.time.ns_per_s); // ≪ the 60 s quantum
    try testing.expectEqual(over0 + 1, stats.block_free_reclaim_cap_overflow.load()); // no overflow this time
    _ = g.r.drainSync(io);
}

test "drain_defers_under_foreground_and_catches_up_idle; thin_supply_drains_despite_moving_foreground" {
    const io = testing.io;
    var f = try Fixture.init(.{ .budget_bytes = test_budget, .batch_ms = 0 }, .file_punch, 64);
    defer f.deinit();
    try f.mintPublished(40);
    const Fg = struct {
        var bytes: Atomic(u64) = .init(0);
        fn read(_: ?*anyopaque) u64 {
            return bytes.load(.monotonic);
        }
    };
    Fg.bytes.store(0, .monotonic);
    f.r.setForegroundSignal(.{ .ctx = null, .read = &Fg.read });
    try f.r.start(io);
    // Foreground moving: the worker defers (zero commands) tick after tick.
    const deferred0 = stats.block_free_reclaim_deferred_ticks.load();
    var i: u64 = 0;
    while (i < 4) : (i += 1) {
        _ = Fg.bytes.fetchAdd(1 << 20, .monotonic);
        try testing.expectEqual(FreeOutcome.terminal, f.r.freeBlock(io, 0, i, f.alloc.stampOf(i)));
    }
    var spins: u32 = 0;
    while (stats.block_free_reclaim_deferred_ticks.load() == deferred0 and spins < 400) : (spins += 1) {
        _ = Fg.bytes.fetchAdd(1 << 20, .monotonic); // keep moving
        stallMs(io, 5);
    }
    try testing.expect(stats.block_free_reclaim_deferred_ticks.load() > deferred0);
    try testing.expectEqual(@as(usize, 0), f.rec.commands.items.len);
    try testing.expectEqual(@as(u64, 4), f.r.population());
    // Idle: the signal stops moving; the worker catches up to empty.
    spins = 0;
    while (f.r.population() != 0 and spins < 2000) : (spins += 1) stallMs(io, 5);
    try testing.expectEqual(@as(u64, 0), f.r.population());
    try testing.expect(f.rec.commands.items.len >= 1);
    // Thin supply (queued × 2 ≥ free supply) drains despite a moving foreground.
    const supply_drains0 = stats.block_free_reclaim_supply_drains.load();
    f.rec.commands.clearRetainingCapacity();
    // 64 blocks: 40 minted, 4 freed+drained ⇒ free 4, virgin 24 ⇒ supply 28; queue 14 ⇒ 28 ≥ 28.
    i = 4;
    while (i < 18) : (i += 1) {
        _ = Fg.bytes.fetchAdd(1 << 20, .monotonic);
        try testing.expectEqual(FreeOutcome.terminal, f.r.freeBlock(io, 0, i, f.alloc.stampOf(i)));
    }
    spins = 0;
    while (f.r.population() != 0 and spins < 2000) : (spins += 1) {
        _ = Fg.bytes.fetchAdd(1 << 20, .monotonic);
        stallMs(io, 5);
    }
    f.r.stopAndJoin(io);
    try testing.expectEqual(@as(u64, 0), f.r.population());
    try testing.expect(stats.block_free_reclaim_supply_drains.load() > supply_drains0);
}

test "the debt drainer: idle drains elided debt past the watermark to zero with counted trim commands; a read-only mount's free halts reclaims" {
    const io = testing.io;
    // 8 blocks: mint all 8 so the virgin tail is 0 — every byte of debt is past the watermark.
    var f = try Fixture.init(.{ .budget_bytes = test_budget, .batch_ms = 0 }, .bdev_discard, 8);
    defer f.deinit();
    try f.mintPublished(8);
    const passes0 = stats.block_free_debt_drain_passes.load();
    // Both frees land BEFORE the drainer starts (the first free's wake is a
    // stored permit): its first pass finds [2,3] whole. Started first, a
    // drainer scheduled between the two frees trimmed [2] alone — the one
    // command below then read two under a loaded host.
    for ([_]u64{ 2, 3 }) |idx| try testing.expectEqual(FreeOutcome.terminal, f.r.freeBlock(io, 0, idx, f.alloc.stampOf(idx)));
    try f.r.start(io);
    // Settled = the debt cancelled AND the claims returned: a pass cancels
    // the debt BEFORE it issues the command and returns the claims after,
    // so a wait on the debt alone read the command list mid-pass.
    var spins: u32 = 0;
    while ((f.alloc.elidedDebtBytes() != 0 or f.alloc.trimClaimed() != 0) and spins < 2000) : (spins += 1) stallMs(io, 5);
    try testing.expectEqual(@as(u64, 0), f.alloc.elidedDebtBytes());
    try testing.expect(stats.block_free_debt_drain_passes.load() > passes0);
    try testing.expectEqual(@as(usize, 1), f.rec.commands.items.len); // [2,3] as one trim command
    try testing.expectEqual(block_alloc.State.free, f.alloc.stateOf(2));
    try testing.expectEqual(@as(u64, 0), f.alloc.trimClaimed());
    // The drainer went idle (no debt): a LATER elided free of the same
    // volume wakes it again — the wake is every 0 → > 0 transition, never
    // the volume's first free alone.
    try testing.expectEqual(FreeOutcome.terminal, f.r.freeBlock(io, 0, 5, f.alloc.stampOf(5)));
    spins = 0;
    while ((f.alloc.elidedDebtBytes() != 0 or f.alloc.trimClaimed() != 0) and spins < 2000) : (spins += 1) stallMs(io, 5);
    f.r.stopAndJoin(io);
    try testing.expectEqual(@as(u64, 0), f.alloc.elidedDebtBytes());
    try testing.expectEqual(@as(usize, 2), f.rec.commands.items.len);

    var ro = try Fixture.init(.{ .budget_bytes = test_budget, .batch_ms = 0, .read_only = true }, .file_punch, 8);
    defer ro.deinit();
    try ro.mintPublished(2);
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(FreeOutcome.terminal, ro.r.freeBlock(io, 0, 1, ro.alloc.stampOf(1)));
    try log.testing.expectAllFired();
    try testing.expect(ro.r.fenceHalted());
    try testing.expectEqual(@as(u64, 0), ro.r.population());
}

// Rung 9 F17: on a wrapped volume (virgin tail 0) every byte of debt is past
// the watermark, so under a foreground stream the shipped drainer looped
// `progressed ⇒ at once` and drained the whole backlog behind the write lane
// at full tilt. The pressure drain is paced to the device: ONE batch, then
// one batch-time at the measured completion rate (the cold ceiling until the
// meter learns).
test "the debt drainer under a FOREGROUND stream past the watermark issues one device-paced batch per pace, never the whole backlog at once; idle it drains to zero at full width" {
    const io = testing.io;
    // The drainer's time is the row's: the quiet window, the meter's first
    // window and the pace pass only when the row advances them (a stalled
    // venue once let the pace elapse — or the meter learn — between two
    // statements). 0 is the drainer's "not yet judged", so it starts at 1 s.
    const Clock = struct {
        var ns: Atomic(u64) = .init(0);
        fn now() u64 {
            return ns.load(.monotonic);
        }
    };
    Clock.ns.store(std.time.ns_per_s, .monotonic);
    var f = try Fixture.init(.{ .budget_bytes = test_budget, .batch_ms = 0, .batch_blocks = 4, .test_debt_clock = &Clock.now }, .bdev_discard, 32);
    defer f.deinit();
    try f.mintPublished(32); // the virgin tail is 0: the watermark is passed by the first byte of debt
    const Fg = struct {
        var bytes: Atomic(u64) = .init(0);
        var moving: Atomic(bool) = .init(false);
        /// A moving stream has new bytes at every look.
        fn read(_: ?*anyopaque) u64 {
            if (moving.load(.monotonic)) return bytes.fetchAdd(1, .monotonic) + 1;
            return bytes.load(.monotonic);
        }
    };
    Fg.bytes.store(1, .monotonic);
    Fg.moving.store(true, .monotonic);
    f.r.setForegroundSignal(.{ .ctx = null, .read = &Fg.read });
    const pressure0 = stats.block_free_debt_pressure_drains.load();
    stats.block_free_debt_pace_ms.set(0);
    // 16 blocks of debt, scattered (every other block): four batches'
    // worth, landed under a moving foreground BEFORE the drainer starts
    // (the first free's wake is a stored permit: pass 1 finds the debt,
    // pass 2 rides the permit — deterministic).
    var idx: u64 = 0;
    while (idx < 32) : (idx += 2) {
        _ = Fg.bytes.fetchAdd(1 << 20, .monotonic);
        try testing.expectEqual(FreeOutcome.terminal, f.r.freeBlock(io, 0, idx, f.alloc.stampOf(idx)));
    }
    try testing.expectEqual(@as(u64, 16 * 4096), f.alloc.elidedDebtBytes());
    try f.r.start(io);
    // Two pressure passes of ONE batch each (4 blocks, the meter still
    // cold: the row's clock stands), each resting the cold ceiling — a rest
    // only a wake ends — so the other 8 are still owed (the shipped hot
    // loop drained all 16 at once).
    var spins: u32 = 0;
    while ((stats.block_free_debt_pressure_drains.load() < pressure0 + 2 or f.alloc.elidedDebtBytes() > 8 * 4096) and spins < 2000) : (spins += 1) stallMs(io, 5);
    try testing.expectEqual(@as(u64, 8 * 4096), f.alloc.elidedDebtBytes());
    try testing.expectEqual(pressure0 + 2, stats.block_free_debt_pressure_drains.load());
    try testing.expectEqual(PARK_BOUND_CEILING_MS, stats.block_free_debt_pace_ms.load());
    // The foreground stops and the quiet window and the pace pass on the
    // row's clock: the woken drainer drains the rest at full width, with
    // no third pressure pass.
    Fg.moving.store(false, .monotonic);
    _ = Clock.ns.fetchAdd((IDLE_CONFIRM_TICKS * MANNERS_TICK_MS + PARK_BOUND_CEILING_MS) * std.time.ns_per_ms, .monotonic);
    f.r.debt_wake.notifyOne(io);
    spins = 0;
    while (f.alloc.elidedDebtBytes() != 0 and spins < 2000) : (spins += 1) stallMs(io, 5);
    f.r.stopAndJoin(io);
    try testing.expectEqual(@as(u64, 0), f.alloc.elidedDebtBytes());
    try testing.expectEqual(pressure0 + 2, stats.block_free_debt_pressure_drains.load());
    try testing.expectEqual(@as(u64, 0), f.alloc.trimClaimed());
    try testing.expect(f.r.debtDrainRate() > 0); // the trim venue fed the meter
}

test "a refused device command counts skipped and never falls back to a zeroing write; a skip-class substrate issues nothing" {
    const io = testing.io;
    var f = try Fixture.init(.{ .budget_bytes = test_budget, .batch_ms = 0 }, .bdev_discard, 8);
    defer f.deinit();
    f.r.cfg.elision = false;
    f.rec.refuse = true;
    try f.mintPublished(3);
    const skipped0 = stats.block_free_reclaim_skipped.load();
    try testing.expectEqual(FreeOutcome.terminal, f.r.freeBlock(io, 0, 1, f.alloc.stampOf(1)));
    try testing.expectEqual(@as(u64, 1), f.r.drainSync(io));
    try testing.expectEqual(skipped0 + 1, stats.block_free_reclaim_skipped.load());
    try testing.expectEqual(block_alloc.State.free, f.alloc.stateOf(1)); // reallocatable regardless
    var g = try Fixture.init(.{ .budget_bytes = test_budget, .batch_ms = 0 }, .skip, 8);
    defer g.deinit();
    try g.mintPublished(3);
    try testing.expectEqual(FreeOutcome.terminal, g.r.freeBlock(io, 0, 1, g.alloc.stampOf(1)));
    try testing.expectEqual(@as(u64, 1), g.r.drainSync(io));
    try testing.expectEqual(skipped0 + 2, stats.block_free_reclaim_skipped.load());
    try testing.expectEqual(@as(usize, 0), g.rec.commands.items.len);
}

test "a device that answers NotSupported is asked once: later frees count skipped without a command (generic/767's scsi_debug: 42 warnings in one pass)" {
    const io = testing.io;
    var f = try Fixture.init(.{ .budget_bytes = test_budget, .batch_ms = 0 }, .bdev_discard, 8);
    defer f.deinit();
    f.r.cfg.elision = false;
    f.rec.refuse = true;
    try f.mintPublished(6);
    const skipped0 = stats.block_free_reclaim_skipped.load();
    for ([_]u64{ 1, 3, 5 }) |b| {
        try testing.expectEqual(FreeOutcome.terminal, f.r.freeBlock(io, 0, b, f.alloc.stampOf(b)));
        try testing.expectEqual(@as(u64, 1), f.r.drainSync(io));
    }
    try testing.expectEqual(@as(u64, 1), f.rec.refused.load(.monotonic));
    try testing.expectEqual(skipped0 + 3, stats.block_free_reclaim_skipped.load());
    for ([_]u64{ 1, 3, 5 }) |b| try testing.expectEqual(block_alloc.State.free, f.alloc.stateOf(b));
}

test "stats JSON names every family; config from env reads the registry rows" {
    var buf: [4096]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"block_free_reclaim_queued\":") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"block_free_reclaim_park_bound_ms\":") != null);
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try env.put("SQUEEZEFS_RECLAIM_BATCH_BLOCKS", "8");
    try env.put("SQUEEZEFS_RECLAIM_CAP_PARK_MS", "1000");
    try env.put("SQUEEZEFS_DISCARD_ELISION", "0");
    const cfg = Config.fromEnv(&env, test_budget);
    try testing.expectEqual(@as(u32, 8), cfg.batch_blocks);
    try testing.expectEqual(@as(?u64, 1000), cfg.explicit_park_bound_ms);
    try testing.expectEqual(@as(?u64, null), cfg.explicit_cap_blocks);
    try testing.expect(!cfg.elision);
    try testing.expectEqual(DEFAULT_LANES_PER_DEV, cfg.lanes_per_dev);
}
