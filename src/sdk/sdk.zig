//! THE NATIVE SDK — `libsqueezefs_sdk` (charter §7 "SDK: the same wire as a
//! second client kind; positional-only ops on arena-backed 4 KiB-aligned
//! buffers under a buffer-lease law; versioned over a one-release window;
//! strengthen-only per-open durability", §12 "the SDK exports a C ABI";
//! spec `docs/porting/il_sdk.md`; decision 0003 §F). The library root.
//!
//! Layout:
//!   version.zig             the two-part ABI word, the window law, the
//!                           refusal texts                       (portable)
//!   buffer_lease.zig        the lease word, the buddy allocator, the
//!                           record pool + `buffer_lease_model.zig` (portable)
//!   session.zig             the SDK over `ipc/client.zig`: establish as
//!                           kind SDK, files, the ops, the batch API,
//!                           poison, fork                        (Linux)
//!   abi.zig                 the `export fn sqz_*` surface; every error
//!                           union → a negative errno             (Linux)
//!   include/squeezefs_sdk.h hand-written, pinned by the rails census and
//!                           the C smoke (`tests/sdk/`)
//!
//! Raw `std.os.linux` syscalls only on every product path — the SDK may be
//! linked into a process that also carries the LD_PRELOAD shim, where
//! `std.posix` I/O would resolve to the shim's own interposers.

const builtin = @import("builtin");

pub const version = @import("version.zig");
pub const buffer_lease = @import("buffer_lease.zig");
pub const buffer_lease_model = @import("buffer_lease_model.zig");
pub const session = if (builtin.os.tag == .linux) @import("session.zig") else struct {};
pub const abi = if (builtin.os.tag == .linux) @import("abi.zig") else struct {};

test {
    _ = version;
    _ = buffer_lease;
    _ = buffer_lease_model;
    if (builtin.os.tag == .linux) _ = session;
    if (builtin.os.tag == .linux) _ = abi;
}
