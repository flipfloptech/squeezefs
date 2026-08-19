//! **The indirect-map I/O hook** — DLM S11 rung 20 residual 1 (the
//! blob-aware owner-side merge).
//!
//! A shared file whose composed layout map exceeds the inline cap spills
//! to an `indirect:` blob — a DATA-plane block holding the encoded map
//! ([`crate::routing::encode_indirect_block_map`]). The meta backend owns
//! no data-plane router, so the three owner-side composition sites (the
//! aggregated conveyor member, the direct chained merge, the
//! custody-scoped Put) historically REFUSED any chained/scoped publish
//! that met an indirect head — fail-safe but never CONVERGING (the
//! retried-class "layout delta base unusable: indirect base" became
//! terminal fsync EIO at 10 GiB scale, the s11-mpiio row's conviction).
//!
//! When the multi-writer authority is armed it HAS a data router, and
//! this hook is how the meta plane borrows it — the exact
//! [`super::block_refs::install_block_ref_resolver`] pattern: a
//! process-global `ArcSwapOption`, installed at
//! `multi_writer::arm_multi_writer` beside the refs resolver, uninstalled
//! at disarm, ONE relaxed load on every un-armed mount. Unarmed mounts
//! keep the refusal verbatim.
//!
//! The `write` half carries the **DUR-6** laws with it: the blob is
//! copy-on-write (a FRESH block per compose; the predecessor is freed
//! only after the naming commit stopped referencing it) and the device
//! write is FLUSHED before the closure returns — the naming commit must
//! never point at bytes that can vanish on power loss (§3's barrier).

use crate::error::SqueezefsError;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Rehydrate a map-blob key into its decoded entries (the router's
/// `read_block` + [`crate::routing::decode_indirect_block_map`]).
pub type IndirectMapReadFn = Arc<
    dyn Fn(
            String,
        )
            -> Pin<Box<dyn Future<Output = Result<Vec<(u32, String)>, SqueezefsError>> + Send>>
        + Send
        + Sync,
>;

/// Encode + allocate + write + **flush** a fresh CoW map blob. The `u64`
/// is the owning ino (error text only). Returns the fresh block key and
/// the RES-9 guard covering it until the naming commit lands.
pub type IndirectMapWriteFn = Arc<
    dyn Fn(
            u64,
            Vec<(u32, String)>,
        ) -> Pin<
            Box<dyn Future<Output = Result<(String, IndirectBlobGuard), SqueezefsError>> + Send>,
        > + Send
        + Sync,
>;

/// Best-effort device free of a DISPLACED blob (called strictly AFTER
/// the commit that stopped naming it). Failures are logged inside the
/// closure, never propagated — the durable truth is already the ledger's.
pub type IndirectMapFreeFn =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// The three data-plane closures the owner-side compose borrows.
pub struct IndirectMapIo {
    /// Rehydrate a blob key into its map entries.
    pub read: IndirectMapReadFn,
    /// Mint + write + flush a fresh CoW blob for a composed map.
    pub write: IndirectMapWriteFn,
    /// Free a displaced blob's device block (post-commit only).
    pub free: IndirectMapFreeFn,
}

/// RES-9 mint guard for a freshly written map blob: any exit between the
/// blob write and the naming commit frees the minted block instead of
/// leaking an allocated-and-unnamed offset only fsck could find.
/// [`Self::disarm`] exactly when custody transfers (the commit landed).
/// Composed of the SAME two guards the router's own spill arm holds —
/// never a re-implementation of their logic.
pub struct IndirectBlobGuard {
    /// PR VL6a: the blob stays registered in-flight until the layout
    /// commit publishes the record naming it (drops with the guard).
    _inflight: Option<crate::block_allocator::InflightAllocGuard>,
    /// DUR-6/RES-9: the mint guard — its `Drop` frees the block unless
    /// disarmed.
    minted: Option<crate::assembly_tasks::MintedBlockGuard>,
}

impl IndirectBlobGuard {
    /// The production guard (the router's spill-arm pair). `pub(crate)`
    /// because both constituents are crate-internal machinery; the
    /// public face for test hooks is [`Self::unguarded`].
    pub(crate) fn new(
        inflight: crate::block_allocator::InflightAllocGuard,
        minted: crate::assembly_tasks::MintedBlockGuard,
    ) -> Self {
        Self {
            _inflight: Some(inflight),
            minted: Some(minted),
        }
    }

    /// A guard with no device-side machinery — test hooks whose blobs
    /// live in memory.
    pub fn unguarded() -> Self {
        Self {
            _inflight: None,
            minted: None,
        }
    }

    /// Custody transferred: the naming commit landed, the blob is
    /// referenced — stand the mint guard down.
    pub fn disarm(&mut self) {
        if let Some(m) = self.minted.as_mut() {
            m.disarm();
        }
    }
}

/// Resolve + push ONE map-blob custody-transfer op (the
/// [`super::block_refs::BLOCK_INDEX_MAP_BLOB`] sentinel) — the compose
/// arms' released(old)/taken(fresh) pair rides this, in both the KV
/// backend (sites a/b) and the custody-scoped Put (site c).
/// Unresolvable keys are counted, never silent — the router's
/// `block_ref_for` discipline verbatim.
pub(crate) fn push_map_blob_transfer_op(
    out: &mut Vec<super::block_refs::BlockRefOp>,
    key: &str,
    ino: u64,
    take: bool,
) {
    use super::block_refs::BlockRefOp;
    let Some(resolver) = super::block_refs::block_ref_resolver() else {
        return;
    };
    match resolver(key, ino, super::block_refs::BLOCK_INDEX_MAP_BLOB) {
        Some(r) => out.push(if take {
            BlockRefOp::taken(r)
        } else {
            BlockRefOp::released(r)
        }),
        None => {
            super::META_KV_BLOCK_REFS_UNRESOLVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

static INDIRECT_MAP_IO: once_cell::sync::Lazy<arc_swap::ArcSwapOption<IndirectMapIo>> =
    once_cell::sync::Lazy::new(arc_swap::ArcSwapOption::empty);

/// Install the hook (multi-writer arm / test fixture).
pub fn install_indirect_map_io(io: IndirectMapIo) {
    INDIRECT_MAP_IO.store(Some(Arc::new(io)));
}

/// Uninstall it (disarm / test teardown).
pub fn uninstall_indirect_map_io() {
    INDIRECT_MAP_IO.store(None);
}

/// The installed hook, if any — one relaxed load on every un-armed mount.
pub fn indirect_map_io() -> Option<Arc<IndirectMapIo>> {
    INDIRECT_MAP_IO.load_full()
}
