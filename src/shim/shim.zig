//! THE LD_PRELOAD SHIM `libsqueezefs_il.so` (charter §7, §12; spec
//! `docs/porting/il_shim.md`; decision 0003 §F): a C-ABI shared library
//! that interposes the POSIX and libaio data ops on fds bound to a
//! SqueezeFS mount and serves them over the shared-memory session the ONE
//! client library (`ipc/client.zig`) speaks; everything else — and every
//! failure — is literally the real call.
//!
//! Modules: `sys` (raw syscalls + local declarations), `fd_table` (the
//! bound-fd table, the offset mirror, the refcounts, the negative device
//! cache, the MAP_SHARED inode set) + its explorer models, `classify` (the
//! ladder, the session registry, the refusal table, the engagement
//! ledger), `ops` (the interposer bodies), `aio` (libaio), `lifecycle`
//! (lazy init, the guard, atfork, the panic handler, linked mode),
//! `interpose` (the ONE symbol table and the exported wrappers).
//!
//! Two faces of the same objects: the `.so` (root `src/shim_so.zig`, built
//! by the explicit `zig build-lib` step with SONAME + `-z nodelete`,
//! exporting exactly the table), and linked mode inside the daemon's test
//! binary, where the bodies run against the in-process stub host with no
//! export in play. This file is the shim's library root in both.
//!
//! This module is Linux-only (raw `std.os.linux`, glibc's loader); the
//! library root gates it so the dev box compiles nothing of it.

const std = @import("std");
const builtin = @import("builtin");

pub const sys = @import("sys.zig");
pub const fd_table = @import("fd_table.zig");
pub const fd_table_model = @import("fd_table_model.zig");
pub const classify = @import("classify.zig");
pub const ops = @import("ops.zig");
pub const aio = @import("aio.zig");
pub const lifecycle = @import("lifecycle.zig");
pub const interpose = @import("interpose.zig");
pub const e2e = @import("e2e.zig");
pub const smoke = @import("smoke.zig");

/// The `.so` root re-exports these: the panic handler (decision 0003 §F)
/// and a log function that writes raw (`std.log`'s default would reach the
/// shim's own exported `write`).
pub const panic = std.debug.FullPanic(lifecycle.shimPanic);

pub fn logFn(comptime level: std.log.Level, comptime scope: @EnumLiteral(), comptime format: []const u8, args: anytype) void {
    var buf: [512]u8 = undefined;
    const s = std.fmt.bufPrint(&buf, "squeezefs-il " ++ @tagName(scope) ++ " (" ++ level.asText() ++ "): " ++ format ++ "\n", args) catch return;
    sys.writeStderr(s);
}

pub const std_options: std.Options = .{
    .logFn = logFn,
    // The shim is quiet by default: knob diagnostics and refusals only.
    .log_level = .warn,
};

comptime {
    std.debug.assert(builtin.os.tag == .linux);
}

test {
    _ = sys;
    _ = fd_table;
    _ = fd_table_model;
    _ = classify;
    _ = ops;
    _ = aio;
    _ = lifecycle;
    _ = interpose;
    _ = e2e;
    _ = smoke;
}
