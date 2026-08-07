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

pub mod aio_core;
pub mod aio_glue;
pub mod bailout;
pub mod dev_cache;
pub mod fd_table;
// The fd table's lock-free protocol core (spec §11 TEST-5): extracted per
// the house `#[path]`-shared-core convention so `loom-models/` checks the
// SHIPPED refcount law, PERF-7 Dekker pair, and fork-epoch predicate —
// not a copy.
pub mod fd_table_core;
#[cfg(feature = "interposers")]
pub mod interpose;
pub mod lane_gate;
pub mod mapped_inos;
pub mod session;
// Session-arena THP helper — canonical file in the squeezefs-ipc tree,
// `#[path]`-included here and by the root crate (the `wake_core`
// production-sharing precedent: the ipc LIBRARY stays dependency-free,
// both consumers link libc). Instances never cross the crate boundary.
#[path = "../../squeezefs-ipc/src/thp.rs"]
pub mod thp;
// The ONE env-knob parsing convention (ENG-10) — canonical file in the
// squeezefs-ipc tree, `#[path]`-included here, by the root crate and by the
// fuse3 fork, so the shim's client-side knobs mean exactly what the
// daemon's mean (`SQUEEZEFS_IPC_ALLOW_DEV=0` disables on BOTH ends). The
// shim never refuses its host application over a bad value — it announces
// and keeps the documented default (§ENG-10's documented asymmetry).
#[path = "../../squeezefs-ipc/src/env_knob_core.rs"]
pub mod env_knob_core;

/// This shim's build identity (`<full-hash>[-dirty]`, `src/version.rs`
/// form) — the KD-7 skew-gate key, compared against the daemon's
/// bootstrap blob at establish.
pub const BUILD_COMMIT: &str = env!("SQUEEZEFS_IL_BUILD_COMMIT");
