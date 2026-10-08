//! Dynamic metadata routing (charter §5.4; `docs/porting/dynamic_routing.md`):
//! the slot → volume map and the effective-ino law (`route_table`), the
//! order-independent discovery with its refusals (`discovery`), the per-slot
//! cutover gate (`slot_gate`, model-checked in `slot_gate_model`), the delta
//! tee the conveyor pass feeds (`migration_tee`), the per-volume runtime
//! stamp with its guest-cursor cells (`stamp_cell`), the online
//! `migrate-meta-slot` engine (`migrate`, Linux), the offline set verbs
//! (`offline`, Linux) and the instruments (`stats`). The multi-volume
//! facade the filesystem binds to is `meta/meta_set.zig`.

const builtin = @import("builtin");

pub const route_table = @import("routing/route_table.zig");
pub const discovery = @import("routing/discovery.zig");
pub const slot_gate = @import("routing/slot_gate.zig");
pub const slot_gate_model = @import("routing/slot_gate_model.zig");
pub const migration_tee = @import("routing/migration_tee.zig");
pub const stamp_cell = @import("routing/stamp_cell.zig");
pub const slot_state = @import("routing/slot_state.zig");
pub const stats = @import("routing/stats.zig");
pub const migrate = if (builtin.os.tag == .linux) @import("routing/migrate.zig") else struct {};
pub const offline = if (builtin.os.tag == .linux) @import("routing/offline.zig") else struct {};

test {
    _ = route_table;
    _ = discovery;
    _ = slot_gate;
    _ = slot_gate_model;
    _ = migration_tee;
    _ = stamp_cell;
    _ = slot_state;
    _ = stats;
    if (builtin.os.tag == .linux) _ = migrate;
    if (builtin.os.tag == .linux) _ = offline;
}
