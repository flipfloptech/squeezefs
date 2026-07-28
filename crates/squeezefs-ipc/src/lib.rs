//! # squeezefs-ipc — the L4 interception IPC protocol
//!
//! Protocol crate for the LD_PRELOAD POSIX-interception data path
//! (`docs/design-preload-interception.md`): the shared-memory session a
//! preload shim (`squeezefs-preload`, PR L4-5) establishes with the daemon
//! to move data-plane reads/writes past the kernel FUSE transport — a
//! lock-free MPSC submission ring, completion-in-place op slots, a payload
//! arena, and a futex doorbell under the L3 [`wake_core::WakeCoalescer`]
//! elision discipline (0 syscalls/op at saturation on both sides).
//!
//! Shared by three consumers, which dictates the shape:
//!
//! - **daemon + shim** (production): [`layout`] defines the sealed-memfd
//!   session geometry both sides map; [`ring_core`]/[`slot_core`] are the
//!   protocol state machines that live *inside* that mapping.
//! - **loom-models**: `ring_core.rs`, `slot_core.rs` (and the shared
//!   `wake_core.rs`) are `#[path]`-included by `loom-models/src/lib.rs`
//!   and exhaustively model-checked — the cores are dependency-free and
//!   `#[cfg(loom)]`-aware for exactly this reason (the `wake_core` house
//!   convention).
//!
//! Nothing here does I/O or syscalls: session establishment (memfd,
//! `SCM_RIGHTS`, futex) belongs to the daemon host / shim / rig binaries.
//! The protocol is same-host, same-boot, and version-locked to the build
//! commit (design KD-7) — explicitly **not** a stable ABI.

pub mod cqe_core;
pub mod layout;
pub mod ring_core;
pub mod sizing;
pub mod slot_core;
pub mod wire;

// Production source-sharing of the shipped L3 wake-coalescing protocol
// core (design-preload-interception §5.3.2, Issue-18 direction pinned):
// `squeezefs-ipc` points at fuse3's file — never the inverse, so the
// shipped, standalone-tested fuse3 fork never reaches outside its own
// tree. This couples our build to fuse3's file layout (accepted), and
// gives the build two distinct `WakeCoalescer` *type identities* (one per
// including crate) — intentional: instances never cross a crate boundary,
// and a future refactor that tries to share one fails to compile rather
// than silently alias.
#[path = "../../fuse3/src/raw/connection/wake_core.rs"]
pub mod wake_core;
