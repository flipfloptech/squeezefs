//! The two-executor test law (`docs/porting/exec.md` §3.10): every
//! primitive test body in `src/sync/` is a `fn (io: Io) !void` that runs
//! under `std.testing.io` (`Io.Threaded`, every host) AND under
//! `SqzIo.testInstance()` (our executor: 2 meta lanes, Linux only) — one
//! contract, two executors. A body that needs a unit of concurrency skips
//! (`testkit.skip(.no_concurrency)`) where the executor refuses one.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;

comptime {
    std.debug.assert(builtin.is_test);
}

const sqz_io = if (builtin.os.tag == .linux) @import("../exec/sqz_io.zig") else struct {};

pub const Executor = enum { threaded, sqz };

/// Run `body` under both executors. Under `SqzIo` the body's caller is the
/// test thread (a raw thread: `futexWait` takes the counted kernel path) and
/// every `io.concurrent` is a fiber on a meta lane.
pub fn underBoth(comptime body: fn (Io) anyerror!void) !void {
    try body(std.testing.io);
    if (builtin.os.tag == .linux) {
        const sqz = try sqz_io.SqzIo.testInstance();
        defer sqz.deinit();
        try body(sqz.io());
    }
}
