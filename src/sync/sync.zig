//! The charter's synchronisation primitives (§9), written over `std.Io`'s
//! futex ops so they work under any `Io` (`docs/decisions/0001` item 3).
//! Every primitive takes `io: std.Io` where it parks and allocates nothing
//! per operation (waiter nodes live on the caller's frame; the bounded
//! tables are inline). `tick.zig` holds the 2 s ticked-park law they share.
//!
//! The lane-class pieces take no `Io` (decision 0001 §4): `futex.zig` is the
//! ONE futex wrapper (both faces), `mpmc.zig` the ONE bounded MPMC queue and
//! `completion.zig` the ONE completion cell a caller parks on while a
//! ring-owning lane runs its request.

pub const futex = @import("futex.zig");
pub const mpmc = @import("mpmc.zig");
pub const completion = @import("completion.zig");
pub const tick = @import("tick.zig");
pub const rwlock = @import("rwlock.zig");
pub const mutex = @import("mutex.zig");
pub const notify = @import("notify.zig");
pub const semaphore = @import("semaphore.zig");
pub const flight = @import("flight.zig");
pub const once = @import("once.zig");
pub const channel = @import("channel.zig");
pub const taskset = @import("taskset.zig");
pub const arc_swap = @import("arc_swap.zig");
pub const lock_order = @import("lock_order.zig");

pub const RwLock = rwlock.RwLock;
pub const Mutex = mutex.Mutex;
pub const Notify = notify.Notify;
pub const Semaphore = semaphore.Semaphore;
pub const SingleFlight = flight.SingleFlight;
pub const OnceCell = once.OnceCell;
pub const Oneshot = channel.Oneshot;
pub const Mpsc = channel.Mpsc;
pub const Watch = channel.Watch;
pub const TaskSet = taskset.TaskSet;
pub const BoundedMpmc = mpmc.BoundedMpmc;
pub const Completion = completion.Completion;

test {
    _ = futex;
    _ = mpmc;
    _ = completion;
    _ = tick;
    _ = rwlock;
    _ = @import("rwlock_model.zig");
    _ = mutex;
    _ = notify;
    _ = @import("notify_model.zig");
    _ = semaphore;
    _ = flight;
    _ = once;
    _ = channel;
    _ = taskset;
    _ = arc_swap;
    _ = lock_order;
}
