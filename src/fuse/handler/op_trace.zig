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

/// `{"fuse_op_latency_ns":{"<class>":{histogram},…}}` — the per-class
/// always-on latencies in the stats dump (the smoke bounds the slowest
/// bucket of every class).
pub fn writeLatencyJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeAll("{\"fuse_op_latency_ns\":{");
    inline for (@typeInfo(OpClass).@"enum".field_names, 0..) |name, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("\"{s}\":", .{name});
        try fuse_op_latency_ns[i].snapshot().writeJson(w);
    }
    try w.writeAll("}}");
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

    pub fn begin(unique: u64, class: OpClass) OpTrace {
        return .{ .unique = unique, .class = class, .profiling = armed(), .t_entry = clock.monotonicNs() };
    }

    /// Stamp a stage: the station moves; under profiling the two phase
    /// boundaries take a clock read (the FIRST backend touch wins — a
    /// multi-commit op's later touches extend `backend`, not the prelude).
    pub fn stamp(self: *OpTrace, s: Stage) void {
        self.last_stage = s;
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
