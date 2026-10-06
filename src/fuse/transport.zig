//! The FUSE-over-io_uring transport (charter §4; `docs/porting/
//! fuse_uring_transport.md`): the slot state machine with its payload-lease
//! commit gate, the per-worker commit-cell queue, the drain-group workers,
//! the session (mount, the ordered INIT bring-up, the classical sideband,
//! notify, teardown) and the `Filesystem` vtable the transport calls.
//!
//! Portable pieces (the slot machine, the gate and its explorer models, the
//! commit cells, the vtable) compile everywhere; the ring, the workers and
//! the session are Linux only and gated here, never stubbed (AGENTS.md).

const builtin = @import("builtin");
const is_linux = builtin.os.tag == .linux;

// ---- portable ----
pub const slot = @import("transport/slot.zig");
pub const slot_model = @import("transport/slot_model.zig");
pub const commit_queue = @import("transport/commit_queue.zig");
pub const filesystem_vtable = @import("transport/filesystem_vtable.zig");

// ---- Linux only ----
pub const worker = if (is_linux) @import("transport/worker.zig") else struct {};
pub const session = if (is_linux) @import("transport/session.zig") else struct {};

pub const SlotRef = slot.SlotRef;
pub const PayloadLease = slot.PayloadLease;
pub const Filesystem = filesystem_vtable.Filesystem;
pub const ReplySlot = filesystem_vtable.ReplySlot;
pub const Handle = filesystem_vtable.Handle;
pub const Session = if (is_linux) session.Session else struct {};

test {
    _ = slot;
    _ = slot_model;
    _ = commit_queue;
    _ = filesystem_vtable;
    if (is_linux) {
        _ = worker;
        _ = session;
    }
}
