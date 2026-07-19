//! `libsqueezefs_il.so` — the L4 LD_PRELOAD POSIX-interception shim
//! (`docs/design-preload-interception.md` §5.4, PR L4-5).
//!
//! Loaded into **arbitrary host applications**: no tokio, no allocator
//! replacement, no panics across `extern "C"` (every interposer runs
//! under `catch_unwind`; a panic poisons the session and the real call
//! serves — §5.4 panic discipline). Data on the ring for bound fds;
//! everything else — and every bail-out — is the real libc call
//! (§5.4.2 fallback-is-correctness).

// §5.4 Issue-4 (normative): the root workspace's `release` profile
// carries `panic = "abort"`, which would make every `catch_unwind` here
// a no-op and turn any shim panic into a HOST-APPLICATION abort. The
// only sanctioned build is `--profile preload-release` (panic=unwind);
// a wrong-profile build must be unrepresentable, not discouraged.
#[cfg(panic = "abort")]
compile_error!(
    "squeezefs-preload must be built with --profile preload-release (panic = \"unwind\"); \
     the root release profile's panic=\"abort\" would void catch_unwind and abort host apps"
);

pub mod bailout;
pub mod dev_cache;
pub mod fd_table;
pub mod session;
