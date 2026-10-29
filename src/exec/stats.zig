//! The executor's gauges (charter §10; `docs/porting/exec.md` §3.11): every
//! mechanism's engagement gauge, every "cannot happen" a tripwire, folded
//! into one JSON object for the `.stats` inode. Portable: plain
//! `core/counters.zig` instruments; the lane writes a per-lane view through
//! `LaneView`.

const std = @import("std");
const Io = std.Io;
const counters = @import("../core/counters.zig");

/// The lane classes (charter §9's venues). `fuse` = `sqz-fuse-ln<N>`
/// handler lanes; `fuse_ur` = the `sqz-fuse-ur<qid>` transport workers
/// (rung 3's drain-group lanes, poll budget 1); `meta` = `sqz-meta<i>`;
/// `lease` = the heartbeat lane; `journal` = `sqz-jrnl<N>`; `main` = the
/// calling thread of `SqzIo.run`.
pub const LaneClass = enum(u8) {
    fuse,
    fuse_ur,
    meta,
    lease,
    journal,
    main,

    pub const count = @typeInfo(LaneClass).@"enum".field_names.len;
};

pub fn PerClass(comptime T: type) type {
    return struct {
        items: [LaneClass.count]T = @splat(.{}),

        pub fn of(self: *@This(), class: LaneClass) *T {
            return &self.items[@backingInt(class)];
        }
    };
}

/// `exec_fibers_live[class]`: fibers between spawn and recycle.
pub var fibers_live: PerClass(counters.Gauge) = .{};
/// `exec_fiber_stack_hwm_bytes`: the deepest stack use ever measured at a
/// fiber's recycle (the canary high-water mark; debug/test builds).
pub var fiber_stack_hwm_bytes: counters.Gauge = .{};
/// `exec_fiber_stack_bytes`: stack bytes MAPPED (the population's virtual
/// reserve as slots are touched) minus the usable regions the idle trim
/// returned.
pub var fiber_stack_bytes: counters.Gauge = .{};
/// `exec_fiber_stack_resident_bytes`: the pages the kernel holds for every
/// mapped stack, measured by `mincore` at each sampler tick — the
/// `mem_budget` component's gauge (`exec_fiber_stacks`): a 1 MiB stack
/// touched to 176 KiB counts 176 KiB (the suites' 752 MB "mapped" gauge
/// counted the reserve).
pub var fiber_stack_resident_bytes: counters.Gauge = .{};
/// `exec_fiber_stack_trimmed_bytes`: stack bytes returned by the idle trim
/// (Yellow/Red ticks).
pub var fiber_stack_trimmed_bytes: counters.ShardedCounter = .{};
/// `exec_polls[class]`: fiber switches into a fiber.
pub var polls: PerClass(counters.ShardedCounter) = .{};
/// `exec_poll_ns`: each poll's wall duration — the long-poll watchdog (a
/// CPU-bound fiber delays every CQE on its ring; risk §3.12.4).
pub var poll_ns: counters.ShardedLatencyHistogram = .{};
/// `exec_same_lane_wakes` / `exec_cross_lane_wakes`: homing engagement (a
/// queue-per-CPU session is ≈ all same-lane).
pub var same_lane_wakes: counters.ShardedCounter = .{};
pub var cross_lane_wakes: counters.ShardedCounter = .{};
/// `exec_wake_writes` / `exec_wake_elided`: the coalescer's ratio over every
/// lane (≈ 1 under saturation = a regression, charter §4).
pub var wake_writes: counters.ShardedCounter = .{};
pub var wake_elided: counters.ShardedCounter = .{};
/// `exec_lane_tick_rescues`: park ticks that found inbox work (≈ 0; growth
/// = a lost wake absorbed, loudly).
pub var lane_tick_rescues: counters.ShardedCounter = .{};
/// `exec_lane_backstop_ticks`: 100 ms backstop parks taken while a lane
/// owed a completion.
pub var lane_backstop_ticks: counters.ShardedCounter = .{};
/// `exec_driven_owner_queued_parks`: a TIMED raw futex park — a
/// primitive's queued wait (a lock's ticked park, a flight's cohort wait)
/// — by the owner thread of a DRIVEN lane, off a fiber: a wait only
/// another thread can end, since the owner's own handler fibers run only
/// in its pass. 0 on a healthy daemon; growth beside
/// `lock_ticked_reregisters` names an off-fiber blocking site on the drain
/// worker (`docs/evidence/rung-10-mdtest-wedge.md`). The interior mutex's
/// untimed park is not counted: its holds are other threads' nanoseconds.
pub var driven_owner_queued_parks: counters.ShardedCounter = .{};
/// `exec_spawn_refused[class]`: a lane at `fiber_capacity`
/// (`error.ConcurrencyUnavailable`; the transport tries the worker's own
/// lane and the rotation before it answers EAGAIN-class).
pub var spawn_refused: PerClass(counters.ShardedCounter) = .{};
/// `exec_fanout_inline[class]`: optional spawns (`Io.async`,
/// `Io.Group.async`) past the lane's `optional_cap` — run inline by their
/// caller, never in the fibers a FUSE lane keeps for its handlers.
pub var fanout_inline: PerClass(counters.ShardedCounter) = .{};
/// `exec_spawn_trim_waits`: spawns that popped a slot the idle trim was
/// returning and waited out its madvise (the trim claims in place; the
/// spawn is delayed, never refused).
pub var spawn_trim_waits: counters.ShardedCounter = .{};
/// `exec_timer_arms` / `exec_timer_fires`: the per-lane heaps.
pub var timer_arms: counters.ShardedCounter = .{};
pub var timer_fires: counters.ShardedCounter = .{};
/// `exec_timer_thread_tombstones`: cancelled entries the `sqz-timer`
/// thread skipped at pop.
pub var timer_thread_tombstones: counters.ShardedCounter = .{};
/// `exec_blocking_*`: the `sqz-blk` pool.
pub var blocking_threads: counters.Gauge = .{};
pub var blocking_queue_depth: counters.Gauge = .{};
pub var blocking_spawns: counters.ShardedCounter = .{};
pub var blocking_refused: counters.ShardedCounter = .{};
pub var blocking_reaped: counters.ShardedCounter = .{};
/// `exec_ring_ops`: SQEs fibers armed on their lane rings (READV/WRITEV/POLL_ADD).
pub var ring_ops: counters.ShardedCounter = .{};
/// `exec_ring_dead_held_ops`: ring ops a dead lane ring still holds because
/// the kernel took them (decision 0003 §AR.4). Each falls at its CQE; one
/// that never comes is leaked on purpose: its fiber is never resumed.
pub var ring_dead_held_ops: counters.Gauge = .{};

// Tripwires (must stay 0).
/// A second `readable(fd)` awaiter on one fd on one lane (a caller bug; the
/// first completes `.replaced`).
pub var fdwatch_double_await: counters.Tripwire = .init("exec_fdwatch_double_await");
/// A blocking `operate` (ioctl, net) ran inline on a fuse-class lane.
pub var blocking_op_on_fuse_lane: counters.Tripwire = .init("exec_blocking_op_on_fuse_lane");
/// `Io.Batch` awaited on a lane fiber (decision 0003 Q-D: unsupported in v1).
pub var batch_on_lane: counters.Tripwire = .init("exec_batch_on_lane");
/// A lane inbox push refused: the bound is 2 × fiber_capacity (one run entry
/// + one cancel per fiber), so this cannot happen.
pub var inbox_overflow: counters.Tripwire = .init("exec_inbox_overflow");
/// A fiber's parked frame used more than half its stack (the pin that
/// fails before an overflow ever can).
pub var fiber_stack_half: counters.Tripwire = .init("exec_fiber_stack_over_half");
/// A second concurrent awaiter of one `Io.Group`: the join has ONE awaiter
/// cell, so the earlier awaiter's wake is lost (`std.Io.Threaded` asserts
/// here; fstests generic/074 and 464 parked an open(O_TRUNC) for 40 min on
/// exactly this). A caller bug, named instead of silent.
pub var group_second_awaiter: counters.Tripwire = .init("exec_group_second_awaiter");

/// What the JSON needs from each lane.
pub const LaneView = struct {
    index: u16,
    class: LaneClass,
    alive: bool,
    queue_depth: u64,
    fibers_live: u64,
    tick_rescues: u64,
};

fn writePerClass(w: *Io.Writer, comptime key: []const u8, values: [LaneClass.count]u64) Io.Writer.Error!void {
    try w.writeAll("\"" ++ key ++ "\":{");
    inline for (@typeInfo(LaneClass).@"enum".field_names, 0..) |name, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("\"{s}\":{d}", .{ name, values[i] });
    }
    try w.writeByte('}');
}

fn perClassLoads(comptime T: type, pc: *PerClass(T)) [LaneClass.count]u64 {
    var out: [LaneClass.count]u64 = undefined;
    for (&pc.items, 0..) |*item, i| out[i] = item.load();
    return out;
}

/// One JSON object with every gauge above and a `lanes` array.
pub fn writeStatsJson(w: *Io.Writer, lanes: []const LaneView) Io.Writer.Error!void {
    try w.writeByte('{');
    try writePerClass(w, "exec_fibers_live", perClassLoads(counters.Gauge, &fibers_live));
    try w.print(",\"exec_fiber_stack_hwm_bytes\":{d},\"exec_fiber_stack_bytes\":{d},\"exec_fiber_stack_resident_bytes\":{d},\"exec_fiber_stack_trimmed_bytes\":{d},", .{ fiber_stack_hwm_bytes.load(), fiber_stack_bytes.load(), fiber_stack_resident_bytes.load(), fiber_stack_trimmed_bytes.load() });
    try writePerClass(w, "exec_polls", perClassLoads(counters.ShardedCounter, &polls));
    try w.writeAll(",\"exec_poll_ns\":");
    try poll_ns.snapshot().writeJson(w);
    try w.print(",\"exec_same_lane_wakes\":{d},\"exec_cross_lane_wakes\":{d},\"exec_wake_writes\":{d},\"exec_wake_elided\":{d},", .{ same_lane_wakes.load(), cross_lane_wakes.load(), wake_writes.load(), wake_elided.load() });
    try w.print("\"exec_lane_tick_rescues\":{d},\"exec_lane_backstop_ticks\":{d},\"exec_driven_owner_queued_parks\":{d},", .{ lane_tick_rescues.load(), lane_backstop_ticks.load(), driven_owner_queued_parks.load() });
    try writePerClass(w, "exec_spawn_refused", perClassLoads(counters.ShardedCounter, &spawn_refused));
    try w.writeByte(',');
    try writePerClass(w, "exec_fanout_inline", perClassLoads(counters.ShardedCounter, &fanout_inline));
    try w.print(",\"exec_spawn_trim_waits\":{d}", .{spawn_trim_waits.load()});
    try w.print(",\"exec_timer_arms\":{d},\"exec_timer_fires\":{d},\"exec_timer_thread_tombstones\":{d},", .{ timer_arms.load(), timer_fires.load(), timer_thread_tombstones.load() });
    try w.print("\"exec_blocking_threads\":{d},\"exec_blocking_queue_depth\":{d},\"exec_blocking_spawns\":{d},\"exec_blocking_refused\":{d},\"exec_blocking_reaped\":{d},\"exec_ring_ops\":{d},\"exec_ring_dead_held_ops\":{d},", .{ blocking_threads.load(), blocking_queue_depth.load(), blocking_spawns.load(), blocking_refused.load(), blocking_reaped.load(), ring_ops.load(), ring_dead_held_ops.load() });
    try w.print("\"exec_fdwatch_double_await\":{d},\"exec_blocking_op_on_fuse_lane\":{d},\"exec_batch_on_lane\":{d},\"exec_inbox_overflow\":{d},\"exec_fiber_stack_over_half\":{d},\"exec_group_second_awaiter\":{d},", .{ fdwatch_double_await.load(), blocking_op_on_fuse_lane.load(), batch_on_lane.load(), inbox_overflow.load(), fiber_stack_half.load(), group_second_awaiter.load() });
    try w.writeAll("\"exec_lanes\":[");
    for (lanes, 0..) |l, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("{{\"index\":{d},\"class\":\"{t}\",\"alive\":{},\"queue_depth\":{d},\"fibers_live\":{d},\"tick_rescues\":{d}}}", .{ l.index, l.class, l.alive, l.queue_depth, l.fibers_live, l.tick_rescues });
    }
    try w.writeAll("]}");
}

const testing = std.testing;

test "the JSON names every family once and is well-formed for an empty and a populated lane list" {
    var buf: [8192]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    try writeStatsJson(&w, &.{});
    const out = w.buffered();
    try testing.expect(std.mem.startsWith(u8, out, "{\"exec_fibers_live\":{\"fuse\":"));
    try testing.expect(std.mem.endsWith(u8, out, "\"exec_lanes\":[]}"));
    for ([_][]const u8{ "exec_poll_ns", "exec_lane_tick_rescues", "exec_driven_owner_queued_parks", "exec_spawn_refused", "exec_fanout_inline", "exec_timer_arms", "exec_blocking_threads", "exec_batch_on_lane", "exec_fiber_stack_bytes" }) |key| {
        try testing.expectEqual(@as(usize, 1), std.mem.count(u8, out, key));
    }
    var w2: Io.Writer = .fixed(&buf);
    try writeStatsJson(&w2, &.{.{ .index = 3, .class = .meta, .alive = true, .queue_depth = 2, .fibers_live = 5, .tick_rescues = 0 }});
    try testing.expect(std.mem.indexOf(u8, w2.buffered(), "\"exec_lanes\":[{\"index\":3,\"class\":\"meta\",\"alive\":true,\"queue_depth\":2,\"fibers_live\":5,\"tick_rescues\":0}]}") != null);
}

test "PerClass addresses one slot per lane class" {
    var pc: PerClass(counters.Gauge) = .{};
    pc.of(.journal).set(7);
    try testing.expectEqual(@as(u64, 7), pc.items[@backingInt(LaneClass.journal)].load());
    try testing.expectEqual(@as(u64, 0), pc.of(.fuse).load());
}
