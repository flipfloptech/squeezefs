//! Per-op trace stages and the `SQUEEZEFS_OP_PROFILE` phase histograms
//! (`docs/porting/fuse_handlers_lanes.md` §7; charter §10 "a per-op trace
//! names stages"; decision 0002 ledger L12/L13).
//!
//! `Stage` is the wire vocabulary (`.trace` ships the id → name table): each
//! stage is the END of the phase named beside it, so per-op stage deltas
//! ARE the phase spans. The stamps are `clock.monotonicNs` — CLOCK_MONOTONIC
//! is the clock `bpftrace`'s `nsecs` and `perf record -k CLOCK_MONOTONIC`
//! use, so a `unique`-keyed join against `fuse:fuse_request_{send,end}` is a
//! MEASUREMENT with no epoch conversion (L13).
//!
//! `OpTrace` is a plain value on the handler's frame (L12: the reference
//! kept its phase stamps in atomics because a future could migrate lanes; a
//! Zig handler runs to completion on one lane). The per-op cost:
//!
//! - always: ONE relaxed load of the armed word at `begin` (cached in the
//!   trace — every later stamp is a plain field compare), two clock reads
//!   (entry, finish) feeding `fuse_op_latency_ns[class]`, the always-on
//!   total the watchdog's derived threshold is measured from (§6), and one
//!   relaxed store of the station per stage; no allocation.
//! - armed (`SQUEEZEFS_OP_PROFILE=1`): two more clock reads (the first
//!   backend touch, the commit's return) into the exact-sum
//!   `fuse_op_phase_ns[class][phase]` family, `handler_to_backend + backend
//!   + backend_to_reply ≡ total` to the nanosecond.
//!
//! The sampled trace RING (`SQUEEZEFS_OP_TRACE`, drained by the `.trace`
//! inode) is the rung-5 stats surface and is not here: the vocabulary and
//! the stamps it would carry are.

const std = @import("std");
const abi = @import("../abi.zig");
const clock = @import("../../core/clock.zig");
const counters = @import("../../core/counters.zig");
const knobs = @import("../../core/knobs.zig");

/// The stage vocabulary with its wire ids (op_trace_core.rs:62–283).
pub const Stage = enum(u8) {
    // transport
    transport_recv = 1,
    dispatch = 2,
    handler_entry = 3,
    handler_return = 4,
    reply_commit = 5,
    fast_dispatch = 6,
    // read serve
    read_routed = 16,
    meta_resolved = 17,
    keys_resolved = 18,
    tier_probed = 19,
    cohort_served = 20,
    block_fetched = 21,
    binding_checked = 22,
    serve_copied = 23,
    read_validated = 24,
    read_return = 25,
    // device + fill
    dev_submit = 32,
    dev_complete = 33,
    fetch_done = 34,
    decoded = 35,
    admitted = 36,
    deposited = 37,
    fill_done = 38,
    bridge_sent = 39,
    bridge_taken = 40,
    // write pipeline
    write_admitted = 48,
    write_detached = 49,
    write_locked = 50,
    write_encoded = 51,
    write_allocated = 52,
    write_dma_done = 53,
    write_published = 54,
    write_freed = 55,
    write_inval_done = 56,
    write_done = 57,
    // publish conveyor
    publish_enqueue = 64,
    publish_drained = 65,
    publish_locked = 66,
    publish_base_ready = 67,
    publish_applied = 68,
    publish_encoded = 69,
    publish_blob_written = 70,
    publish_committed = 71,
    commit_guarded = 72,
    commit_inode_read = 73,
    commit_slot_probed = 74,
    commit_tx_done = 75,
    publish_done = 76,
    // meta journal conveyor
    meta_enqueue = 80,
    pass_begin = 81,
    pass_admitted = 82,
    pass_locked = 83,
    journal_written = 84,
    journal_prefix_done = 85,
    barrier = 86,
    pass_end = 87,
    fanout = 88,
    // metadata op
    meta_backend_start = 96,
    meta_backend_done = 97,
    meta_op_return = 98,
    // il ring
    ipc_ingress = 112,
    ipc_dequeue = 113,
    ipc_admitted = 114,
    ipc_sq_enter = 115,
    ipc_cqe = 116,
    ipc_complete = 117,
    // uring_fs write
    ufs_submit = 120,
    ufs_admit = 121,
    ufs_wake = 122,
    ufs_observed = 123,
    // locks
    stripe_lock_acquired = 128,
    stripe_lock_released = 129,
    dlm_guard_acquired = 130,
    dlm_guard_released = 131,

    pub const all = std.enums.values(Stage);

    pub fn name(self: Stage) []const u8 {
        return @tagName(self);
    }

    /// The station name for an unregistered word (a slot read before its
    /// first stage).
    pub fn nameOf(id: u8) []const u8 {
        return if (std.enums.fromInt(Stage, id)) |s| s.name() else "none";
    }
};

/// The op classes the phase histograms and the watchdog's per-class tail
/// are keyed on: the reference's `META_OP_KINDS` (lookup, getattr, setattr,
/// mkdir, create, unlink, rename, readdir, readdirplus) plus the two data
/// ops and one bucket for everything else (`fuse_op_phase_ns[op]`).
pub const OpClass = enum(u8) {
    lookup,
    getattr,
    setattr,
    mkdir,
    create,
    unlink,
    rename,
    readdir,
    read,
    write,
    other,

    pub const count = @typeInfo(OpClass).@"enum".field_names.len;

    pub fn of(op: abi.Opcode) OpClass {
        return switch (op) {
            .lookup => .lookup,
            .getattr => .getattr,
            .setattr => .setattr,
            .mkdir, .mknod, .symlink, .link => .mkdir,
            .create => .create,
            .unlink, .rmdir => .unlink,
            .rename, .rename2 => .rename,
            .readdir, .readdirplus => .readdir,
            .read => .read,
            .write => .write,
            else => .other,
        };
    }
};

/// `fuse_op_phase_ns[op][phase]` (fuse_client.rs:1384–1390).
pub const Phase = enum(u8) {
    handler_to_backend,
    backend,
    backend_to_reply,
    total,

    pub const count = @typeInfo(Phase).@"enum".field_names.len;
};

/// The armed word: stored once at mount from the registry row, read once
/// per op. `std::env::var` per op cost the reference ≈ 0.7 % of daemon
/// cycles (fuse_client.rs:198–201); here the env is read at `arm` alone.
var profile_armed: std.atomic.Value(bool) = .init(false);

pub fn arm(env: *const std.process.Environ.Map) void {
    profile_armed.store(knobs.boolKnob("SQUEEZEFS_OP_PROFILE", env), .monotonic);
}

pub fn armed() bool {
    return profile_armed.load(.monotonic);
}

/// Test seam: flip the armed word without an environment.
pub fn setArmedForTest(on: bool) void {
    std.debug.assert(@import("builtin").is_test);
    profile_armed.store(on, .monotonic);
}

/// `fuse_op_latency_ns[class]`: every op's entry → finish, ALWAYS (the
/// watchdog measures each class's p99 here; charter §4 "overdue").
pub var fuse_op_latency_ns: [OpClass.count]counters.ShardedLatencyHistogram = @splat(.{});
/// `fuse_op_phase_ns[class][phase]`: armed only; exact-sum.
pub var fuse_op_phase_ns: [OpClass.count][Phase.count]counters.ShardedLatencyHistogram = @splat(@splat(.{}));

pub fn phaseHistogram(class: OpClass, phase: Phase) *counters.ShardedLatencyHistogram {
    return &fuse_op_phase_ns[@backingInt(class)][@backingInt(phase)];
}

pub fn latencyHistogram(class: OpClass) *counters.ShardedLatencyHistogram {
    return &fuse_op_latency_ns[@backingInt(class)];
}

/// `{"<class>":{histogram},…}` — the per-class always-on latencies (the
/// `ops` family's `fuse_op_latency_ns`; the smoke bounds the slowest
/// bucket of every class).
pub fn writeLatencyClassesJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeByte('{');
    inline for (@typeInfo(OpClass).@"enum".field_names, 0..) |name, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("\"{s}\":", .{name});
        try fuse_op_latency_ns[i].snapshot().writeJson(w);
    }
    try w.writeByte('}');
}

/// `{"<class>":{"<phase>":{histogram},…},…}` — the armed-only phase family
/// (`SQUEEZEFS_OP_PROFILE`; the aggregator emits it iff `armed()`).
pub fn writePhaseClassesJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeByte('{');
    inline for (@typeInfo(OpClass).@"enum".field_names, 0..) |cname, ci| {
        if (ci != 0) try w.writeByte(',');
        try w.print("\"{s}\":{{", .{cname});
        inline for (@typeInfo(Phase).@"enum".field_names, 0..) |pname, pi| {
            if (pi != 0) try w.writeByte(',');
            try w.print("\"{s}\":", .{pname});
            try fuse_op_phase_ns[ci][pi].snapshot().writeJson(w);
        }
        try w.writeByte('}');
    }
    try w.writeByte('}');
}

// ---------------------------------------------------------------------------
// The sampled trace RING (`SQUEEZEFS_OP_TRACE`; `docs/porting/
// fuse_handlers_lanes.md` §7.1; `stats_cli.md` §1.6): one ring per stripe
// (the counters' per-thread stripe is the producer index), Lamport SPSC
// over three-word slots, a full ring DROPS and counts, the `.trace`
// lookup DRAINS every ring into one immutable payload.
// ---------------------------------------------------------------------------

/// `OPS_PER_CORE_PER_S`: measured 1.03–1.04 M IOPS flat on 32 cores ≈ 32 k
/// per core, rounded up. `STAGES_PER_OP`: the longest chain is 24 stages →
/// the next power of two. `DRAIN_INTERVAL_S`: the rigs' 1 Hz poll.
/// `SAMPLE_BYTES`: three u64 words. `RING_MIN_CAPACITY`: 32 longest chains
/// — the physical minimum, never a tuning floor.
pub const OPS_PER_CORE_PER_S: u64 = 40_000;
pub const STAGES_PER_OP: u64 = 32;
pub const DRAIN_INTERVAL_S: u64 = 1;
pub const SAMPLE_BYTES: u64 = 24;
pub const RING_MIN_CAPACITY: u64 = 1024;

/// The pool geometry derived from the machine and the budget (§7.1):
/// `rings = stripes`, `pool_bytes = max(budget >> 10, rings × 1024 × 24)`,
/// `ring_capacity = prev_pow2(pool_bytes / rings / 24)`, `divisor` = the
/// smallest N such that one drain interval of the op ceiling at the
/// longest chain fits the pool.
pub const Geometry = struct {
    rings: u32,
    ring_capacity: u32,
    divisor: u32,

    pub fn derive(cpus: u32, budget_bytes: u64) Geometry {
        const rings: u64 = counters.stripeCount(cpus);
        const pool_bytes = @max(budget_bytes >> 10, rings * RING_MIN_CAPACITY * SAMPLE_BYTES);
        const cap_raw = pool_bytes / rings / SAMPLE_BYTES;
        const capacity: u64 = @max(RING_MIN_CAPACITY, std.math.floorPowerOfTwo(u64, cap_raw));
        const demand = @as(u64, cpus) * OPS_PER_CORE_PER_S * STAGES_PER_OP * DRAIN_INTERVAL_S;
        const divisor = @max(1, (demand + rings * capacity - 1) / (rings * capacity));
        return .{ .rings = @intCast(rings), .ring_capacity = @intCast(capacity), .divisor = @intCast(@min(divisor, std.math.maxInt(u32))) };
    }

    pub fn poolBytes(self: Geometry) u64 {
        return @as(u64, self.rings) * self.ring_capacity * SAMPLE_BYTES;
    }
};

/// One sample: `(op_id, stage, mono_ns)`.
pub const Sample = struct { op_id: u64, stage: u8, mono_ns: u64 };

const Slot = struct {
    op_id: std.atomic.Value(u64) = .init(0),
    stage: std.atomic.Value(u64) = .init(0),
    mono_ns: std.atomic.Value(u64) = .init(0),
};

/// Lamport SPSC: the producer's Release store of `head` publishes the three
/// words to the consumer's Acquire load; the producer's Acquire load of
/// `tail` against the consumer's Release store is the reuse edge.
const Ring = struct {
    slots: []Slot,
    head: std.atomic.Value(u64) align(std.atomic.cache_line) = .init(0),
    tail: std.atomic.Value(u64) align(std.atomic.cache_line) = .init(0),

    fn push(self: *Ring, s: Sample) bool {
        const h = self.head.load(.monotonic);
        // acquire pairs with the consumer's release store of tail in `drainInto`.
        const t = self.tail.load(.acquire);
        if (h - t >= self.slots.len) return false;
        const sl = &self.slots[h & (self.slots.len - 1)];
        sl.op_id.store(s.op_id, .monotonic);
        sl.stage.store(s.stage, .monotonic);
        sl.mono_ns.store(s.mono_ns, .monotonic);
        // release pairs with the consumer's acquire load of head.
        self.head.store(h + 1, .release);
        return true;
    }

    fn drainInto(self: *Ring, gpa: std.mem.Allocator, out: *std.ArrayList(Sample)) std.mem.Allocator.Error!void {
        // acquire pairs with the producer's release store of head in `push`.
        const h = self.head.load(.acquire);
        var t = self.tail.load(.monotonic);
        while (t < h) : (t += 1) {
            const sl = &self.slots[t & (self.slots.len - 1)];
            try out.append(gpa, .{ .op_id = sl.op_id.load(.monotonic), .stage = @intCast(sl.stage.load(.monotonic)), .mono_ns = sl.mono_ns.load(.monotonic) });
        }
        // release pairs with the producer's acquire load of tail.
        self.tail.store(t, .release);
    }
};

/// The armed pool. Published through ONE atomic pointer (non-null ⇔
/// armed); allocated at `armTrace`, never on the hot path; disarming nulls
/// the pointer and the undrained samples stay readable through `drain`.
pub const Pool = struct {
    gpa: std.mem.Allocator,
    geometry: Geometry,
    rings: []Ring,
    slots: []Slot,
    threshold: u32,
    pushed: counters.ShardedCounter = .{},
    dropped: counters.ShardedCounter = .{},
    /// The `.trace` drain serializes consumers (one drain at a time).
    drain_lock: std.atomic.Value(bool) = .init(false),

    pub fn init(gpa: std.mem.Allocator, geo: Geometry) std.mem.Allocator.Error!*Pool {
        const p = try gpa.create(Pool);
        errdefer gpa.destroy(p);
        const slots = try gpa.alloc(Slot, @as(usize, geo.rings) * geo.ring_capacity);
        errdefer gpa.free(slots);
        @memset(slots, .{});
        const rings = try gpa.alloc(Ring, geo.rings);
        for (rings, 0..) |*r, i| r.* = .{ .slots = slots[i * geo.ring_capacity ..][0..geo.ring_capacity] };
        p.* = .{ .gpa = gpa, .geometry = geo, .rings = rings, .slots = slots, .threshold = std.math.maxInt(u32) / geo.divisor };
        return p;
    }

    pub fn deinit(self: *Pool) void {
        const gpa = self.gpa;
        gpa.free(self.rings);
        gpa.free(self.slots);
        gpa.destroy(self);
    }

    /// Deterministic per id so every hook on one op agrees (a traced op's
    /// chain is complete), stride-independent, one multiply.
    pub fn selected(self: *const Pool, op_id: u64) bool {
        return (op_id *% 0x9E37_79B9_7F4A_7C15) >> 32 < self.threshold;
    }

    pub fn push(self: *Pool, s: Sample) void {
        const r = &self.rings[counters.stripeIndex() % self.rings.len];
        if (r.push(s)) self.pushed.inc() else self.dropped.inc();
    }

    /// Every ring emptied into `out`, sorted by `(op_id, mono_ns)`.
    pub fn drain(self: *Pool, gpa: std.mem.Allocator, out: *std.ArrayList(Sample)) std.mem.Allocator.Error!void {
        while (self.drain_lock.swap(true, .acquire)) std.atomic.spinLoopHint();
        defer self.drain_lock.store(false, .release);
        for (self.rings) |*r| try r.drainInto(gpa, out);
        std.mem.sort(Sample, out.items, {}, sampleLess);
    }

    fn sampleLess(_: void, a: Sample, b: Sample) bool {
        if (a.op_id != b.op_id) return a.op_id < b.op_id;
        return a.mono_ns < b.mono_ns;
    }
};

var trace_pool: std.atomic.Value(?*Pool) = .init(null);

/// Arm the ring from the registry row (once, at mount): the geometry is
/// derived from the cpus and the budget; off, nothing is allocated.
pub fn armTrace(gpa: std.mem.Allocator, env: *const std.process.Environ.Map, cpus: u32, budget_bytes: u64) std.mem.Allocator.Error!void {
    if (!knobs.boolKnob("SQUEEZEFS_OP_TRACE", env)) return;
    try armTraceWith(gpa, Geometry.derive(cpus, budget_bytes));
}

pub fn armTraceWith(gpa: std.mem.Allocator, geo: Geometry) std.mem.Allocator.Error!void {
    const p = try Pool.init(gpa, geo);
    // release pairs with the hot path's acquire load in `tracePool`.
    if (trace_pool.swap(p, .acq_rel)) |old| old.deinit();
}

/// Disarm and free the pool (teardown).
pub fn disarmTrace() void {
    if (trace_pool.swap(null, .acq_rel)) |old| old.deinit();
}

fn tracePool() ?*Pool {
    // acquire pairs with `armTraceWith`'s release swap.
    return trace_pool.load(.acquire);
}

/// `op_trace_{armed,samples,dropped,divisor,pool_bytes}` (`R:op_trace.rs:
/// 280-288`): a stats read never drains.
pub fn writeGaugesJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    if (tracePool()) |p| {
        try w.print("{{\"op_trace_armed\":true,\"op_trace_samples\":{d},\"op_trace_dropped\":{d},\"op_trace_divisor\":{d},\"op_trace_rings\":{d},\"op_trace_ring_capacity\":{d},\"op_trace_pool_bytes\":{d}}}", .{ p.pushed.load(), p.dropped.load(), p.geometry.divisor, p.geometry.rings, p.geometry.ring_capacity, p.geometry.poolBytes() });
    } else {
        try w.writeAll("{\"op_trace_armed\":false,\"op_trace_samples\":0,\"op_trace_dropped\":0,\"op_trace_divisor\":0,\"op_trace_rings\":0,\"op_trace_ring_capacity\":0,\"op_trace_pool_bytes\":0}");
    }
}

/// The `.trace` payload (`stats_cli.md` §1.6): DRAINS every ring — a second
/// lookup reads empty; `samples_total`/`dropped` are cumulative over the
/// pool's life; `stages` is the wire vocabulary; `mono_ns` is
/// CLOCK_MONOTONIC. Ends with ONE `\n`.
pub fn writeTraceJson(gpa: std.mem.Allocator, w: *std.Io.Writer) (std.Io.Writer.Error || std.mem.Allocator.Error)!void {
    var samples: std.ArrayList(Sample) = .empty;
    defer samples.deinit(gpa);
    const pool = tracePool();
    if (pool) |p| try p.drain(gpa, &samples);
    try w.print("{{\"armed\":{},\"divisor\":{d},\"dropped\":{d},\"samples_total\":{d},\"clock\":\"CLOCK_MONOTONIC_ns\",\"stages\":{{", .{
        pool != null,
        if (pool) |p| p.geometry.divisor else 0,
        if (pool) |p| p.dropped.load() else 0,
        if (pool) |p| p.pushed.load() else 0,
    });
    for (Stage.all, 0..) |s, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("\"{d}\":\"{s}\"", .{ @backingInt(s), s.name() });
    }
    try w.writeAll("},\"samples\":[");
    for (samples.items, 0..) |s, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("[{d},{d},{d}]", .{ s.op_id, s.stage, s.mono_ns });
    }
    try w.writeAll("]}\n");
}

/// One op's trace, on the handler's frame.
pub const OpTrace = struct {
    unique: u64,
    class: OpClass,
    /// The armed word as read at `begin`: the one atomic load.
    profiling: bool,
    /// The last stage stamped — the watchdog's station.
    last_stage: Stage = .handler_entry,
    t_entry: u64,
    t_backend_start: u64 = 0,
    t_backend_done: u64 = 0,
    finished: bool = false,
    /// The sampled ring this op was SELECTED into (null: disarmed, or the
    /// selection law said no — one multiply at `begin`, nothing per stage).
    ring: ?*Pool = null,

    pub fn begin(unique: u64, class: OpClass) OpTrace {
        const t_entry = clock.monotonicNs();
        var t: OpTrace = .{ .unique = unique, .class = class, .profiling = armed(), .t_entry = t_entry };
        if (tracePool()) |p| if (unique != 0 and p.selected(unique)) {
            t.ring = p;
            p.push(.{ .op_id = unique, .stage = @backingInt(Stage.handler_entry), .mono_ns = t_entry });
        };
        return t;
    }

    /// Stamp a stage: the station moves; under profiling the two phase
    /// boundaries take a clock read (the FIRST backend touch wins — a
    /// multi-commit op's later touches extend `backend`, not the prelude);
    /// a selected op pushes one ring sample per stage.
    pub fn stamp(self: *OpTrace, s: Stage) void {
        self.last_stage = s;
        if (self.ring) |p| p.push(.{ .op_id = self.unique, .stage = @backingInt(s), .mono_ns = clock.monotonicNs() });
        if (!self.profiling) return;
        switch (s) {
            .meta_backend_start => if (self.t_backend_start == 0) {
                self.t_backend_start = clock.monotonicNs();
            },
            .meta_backend_done => self.t_backend_done = clock.monotonicNs(),
            else => {},
        }
    }

    /// The op's end: `total` always; the three phases when armed. Drop-
    /// based in the reference so an error exit records truthfully; here the
    /// dispatcher calls it on every exit path and asserts exactly once.
    pub fn finish(self: *OpTrace) void {
        std.debug.assert(!self.finished);
        self.finished = true;
        const now = clock.monotonicNs();
        const total = now -| self.t_entry;
        latencyHistogram(self.class).record(total);
        if (self.ring) |p| p.push(.{ .op_id = self.unique, .stage = @backingInt(Stage.handler_return), .mono_ns = now });
        if (!self.profiling) return;
        // An op that never touched the backend puts its whole span in
        // `backend_to_reply` (the reply-side phase), keeping Σ ≡ total.
        const bs = if (self.t_backend_start != 0) self.t_backend_start else self.t_entry;
        const bd = if (self.t_backend_done != 0) @max(self.t_backend_done, bs) else bs;
        phaseHistogram(self.class, .handler_to_backend).record(bs -| self.t_entry);
        phaseHistogram(self.class, .backend).record(bd -| bs);
        phaseHistogram(self.class, .backend_to_reply).record(now -| bd);
        phaseHistogram(self.class, .total).record(total);
    }
};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

test "stage table is total and unique: every id decodes to its name and back; the ids are the reference's wire vocabulary" {
    var seen: [256]bool = @splat(false);
    for (Stage.all) |s| {
        const id = @backingInt(s);
        try testing.expect(!seen[id]);
        seen[id] = true;
        try testing.expectEqualStrings(s.name(), Stage.nameOf(id));
    }
    try testing.expectEqual(@as(u8, 1), @backingInt(Stage.transport_recv));
    try testing.expectEqual(@as(u8, 32), @backingInt(Stage.dev_submit));
    try testing.expectEqual(@as(u8, 39), @backingInt(Stage.bridge_sent));
    try testing.expectEqual(@as(u8, 57), @backingInt(Stage.write_done));
    try testing.expectEqual(@as(u8, 76), @backingInt(Stage.publish_done));
    try testing.expectEqual(@as(u8, 98), @backingInt(Stage.meta_op_return));
    try testing.expectEqual(@as(u8, 131), @backingInt(Stage.dlm_guard_released));
    try testing.expectEqualStrings("none", Stage.nameOf(0));
    try testing.expectEqualStrings("none", Stage.nameOf(200));
}

test "disarmed: begin reads the armed word once, stamps move the station without a clock read, finish records total only" {
    setArmedForTest(false);
    defer setArmedForTest(false);
    const before = latencyHistogram(.lookup).snapshot().count;
    const phase_before = phaseHistogram(.lookup, .total).snapshot().count;
    var t = OpTrace.begin(7, .lookup);
    try testing.expect(!t.profiling);
    t.stamp(.meta_backend_start);
    t.stamp(.meta_backend_done);
    try testing.expectEqual(Stage.meta_backend_done, t.last_stage);
    try testing.expectEqual(@as(u64, 0), t.t_backend_start);
    try testing.expectEqual(@as(u64, 0), t.t_backend_done);
    t.finish();
    try testing.expectEqual(before + 1, latencyHistogram(.lookup).snapshot().count);
    try testing.expectEqual(phase_before, phaseHistogram(.lookup, .total).snapshot().count);
}

test "armed: the four phases are exact-sum (Σ three ≡ total to the ns) and the first backend touch wins" {
    setArmedForTest(true);
    defer setArmedForTest(false);
    const class: OpClass = .rename;
    var snaps_before: [Phase.count]u64 = undefined;
    for (&snaps_before, 0..) |*s, i| s.* = phaseHistogram(class, @fromBackingInt(@intCast(i))).snapshot().sum_ns;
    var t = OpTrace.begin(9, class);
    try testing.expect(t.profiling);
    t.stamp(.meta_backend_start);
    const first = t.t_backend_start;
    try testing.expect(first >= t.t_entry);
    t.stamp(.meta_backend_start);
    try testing.expectEqual(first, t.t_backend_start);
    t.stamp(.meta_backend_done);
    try testing.expect(t.t_backend_done >= first);
    t.finish();
    var sums: [Phase.count]u64 = undefined;
    for (&sums, 0..) |*s, i| s.* = phaseHistogram(class, @fromBackingInt(@intCast(i))).snapshot().sum_ns - snaps_before[i];
    try testing.expectEqual(sums[3], sums[0] + sums[1] + sums[2]);
}

test "armed op with no backend touch: the whole span is backend_to_reply; Σ still ≡ total" {
    setArmedForTest(true);
    defer setArmedForTest(false);
    const class: OpClass = .other;
    var before: [Phase.count]u64 = undefined;
    for (&before, 0..) |*s, i| s.* = phaseHistogram(class, @fromBackingInt(@intCast(i))).snapshot().sum_ns;
    var t = OpTrace.begin(11, class);
    t.finish();
    const h2b = phaseHistogram(class, .handler_to_backend).snapshot().sum_ns - before[0];
    const b = phaseHistogram(class, .backend).snapshot().sum_ns - before[1];
    const b2r = phaseHistogram(class, .backend_to_reply).snapshot().sum_ns - before[2];
    const total = phaseHistogram(class, .total).snapshot().sum_ns - before[3];
    try testing.expectEqual(@as(u64, 0), h2b);
    try testing.expectEqual(@as(u64, 0), b);
    try testing.expectEqual(total, b2r);
}

test "trace ring geometry: rings = stripes, capacity ≥ the physical minimum and a power of two, the divisor is the smallest N fitting one drain interval of the op ceiling" {
    const g = Geometry.derive(32, 16 << 30);
    try testing.expectEqual(@as(u32, 32), g.rings);
    // pool = 16 MiB → 16 MiB / 32 / 24 = 21,845 → prev_pow2 = 16,384.
    try testing.expectEqual(@as(u32, 16384), g.ring_capacity);
    // demand = 32 × 40 000 × 32 = 40.96 M samples/s over 524 288 slots → 79.
    try testing.expectEqual(@as(u32, 79), g.divisor);
    try testing.expectEqual(@as(u64, 32 * 16384 * 24), g.poolBytes());
    const small = Geometry.derive(1, 1 << 20);
    try testing.expectEqual(@as(u32, 1), small.rings);
    try testing.expectEqual(@as(u32, RING_MIN_CAPACITY), small.ring_capacity);
    try testing.expectEqual(@as(u32, 1250), small.divisor);
}

test "trace ring: armed, a selected op's chain lands in order; the drain empties the rings (a second drain is empty); a full ring drops and counts; disarmed costs one load and the trace reads empty" {
    disarmTrace();
    var buf: [8192]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeTraceJson(testing.allocator, &w);
    try testing.expect(std.mem.startsWith(u8, w.buffered(), "{\"armed\":false,\"divisor\":0,\"dropped\":0,\"samples_total\":0,\"clock\":\"CLOCK_MONOTONIC_ns\",\"stages\":{\"1\":\"transport_recv\","));
    try testing.expect(std.mem.endsWith(u8, w.buffered(), "\"samples\":[]}\n"));
    // Divisor 1: every op is selected; capacity = the minimum.
    try armTraceWith(testing.allocator, .{ .rings = 1, .ring_capacity = @intCast(RING_MIN_CAPACITY), .divisor = 1 });
    defer disarmTrace();
    var t = OpTrace.begin(77, .lookup);
    try testing.expect(t.ring != null);
    t.stamp(.meta_backend_start);
    t.stamp(.meta_backend_done);
    t.finish();
    var samples: std.ArrayList(Sample) = .empty;
    defer samples.deinit(testing.allocator);
    try tracePool().?.drain(testing.allocator, &samples);
    try testing.expectEqual(@as(usize, 4), samples.items.len);
    try testing.expectEqual(@backingInt(Stage.handler_entry), samples.items[0].stage);
    try testing.expectEqual(@backingInt(Stage.meta_backend_start), samples.items[1].stage);
    try testing.expectEqual(@backingInt(Stage.handler_return), samples.items[3].stage);
    try testing.expect(samples.items[0].mono_ns <= samples.items[3].mono_ns);
    samples.clearRetainingCapacity();
    try tracePool().?.drain(testing.allocator, &samples);
    try testing.expectEqual(@as(usize, 0), samples.items.len);
    // Overflow: RING_MIN_CAPACITY + 8 pushes drop 8.
    var i: u64 = 0;
    while (i < RING_MIN_CAPACITY + 8) : (i += 1) tracePool().?.push(.{ .op_id = 1, .stage = 1, .mono_ns = i });
    try testing.expectEqual(@as(u64, 8), tracePool().?.dropped.load());
    var w2: std.Io.Writer = .fixed(&buf);
    try writeGaugesJson(&w2);
    try testing.expect(std.mem.indexOf(u8, w2.buffered(), "\"op_trace_armed\":true") != null);
    try testing.expect(std.mem.indexOf(u8, w2.buffered(), "\"op_trace_dropped\":8") != null);
    // The gauges read never drains; the trace does.
    var big: std.Io.Writer.Allocating = .init(testing.allocator);
    defer big.deinit();
    try writeTraceJson(testing.allocator, &big.writer);
    try testing.expect(std.mem.indexOf(u8, big.written(), "\"dropped\":8") != null);
    try testing.expectEqual(@as(usize, RING_MIN_CAPACITY), std.mem.count(u8, big.written(), "[1,1,"));
    // The selection law is deterministic and stride-independent: the
    // same id always agrees with itself.
    const p = tracePool().?;
    try testing.expectEqual(p.selected(123456), p.selected(123456));
}

test "OpClass.of covers every opcode: the metadata kinds by name, the two data ops, the rest other" {
    try testing.expectEqual(OpClass.lookup, OpClass.of(.lookup));
    try testing.expectEqual(OpClass.mkdir, OpClass.of(.symlink));
    try testing.expectEqual(OpClass.unlink, OpClass.of(.rmdir));
    try testing.expectEqual(OpClass.rename, OpClass.of(.rename2));
    try testing.expectEqual(OpClass.readdir, OpClass.of(.readdirplus));
    try testing.expectEqual(OpClass.read, OpClass.of(.read));
    try testing.expectEqual(OpClass.write, OpClass.of(.write));
    try testing.expectEqual(OpClass.other, OpClass.of(.statfs));
    try testing.expectEqual(OpClass.other, OpClass.of(abi.Opcode.fromInt(999)));
}
