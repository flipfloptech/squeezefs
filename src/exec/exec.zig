//! The executor: `SqzIo`, our own `std.Io` implementation (charter §9;
//! decisions 0001 and 0003 §D; `docs/porting/exec.md` §3). Portable pieces
//! (the delivery word, the timer heap, placement over synthetic topology,
//! the CPU-class ledger, the gauges, the explorer models) compile
//! everywhere; the lane, fiber, ring and thread pieces are Linux only and
//! gated here, never stubbed (AGENTS.md).

const builtin = @import("builtin");
const is_linux = builtin.os.tag == .linux;

// ---- portable ----
pub const task_state = @import("task_state.zig");
pub const task_state_model = @import("task_state_model.zig");
pub const timer_heap = @import("timer_heap.zig");
pub const placement = @import("placement.zig");
pub const cpu_class = @import("cpu_class.zig");
pub const stats = @import("stats.zig");
pub const futex_table_model = @import("futex_table_model.zig");
pub const group_model = @import("group_model.zig");
pub const wake_core_model = @import("wake_core_model.zig");
pub const slot_pool_model = @import("slot_pool_model.zig");
pub const fanout_slack_model = @import("fanout_slack_model.zig");

// ---- Linux only ----
pub const fiber = if (is_linux) @import("fiber.zig") else struct {};
pub const lane = if (is_linux) @import("lane.zig") else struct {};
pub const futex_table = if (is_linux) @import("futex_table.zig") else struct {};
pub const group = if (is_linux) @import("group.zig") else struct {};
pub const blocking = if (is_linux) @import("blocking.zig") else struct {};
pub const fdwatch = if (is_linux) @import("fdwatch.zig") else struct {};
pub const timer_thread = if (is_linux) @import("timer_thread.zig") else struct {};
pub const sqz_io = if (is_linux) @import("sqz_io.zig") else struct {};

pub const TaskState = task_state.TaskState;
pub const LaneClass = stats.LaneClass;
pub const SqzIo = if (is_linux) sqz_io.SqzIo else struct {};
pub const Lane = if (is_linux) lane.Lane else struct {};

test {
    _ = task_state;
    _ = task_state_model;
    _ = timer_heap;
    _ = placement;
    _ = cpu_class;
    _ = stats;
    _ = futex_table_model;
    _ = group_model;
    _ = wake_core_model;
    _ = slot_pool_model;
    _ = fanout_slack_model;
    if (is_linux) {
        _ = fiber;
        _ = lane;
        _ = futex_table;
        _ = group;
        _ = blocking;
        _ = fdwatch;
        _ = timer_thread;
        _ = sqz_io;
    }
}
