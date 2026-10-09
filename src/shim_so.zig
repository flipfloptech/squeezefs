//! The root source file of `libsqueezefs_il.so` (charter §12). Zig roots a
//! module at its root file's directory, and the shim imports the client
//! library through `src/ipc/`, so the root sits at `src/` and delegates to
//! `shim/shim.zig`; it adds the two things only a root carries: the build
//! identity (the daemon's `build_options.git_commit`, so the daemon and shim
//! refuse to pair across builds — charter §11) and the panic/log hooks.
//! Built by build.zig's explicit `zig build-lib` step (SONAME +
//! `-z nodelete`), never imported by the daemon.

const std = @import("std");
const shim = @import("shim/shim.zig");

/// Read by `shim/lifecycle.zig` through `@import("root")`; its presence is
/// also what turns the exports on.
pub const squeezefs_il_build_commit: []const u8 = @import("build_options").git_commit;

pub const panic = shim.panic;
pub const std_options = shim.std_options;

comptime {
    _ = shim.interpose;
}
