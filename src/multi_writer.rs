//! DLM **stage S9** — the **multi-writer mount arm**: the one opt-in path
//! that arms metadata ownership (S8), the data-plane custody fence and its
//! device-enforced WERO hold (S7), and the remote write-custody plane
//! ([`crate::data_grant`]) as a coherent whole — or refuses loudly, naming
//! the piece that is missing.
//!
//! # Why the arm is one function
//!
//! S8 declined to arm ownership and said why: *"a mount that ships metadata
//! but cannot ship data custody is not a product."* The converse is equally
//! true — a mount that grants remote data custody but publishes layouts
//! locally would append to a tree whose journal ring, extent bitmap and
//! root ledger belong to another writer. The three planes are only sound
//! **together**, so there is exactly one place that turns them on, and its
//! refusal ladder is the product's honest answer about what multi-writer
//! requires.
//!
//! # The ladder, in order, and why each rung is where it is
//!
//! 1. **A reader is refused.** `-o ro` is S5's posture: a reader takes no
//!    lease, writes no claim, holds no PR key. Demanding a multi-writer
//!    DATA plane on it is a category error, not a degradation.
//! 2. **The format's capability bits** — cheap, side-effect-free, and it
//!    must come before anything is acquired. [`REQUIRED_INCOMPAT`] is the
//!    set whose absence would make a second writer *unsound*, and the
//!    refusal names the first missing bit **and its offline stamping
//!    path**, because ruling **D9** means nothing stamps them today.
//! 3. **The substrate** (§6.9's S9 guarantee: *"full multi-writer on PR
//!    substrates; **refused on non-PR**"*): [`crate::data_custody`]'s own
//!    arm takes the WERO (rtype 3) hold, joining the standing one rather
//!    than forking a second key, and refuses a namespace that advertises no
//!    reservation support — naming it.
//! 4. **The membership plane**, because a co-writer that cannot be *seen*
//!    cannot be *evicted*: S6's eviction is what mints the dead epoch S7's
//!    quarantine is keyed on, and its census is what a `squeezefs clients`
//!    audit reads. Checked after the hold, so a refusal releases it.
//! 5. **A durable writer era** (`dlm_term > 0`). Bit 7 is in the required
//!    set, so the D0 gate has published one by the time we get here; a zero
//!    era would mean every custody epoch is a constant, i.e. the fence
//!    could not express a revocation at all.
//! 6. **A bind address** for the custody + publish authority. A
//!    multi-writer mount that serves neither is inert, so `off` is a
//!    refusal rather than a silent no-op.
//!
//! # What the arm CANNOT do yet in the field, stated plainly
//!
//! Two things stand between this code and a live two-host cluster, and
//! neither is S9's to fix quietly:
//!
//! * **Nothing stamps the capability bits** (ruling D9). Every volume in
//!   the field fails rung 2. The Phase-8 batched reformat window is where
//!   they land; the offline `set_*_bit` paths named in the refusal are what
//!   it calls, and `tests/dlm_multi_writer_tests.rs` calls them too.
//! * **The D0 Layer-B2 gate refuses a fresh foreign claim on every
//!   substrate** (`kv/backend.rs`'s `writer_guard_gate`), unconditionally
//!   and regardless of the claim-set bit. So a second *write* mount of one
//!   volume set cannot open the metadata volumes at all, and the co-writer
//!   posture this module's client half implements is unreachable from
//!   `main` until that gate learns to admit a co-member of an ENGAGED claim
//!   set (§6.2 item 7's consumer half). That change belongs with the
//!   stamped bit and a PR-capable fleet, not with a mechanism landing.
//!
//! What the arm therefore delivers today is the **authority** half in
//! production shape — a mount that can grant remote write custody and serve
//! the publish path for peers — plus a fully wired client half that the
//! suite exercises against it. The gap is named, not papered over.

use crate::data_custody::{self, CustodyPosture, WeroHold};
use crate::data_grant::{self, AsyncVerbRouter, CustodyQuarantine, WriteCustodyOwner};
use crate::error::{Result, SqueezefsError};
use crate::membership::{LeaseClock, LeaseClocks};
use crate::meta_backend::kv::superblock as sb;
use crate::meta_backend::RoutedMetaBackend;
use crate::meta_ship::{publish, OwnerMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Where this mount serves the custody + publish authority
/// (`SQUEEZEFS_MW_BIND`).
pub const MW_BIND_ENV: &str = "SQUEEZEFS_MW_BIND";

/// The `features_incompat` bits a multi-writer mount REQUIRES, and the
/// reason each one is not optional:
///
/// | Bit | Capability | Why a second writer is unsound without it |
/// |---|---|---|
/// | 7 | durable writer term | a custody epoch that restarts at every mount cannot express a revocation, and every fencing token would be era-less (§6.7 decision 4) |
/// | 9 | durable block refcounts | §6.2 item 1, verbatim: *"Without durable shared ownership accounting, no multi-writer data path is expressible"* — two writers each derive a private answer from the subtree they walked |
/// | 10 | writer-scoped staging | §6.2 items 8/10: unlabelled `active_block:`/`mapping:` keys and a node-blind staging root let one node adopt or wipe a peer's staged payloads |
/// | 11 | multi-writer data | S7's capability gate: the format's recovery paths are expressed for more than one data-plane writer |
/// | 13 | `offset ‖ incarnation` block keys | §6.3's first coherence obligation: with bare reusable offsets a stale binding is structurally UNDETECTABLE, so a peer serves another file's bytes with no error and no counter |
/// | 14 | the claim-set record | §6.2 item 7: `writer_claim` is singular — it expresses exclusion, not partition membership, and the registrant keys a preempt (i.e. a drain proof) needs live in the set |
///
/// **Deliberately NOT required**: bit 8 (partitioned append) and bit 12
/// (per-writer ino lanes). Both express **two appenders on ONE volume**,
/// and S9's ownership granularity is the VOLUME (spec §6.10 R4) — so every
/// §6.2 single-appender structure still has exactly one appender, exactly
/// as S8 argued when it stamped no bit at all. Requiring them would demand
/// a format change for a shape this stage never produces.
///
/// **S9 takes no new bit.** Every capability it gates on was already built
/// by the §6.2 format work; this program has had four parallel bit
/// collisions, and not taking a fifth is the safest available answer.
pub const REQUIRED_INCOMPAT: u64 = sb::FEATURE_INCOMPAT_KV_DURABLE_TERM
    | sb::FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS
    | sb::FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING
    | sb::FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA
    | sb::FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION
    | sb::FEATURE_INCOMPAT_KV_CLAIM_SET;

/// The offline stamping verb for one required bit — what the refusal names,
/// so an operator learns what the Phase-8 reformat window owes rather than
/// only that something is missing.
fn stamping_path(bit: u64) -> &'static str {
    match bit {
        b if b == sb::FEATURE_INCOMPAT_KV_DURABLE_TERM => "superblock::set_durable_term_bit",
        b if b == sb::FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS => "superblock::set_block_refcounts_bit",
        b if b == sb::FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING => {
            "superblock::set_writer_scoped_staging_bit"
        }
        b if b == sb::FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA => {
            "superblock::set_multi_writer_data_bit"
        }
        b if b == sb::FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION => {
            "superblock::set_block_key_incarnation_bit"
        }
        b if b == sb::FEATURE_INCOMPAT_KV_CLAIM_SET => "superblock::set_claim_set_bit",
        _ => "the Phase-8 reformat window",
    }
}

fn bit_name(bit: u64) -> &'static str {
    match bit {
        b if b == sb::FEATURE_INCOMPAT_KV_DURABLE_TERM => "durable writer term",
        b if b == sb::FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS => "durable block refcounts",
        b if b == sb::FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING => "writer-scoped staging",
        b if b == sb::FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA => "multi-writer data plane",
        b if b == sb::FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION => {
            "offset-with-incarnation block keys"
        }
        b if b == sb::FEATURE_INCOMPAT_KV_CLAIM_SET => "the claim-set record",
        _ => "an unnamed capability",
    }
}

/// The mount's quarantine sink: a dead co-writer's declared destinations,
/// admitted to S7's do-not-reallocate cohort on this mount's data volumes.
///
/// **The predicate is capacity, not the refcount map, and the reason is the
/// honest shape of this stage.** A co-writer allocates from ITS OWN
/// process's [`crate::block_allocator::BlockAllocator`], so the authority's
/// refcount map has no entry for the offset at all — asking it would answer
/// "unknown" and quarantine nothing. Admitting on every data volume whose
/// capacity contains the offset over-quarantines at most one offset per
/// volume until the drain proof arrives, which is bounded, visible
/// (`dlm_quarantined_offsets`) and safe; under-quarantining would hand a
/// possibly-live zombie's offset to a new owner, which is silent corruption.
///
/// **The unambiguous form is `(volume, offset)`** and it is a named
/// residual: the wire would carry the data volume's durable `vol-{hex}` id
/// beside each offset, exactly as `TREE_BLOCK_REFS` keys do (KD-5).
pub struct RouterQuarantine {
    router: Arc<crate::routing::BackendRouter>,
}

impl std::fmt::Debug for RouterQuarantine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouterQuarantine")
            .field("data_volumes", &self.router.backends.len())
            .finish()
    }
}

impl RouterQuarantine {
    /// A sink over every data volume this mount routes to.
    pub fn new(router: Arc<crate::routing::BackendRouter>) -> Arc<Self> {
        Arc::new(Self { router })
    }
}

impl CustodyQuarantine for RouterQuarantine {
    fn quarantine(&self, offsets: &[u64], epoch: crate::data_custody::DeadEpoch) -> usize {
        let mut admitted = 0;
        for entry in self.router.backends.iter() {
            let alloc = &entry.value().block_allocator;
            let cap = alloc.capacity_bytes();
            for &offset in offsets {
                if cap == 0 || offset < cap {
                    admitted += usize::from(alloc.quarantine_offset(offset, epoch));
                }
            }
        }
        admitted
    }

    fn release(&self, epoch: crate::data_custody::DeadEpoch) -> usize {
        self.router
            .backends
            .iter()
            .map(|entry| entry.value().block_allocator.release_quarantine(epoch))
            .sum()
    }
}

/// Rung 18 (the zeros-interleave conviction): the **PRODUCTION §9.2
/// range-geometry source** — the answer `custody_scoped_layout` scopes a
/// range holder's full Put with, and the per-file span cap + §9.3
/// demotion-barrier block walls at every ranged acquire. `(size, block)`
/// from the authority's OWN planes: size = the ino's durable layout
/// head's (0 when absent/undecodable — the span cap floors at 16 and the
/// scoping arm only consumes `block`); block = the data plane's live
/// block size. Never a constant — the Issue-19 law.
pub fn router_range_geometry(
    meta: Arc<RoutedMetaBackend>,
    backend: Arc<crate::routing::BackendRouter>,
) -> Arc<dyn data_grant::RangeGeometry> {
    struct RouterRangeGeometry {
        meta: Arc<RoutedMetaBackend>,
        backend: Arc<crate::routing::BackendRouter>,
    }
    impl std::fmt::Debug for RouterRangeGeometry {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("RouterRangeGeometry")
                .finish_non_exhaustive()
        }
    }
    impl data_grant::RangeGeometry for RouterRangeGeometry {
        fn geometry(
            &self,
            ino: u64,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<(u64, u64)>> + Send + '_>>
        {
            Box::pin(async move {
                let block = self.backend.block_size.load(Ordering::Relaxed);
                if block == 0 {
                    // A zero block size cannot express block walls — no
                    // answer (the scoped Put then REFUSES rather than
                    // guessing; the acquire runs the byte budget alone).
                    return None;
                }
                use crate::meta_backend::Metadata as _;
                // Rung 18 (the MPI-IO row's live wedge — the Issue-19
                // class at the SOURCE): size is the MAX of the INODE
                // record's (the truncate/setattr truth — the rank-0
                // create-truncate shape has no layout at all) and the
                // layout head's (which can be a CHAINED/indirect head
                // decode_base_layout cannot read — a 10 GiB shared file
                // answered 0, the span cap floored at 16, and the
                // 32-rank decomposition's 17th stripe refused "at
                // capacity"). Growth published in either plane counts;
                // absent-both is honestly 0.
                let inode_size = self
                    .meta
                    .getattr(ino)
                    .await
                    .map(|rec| rec.size)
                    .unwrap_or(0);
                let layout_size = match self.meta.getxattr(ino, "layout").await {
                    Ok(Some(bytes)) => crate::layout_wire::decode_base_layout(&bytes)
                        .map(|l| l.size)
                        .unwrap_or(0),
                    _ => 0,
                };
                Some((inode_size.max(layout_size), block))
            })
        }
    }
    Arc::new(RouterRangeGeometry { meta, backend })
}

/// Rung 20 residual 1: the PRODUCTION indirect-map I/O hook — the
/// authority's data router behind the
/// [`crate::meta_backend::kv::indirect_map`] seam, so the owner-side
/// compose sites can rehydrate/rewrite/free indirect map blobs.
///
/// * `read` — one whole-block device read + the versioned decode
///   ([`crate::routing::decode_indirect_block_map`]), counted on the
///   existing `layout_indirect_map_read{s,_bytes}` gauges.
/// * `write` — the router spill arm's ladder VERBATIM (DUR-6): encode,
///   one-block bound check (LOUD overflow), pad to 4096, allocate on the
///   active backend, in-flight register + RES-9 mint guard, persist key,
///   `write_block`, **`flush()`** (§3 — the blob is durable BEFORE the
///   naming commit), counted on `publish_indirect_blob_bytes`.
/// * `free` — best-effort terminal free of a DISPLACED blob (called
///   strictly post-commit); failure is a warn, never an error — the
///   ledger already released the reference, and `trim --full`/remount
///   derivation reclaims stragglers.
pub fn indirect_map_io_for(
    backend: Arc<crate::routing::BackendRouter>,
) -> crate::meta_backend::kv::indirect_map::IndirectMapIo {
    use crate::meta_backend::kv::indirect_map::{IndirectBlobGuard, IndirectMapIo};
    use std::future::Future;
    use std::pin::Pin;

    let read_router = Arc::clone(&backend);
    let read = Arc::new(
        move |key: String| -> Pin<Box<dyn Future<Output = Result<Vec<(u32, String)>>> + Send>> {
            let router = Arc::clone(&read_router);
            Box::pin(async move {
                let block_size = router.block_size.load(Ordering::Relaxed) as usize;
                let raw = router.read_block(&key, block_size).await?;
                crate::fuse_client::METRICS
                    .layout_indirect_map_reads
                    .fetch_add(1, Ordering::Relaxed);
                crate::fuse_client::METRICS
                    .layout_indirect_map_read_bytes
                    .fetch_add(raw.len() as u64, Ordering::Relaxed);
                crate::routing::decode_indirect_block_map(&raw)
            })
        },
    );

    let write_router = Arc::clone(&backend);
    let write = Arc::new(
        move |ino: u64,
              entries: Vec<(u32, String)>|
              -> Pin<Box<dyn Future<Output = Result<(String, IndirectBlobGuard)>> + Send>> {
            let router = Arc::clone(&write_router);
            Box::pin(async move {
                let map: std::collections::HashMap<u32, String> = entries.into_iter().collect();
                let mut serialized = crate::routing::encode_indirect_block_map(&map)?;
                // The blob lives in ONE allocator block and the fetch path
                // reads exactly one block back — overflow fails LOUD (writing
                // past the block would corrupt the neighboring allocation).
                let block_size = router.block_size.load(Ordering::Relaxed) as usize;
                if serialized.len() > block_size {
                    return Err(SqueezefsError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "rung 20: composed indirect block map for ino {ino} ({} entries, \
                         {} B serialized) exceeds one {block_size} B block",
                            map.len(),
                            serialized.len()
                        ),
                    )));
                }
                let aligned_len = (serialized.len() + 4095) & !4095;
                serialized.resize(aligned_len, 0);
                // Finding 29: the serve-side blob mint rides the bounded
                // form too — an authority mid grace-storm must park for
                // the fence, not fail the served publish.
                let (be_id, block_allocator, nvme_writer, offset) =
                    router.allocate_placed_block().await?;
                let inflight = block_allocator.inflight_register(offset);
                // RES-9: any `?` between here and the caller's naming commit
                // frees the fresh blob instead of leaking an allocated block
                // no map will ever name.
                let minted = crate::assembly_tasks::MintedBlockGuard::new(
                    Arc::clone(&block_allocator),
                    offset,
                );
                let block_key = router.persist_block_key(&be_id, offset);
                let data = bytes::Bytes::from(serialized);
                let blob_len = data.len() as u64;
                nvme_writer.write_block(offset, data).await?;
                // DUR-6 §3 — barrier BEFORE the naming commit: the device
                // write is volatile until its cache is flushed, and a commit
                // naming un-flushed bytes can lose the map on power loss.
                nvme_writer.flush().await?;
                crate::fuse_client::METRICS
                    .publish_indirect_blob_bytes
                    .fetch_add(blob_len, Ordering::Relaxed);
                Ok((block_key, IndirectBlobGuard::new(inflight, minted)))
            })
        },
    );

    let free_router = Arc::clone(&backend);
    let free = Arc::new(
        move |key: String| -> Pin<Box<dyn Future<Output = ()> + Send>> {
            let router = Arc::clone(&free_router);
            Box::pin(async move {
                // The displaced blob is THIS authority's own lifecycle
                // (its compose minted it), so the free is the authority's
                // accounting act wherever the serve runs — the same scope
                // the shipped-free executor enters. Without it a process
                // whose posture latch reads co-writer (a partial authority
                // serving its own volumes; a one-process venue) SHIPPED
                // its own blob's free to the set authority, which refused
                // it on the live-free shield, and the blob leaked.
                let freed =
                    crate::cowriter::with_authority_accounting(router.free_block(&key)).await;
                if let Err(e) = freed {
                    log::warn!(
                        "rung 20: freeing displaced indirect map blob '{key}' failed ({e}) — \
                         the ledger already released it; trim --full / remount derivation \
                         reclaims the block"
                    );
                }
            })
        },
    );

    IndirectMapIo { read, write, free }
}

/// Where the authority binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bind {
    /// Ruling D2's posture: an ephemeral port on every interface.
    Auto,
    /// An explicit address.
    Addr(std::net::SocketAddr),
    /// Explicitly off — a refusal for a multi-writer mount (see the ladder).
    Off,
}

fn resolve_bind() -> Result<Bind> {
    let raw = std::env::var(MW_BIND_ENV).unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("auto") {
        return Ok(Bind::Auto);
    }
    if raw.eq_ignore_ascii_case("off") {
        return Ok(Bind::Off);
    }
    raw.parse::<std::net::SocketAddr>()
        .map(Bind::Addr)
        .map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "{MW_BIND_ENV}='{raw}' is neither `auto`, `off`, nor an `addr:port` ({e}) — \
                 refusing rather than serving write custody somewhere the operator did not ask \
                 for"
            ))
        })
}

/// What a multi-writer mount armed, and the teardown that undoes it.
pub struct MultiWriterArm {
    listener: Option<Arc<crate::cluster_wire::RpcListener>>,
    owner: Arc<WriteCustodyOwner>,
    wero: Option<WeroHold>,
    stop: Arc<AtomicBool>,
    endpoint: String,
}

impl std::fmt::Debug for MultiWriterArm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiWriterArm")
            .field("endpoint", &self.endpoint)
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}

impl MultiWriterArm {
    /// The address peers reach this mount's custody + publish authority on.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The custody authority.
    pub fn owner(&self) -> &Arc<WriteCustodyOwner> {
        &self.owner
    }

    /// Stop serving: end the cadence task, drop the listener, restore solo
    /// ownership, uninstall both custody halves, and release the WERO hold
    /// off the async runtime (its ioctls are blocking).
    pub async fn disarm(mut self) {
        // Stop latch (the D0 heartbeat precedent — sqz-meta tasks are
        // never aborted mid-poll): the sweep loop checks `stop` after
        // every cadence sleep and exits before touching the owner again.
        self.stop.store(true, Ordering::Release);
        if let Some(listener) = self.listener.take() {
            listener.shutdown();
        }
        data_grant::uninstall_custody_client();
        data_grant::uninstall_custody_owner();
        publish::uninstall_client();
        // The client half of the ownership plane dies with it: a stale
        // router over a disarmed plane is inert (the armed load gates
        // first), but leaving one installed would outlive its map.
        crate::meta_ship::uninstall_daemon_verb_router();
        crate::meta_ship::uninstall_delegation_host();
        // Rung 14: the placement policy's vehicle dies with the authority
        // (its runtime state dies inside disarm_ownership).
        crate::meta_ship::placement::uninstall_migration_executor();
        crate::meta_ship::disarm_ownership();
        // The lane map dies with the authority (it is era-scoped), and so does
        // the frontier source a served OPEN read. The allocators keep their
        // engaged lanes: a mount's own residue class is fixed for its life
        // (`install_mount_partition` refuses a swap), and the offsets it has
        // minted are in it.
        crate::alloc_lane_grant::uninstall_frontier_source();
        // The shipped-free EXECUTOR dies with the authority too: a served
        // free with no executor refuses loud rather than stranding half a
        // ladder on a disarmed mount. Same for its reuse half — and for
        // rung 17's extent ASSEMBLER pair (a served extent with no
        // assembler refuses loud rather than acking bytes nobody merges).
        publish::uninstall_free_executor();
        publish::uninstall_harvest_executor();
        publish::uninstall_binding_witness();
        publish::uninstall_released_block_probe();
        crate::free_grace::uninstall_release_hook();
        crate::free_grace::uninstall_lane_supply_source();
        publish::uninstall_extent_merge_executor();
        publish::uninstall_extent_flush_executor();
        publish::uninstall_served_layout_invalidation();
        publish::uninstall_served_displacement_sink();
        // Rung 19: the refs resolver dies with the authority (it holds
        // the data router; a disarmed mount serves no composed commits).
        crate::meta_backend::kv::block_refs::uninstall_block_ref_resolver();
        // Rung 20: the indirect-map I/O hook dies with it too (same
        // router; a disarmed mount's indirect heads go back to the
        // fail-safe refusal).
        crate::meta_backend::kv::indirect_map::uninstall_indirect_map_io();
        if let Some(hold) = self.wero.take() {
            data_custody::release_hold(hold).await;
        }
        log::warn!(
            "multi-writer DISARMED: solo authority restored, no custody is granted to peers, \
             and the data-namespace WERO hold is released"
        );
    }
}

/// `true` ⇔ this mount was asked to arm the multi-writer planes
/// (`SQUEEZEFS_MULTI_WRITER=1` — the same knob S7 introduced, whose meaning
/// S9 completes).
pub fn requested() -> bool {
    data_custody::multi_writer_requested()
}

/// Mount-path entry point: arm the multi-writer planes when the operator
/// asked for them, else `Ok(None)` — the shipped single-writer posture,
/// after one env read.
pub async fn arm_mount_multi_writer(
    meta: &Arc<RoutedMetaBackend>,
    data_paths: &[PathBuf],
    read_only: bool,
    quarantine: Option<Arc<dyn CustodyQuarantine>>,
    backend: Option<&Arc<crate::routing::BackendRouter>>,
    pv: Option<&crate::partial_authority::SetAdmission>,
) -> Result<Option<MultiWriterArm>> {
    if !requested() {
        log::debug!(
            "multi-writer not requested (SQUEEZEFS_MULTI_WRITER is off — the shipped posture: \
             the D0 guard arbitrates and the data plane is fenced locally by the custody epoch)"
        );
        // Rung 12, design §11: `SQUEEZEFS_DELEGATION` set on an unarmed
        // mount is ANNOUNCED-INERT — a startup notice, never a refusal
        // (the SQUEEZEFS_MW_ROLE "read only when armed" precedent).
        if std::env::var(crate::meta_ship::DELEGATION_ENV)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            log::info!(
                "SQUEEZEFS_DELEGATION is set but no multi-writer plane is armed on this mount — \
                 the S10 delegation lever is INERT here (every dlm_delegation gauge stays 0 by \
                 construction); it engages only on mounts whose ownership plane is armed"
            );
        }
        return Ok(None);
    }
    // DLM S9: this is the AUTHORITY's arm. A mount that declared
    // `SQUEEZEFS_MW_ROLE=co-writer` owns the other half of the posture —
    // `cowriter::arm`, whose ladder already decided it is admissible — and
    // running this one there would try to ACQUIRE a reservation the
    // authority holds and to serve custody this mount has no claim to
    // grant.
    if crate::cowriter::requested_role() == crate::cowriter::MwRole::CoWriter {
        log::info!(
            "multi-writer: this mount is a CO-WRITER (SQUEEZEFS_MW_ROLE=co-writer), so the \
             AUTHORITY arm is not run — the co-writer arm installs the client halves of the \
             same three planes (ownership + publish + custody) after its admission ladder \
             passes"
        );
        return Ok(None);
    }
    // Per-volume claim admission, PR 7b: the same split one grain finer. A
    // PARTIAL AUTHORITY appends to a subset and ships the rest, so under
    // D20 it must take none of the set-singular planes this arm takes —
    // its own arm ([`arm_partial_authority`]) composes the co-writer
    // client halves with an owner half over the volumes it appends to.
    // Reaching this arm anyway is refused inside it (the exclusive-door
    // law), and routing here keeps the refusal for the shape that means
    // it: a mount whose DERIVATION disagrees with its declaration.
    if crate::cowriter::requested_role() == crate::cowriter::MwRole::PartialAuthority {
        log::info!(
            "multi-writer: this mount declared PARTIAL AUTHORITY \
             (SQUEEZEFS_MW_ROLE=partial-authority), so the SET authority's arm is not run — the \
             partial arm installs the client halves toward the set authority plus an owner half \
             serving only the volumes this node appends to (D20, §5.7)"
        );
        return Ok(None);
    }
    arm_multi_writer(meta, data_paths, read_only, quarantine, backend, pv).await
}

/// The authority's release-on-ack hook over its data router (the
/// lane-push lever's authority half — [`crate::free_grace::ReleaseHook`]):
/// every allocator's grace ring harvested to its uncovered front. RAM
/// only; installed beside the harvest executor.
pub fn release_hook(backend: Arc<crate::routing::BackendRouter>) -> crate::free_grace::ReleaseHook {
    Arc::new(move || {
        for alloc in backend.lane_allocators() {
            alloc.harvest_grace_to_front();
        }
    })
}

/// The authority's per-member lane-supply source (the lever's wire half —
/// [`crate::free_grace::LaneSupplySource`]): a member id → its lane under
/// this era's assignment → PER DATA VOLUME, `(vol_tag, that lane's
/// free-listed population on the volume)` off the free set's per-lane
/// counters (finding 15's fpp residue: the grant names each volume's
/// share, so the co-writer's per-volume decline and pushed refill read
/// their own volume). A member with no lane (a reader, an unknown id)
/// reads empty.
pub fn lane_supply_source(
    backend: Arc<crate::routing::BackendRouter>,
    assignment: Arc<crate::alloc_lane_grant::LaneAssignment>,
) -> crate::free_grace::LaneSupplySource {
    Arc::new(move |member_id: &str| {
        let Some(lane) = assignment.lane_of(member_id) else {
            return Vec::new();
        };
        // `lane_allocators` names each allocator once (the default slot's
        // alias of the first registered backend is deduplicated there).
        backend
            .lane_allocators()
            .iter()
            .map(|alloc| {
                (
                    crate::meta_backend::kv::block_refs::volume_tag(alloc.volume_id()),
                    alloc.lane_free_count(lane),
                )
            })
            .collect()
    })
}

/// **Arm the multi-writer planes, or refuse naming what is missing.**
///
/// Called only when multi-writer was demanded, so every failure here is a
/// refusal rather than a degradation: the operator asked for a guarantee
/// class, and half of it is not a class.
pub async fn arm_multi_writer(
    meta: &Arc<RoutedMetaBackend>,
    data_paths: &[PathBuf],
    read_only: bool,
    quarantine: Option<Arc<dyn CustodyQuarantine>>,
    backend: Option<&Arc<crate::routing::BackendRouter>>,
    pv: Option<&crate::partial_authority::SetAdmission>,
) -> Result<Option<MultiWriterArm>> {
    // Rung 1: a reader.
    if read_only {
        return Err(SqueezefsError::InvalidOperation(
            "multi-writer refuses to arm on a READ-ONLY mount: a reader takes no lease, writes \
             no claim and holds no NVMe registrant key (DLM S5's posture), so there is no write \
             custody to grant or hold. Drop -o ro / --read-only, or unset \
             SQUEEZEFS_MULTI_WRITER."
                .to_string(),
        ));
    }
    // Rung 2: the format's capability bits — before anything is acquired.
    check_capabilities(meta)?;

    // **The DERIVED ownership plane** (per-volume claim admission
    // §5.10/KD-PV-3), read here — before anything is acquired — because
    // **D20** decides from it WHICH planes this mount arms at all: the
    // owner of the volume hosting slot 0 is the SET AUTHORITY, and the
    // allocation-lane assignment, the custody endpoint, the WERO hold,
    // the roster enrollment and the freed-offset grace ring are its
    // singular planes. On an unassigned set — every set in the field
    // until `squeezefs volume set-owners` runs — the derivation answers
    // all-local, which is the shipped posture verbatim.
    let node_id = crate::cowriter::node_member_id()?;
    let map = derive_ownership(meta, &node_id, pv).await?;
    let set_authority = map.owns_slot_0();
    let peer_owned = map.volume_count() - map.local_volumes();
    if !set_authority {
        // A PARTIAL AUTHORITY: it appends to a subset and ships the rest,
        // and under D20 it must not take the set-singular planes. The
        // posture's own arm is the co-writer client half composed with an
        // owner half over its OWN volumes; refusing here is the
        // fail-closed answer until that composition lands with the fleet
        // rung that first runs it (PR 8), because arming half of it would
        // leave a mount that grants custody nobody may hold or that mints
        // lanes nobody granted.
        return Err(SqueezefsError::InvalidOperation(format!(
            "multi-writer refuses to arm: the volume hosting slot 0 is owned by '{}', so this \
             mount is a PARTIAL AUTHORITY under D20 — the set authority assigns allocation \
             lanes, serves the S9 custody endpoint, holds the one WERO reservation, enrolls the \
             roster and owns the only freed-offset grace ring. This arm is the SET authority's; \
             a partial authority's own arm (its custody lease from {}, the lane it installs \
             from that lease, and an owner half serving only the {} volume(s) it appends to) is \
             the fleet rung's, and half of it is not a posture",
            map.owner_of_volume(meta.route_ino(1).0)
                .map(|p| p.peer_id.clone())
                .unwrap_or_else(|| "an unresolved peer".to_string()),
            pv.map(|a| a.set_authority_endpoint())
                .filter(|e| !e.is_empty())
                .unwrap_or("SQUEEZEFS_MW_AUTHORITY"),
            map.local_volumes(),
        )));
    }

    // Rung 3: the substrate. S7's own arm takes (or JOINS) the WERO hold and
    // refuses a namespace that advertises no reservation support, naming it.
    let paths = data_paths.to_vec();
    let wero = squeezefs_ipc::sqz_blocking::run_blocking(move || {
        data_custody::arm_data_plane(CustodyPosture::MultiWriter, &paths, true)
    })
    .await?;

    // Rung 4: the membership plane. A co-writer that cannot be SEEN cannot
    // be EVICTED, and eviction is what mints the dead epoch S7's quarantine
    // is keyed on. Released-on-refusal, so a refused arm leaves no
    // reservation behind.
    if crate::membership::membership_mode() == "off" {
        if let Some(hold) = wero {
            data_custody::release_hold(hold).await;
        }
        return Err(SqueezefsError::InvalidOperation(
            "multi-writer refuses to arm: the membership plane is off, so co-writers cannot be \
             discovered, cannot appear in `squeezefs clients`, and — the part that matters — \
             cannot be EVICTED. S6's eviction is what mints the dead epoch whose offsets enter \
             the do-not-reallocate quarantine, so without it a dead co-writer's destinations \
             would be handed to a new owner. Set SQUEEZEFS_MEMBERSHIP_BIND (auto, or an \
             addr:port) on this mount."
                .to_string(),
        ));
    }

    // Rung 5: a durable era. Bit 7 is required above, so the D0 gate has
    // published one — a zero era here means the gate did not run.
    let term = crate::dlm::durable_term();
    if term == 0 {
        if let Some(hold) = wero {
            data_custody::release_hold(hold).await;
        }
        return Err(SqueezefsError::InvalidOperation(
            "multi-writer refuses to arm: this process has no durable writer era (dlm_term = \
             0), so every custody epoch would be the same constant and a revocation could not \
             be expressed at all. The era is published by the D0 mount gate's claim barrier on \
             a volume carrying incompat bit 7 — arm after it."
                .to_string(),
        ));
    }

    // Rung 5b (DLM S9's co-writer half — §6.2 item 7's PRODUCER side): commit
    // the operator-declared co-writer roster into every volume's durable
    // claim set. This is the ONLY place a co-writer's member entry is
    // written, and it must be the authority: the record is a metadata commit
    // on ino 1, and a co-writer holds no metadata authority over these
    // volumes — which is exactly why it cannot enroll itself and why its
    // admission rung 3 refuses an unenrolled node naming this knob.
    //
    // Deliberately BEFORE the listener starts: a co-writer that dials before
    // its enrollment is durable would be refused at rung 3, and an operator
    // reading the two logs would see the refusal without its cause.
    let roster = crate::cowriter::rostered_members();
    if !roster.is_empty() {
        match crate::cowriter::enroll_members(&meta.volumes, &roster, term).await {
            Ok(0) => log::warn!(
                "multi-writer: the co-writer roster {roster:?} committed NO claim-set entries — \
                 every volume of this set must carry incompat bit 14 for the record to exist \
                 (rung 2 refuses a half-engaged set, so those nodes will be refused with their \
                 own ids named)"
            ),
            Ok(n) => log::warn!(
                "multi-writer: {n} durable co-writer enrollment(s) committed for {roster:?} in \
                 era {term} — each named node may now pass admission rung 3"
            ),
            Err(e) => {
                if let Some(hold) = wero {
                    data_custody::release_hold(hold).await;
                }
                return Err(e);
            }
        }
    }

    // Rung 6: where the authority serves.
    let bind = match resolve_bind() {
        Ok(Bind::Off) => {
            if let Some(hold) = wero {
                data_custody::release_hold(hold).await;
            }
            return Err(SqueezefsError::InvalidOperation(format!(
                "multi-writer refuses to arm: {MW_BIND_ENV}=off, so this mount would grant no \
                 custody and serve no peer's publish path — an inert multi-writer mount is a \
                 misconfiguration, not a posture. Give it `auto` (an ephemeral port on every \
                 interface — ruling D2) or an addr:port, or unset SQUEEZEFS_MULTI_WRITER."
            )));
        }
        Ok(Bind::Auto) => "0.0.0.0:0".parse().expect("literal addr"),
        Ok(Bind::Addr(a)) => a,
        Err(e) => {
            if let Some(hold) = wero {
                data_custody::release_hold(hold).await;
            }
            return Err(e);
        }
    };

    // Everything demanded is present. Arm, in dependency order.
    let secret = cluster_secret(meta).await.ok_or_else(|| {
        SqueezefsError::InvalidOperation(
            "multi-writer refuses to arm: the volume set carries no `job:enroll` record, which \
             is this wire's root of trust (possession of volume access IS cluster membership — \
             ruling D2). Enable the cluster listener (SQUEEZEFS_JOB_WIRE_BIND) so the secret \
             exists."
                .to_string(),
        )
    })?;

    let clocks = LeaseClocks::derive(Duration::ZERO)?;
    let renew = clocks.renew_interval;
    let owner = WriteCustodyOwner::arm(
        &format!("mw-{}", uuid::Uuid::new_v4()),
        term,
        // The predecessor's era is the D0 gate's business, and it already
        // did it: S2's law is that a successor bumps `WriterClaim.term`
        // DURABLY BEFORE arming, so the era this mount holds is already
        // strictly greater than every predecessor's. Passing 0 records that
        // we rely on the gate's evidence rather than re-deriving it here
        // (the `term < durable_term()` guard inside `arm` is the backstop
        // against a caller that armed with an era the gate never published).
        0,
        clocks,
        LeaseClock::monotonic(),
        quarantine,
    )?;

    // DLM S9 blocker #3's ADMISSION (`crate::alloc_lane_grant`,
    // docs/design-mw-data-alloc-partition.md §9 item 1): this era's data-plane
    // allocation lane map, derived from the DURABLE claim set — the record
    // this arm just wrote the roster into, and the only source that can make
    // two nodes agree without a message. The authority keeps lane 0; each
    // enrolled writer member gets the next lane; the width is fixed for the
    // era, and it reaches every member on its custody lease.
    //
    // With NO enrolled co-writer this is lane 0 of 1 = SOLO, which installs
    // nothing at all: the authority's own allocation is then not "equivalent
    // to" the shipped path, it IS the shipped path.
    let assignment = match derive_lane_assignment(meta, &map).await {
        Ok(a) => a,
        Err(e) => {
            if let Some(hold) = wero {
                data_custody::release_hold(hold).await;
            }
            return Err(e);
        }
    };
    owner.install_lane_assignment(Arc::clone(&assignment));
    // Rung 10 — phantom-era frontier hygiene (rung-8 finding #4's named
    // residual): records of a width no live era runs are pruned HERE, at
    // the one node that both knows the era's width (it just derived it
    // from the durable claim set) and holds the metadata authority to
    // delete them. The pass folds a foreign-width record's protection
    // into every current-width lane FIRST (clause 3 preserved through the
    // delete); a solo era deletes outright — its D0 admission is the
    // no-live-peer proof and a solo engagement reads no records at all.
    // An undecodable record refuses the arm loud (the load law: a
    // watermark we cannot read is a floor we cannot honour — and one we
    // must not delete).
    match crate::data_alloc_lane::prune_stale_lane_records(meta, assignment.writers()).await {
        Ok(report) if report.pruned > 0 => {
            log::warn!(
                "multi-writer arm: pruned {} stale allocation-lane record(s) ({} folded into \
                 the current {}-way era first) — retired-width residue is gone from this set \
                 (alloc_lane_stale_records_pruned)",
                report.pruned,
                report.folded,
                assignment.writers(),
            );
        }
        Ok(_) => {}
        Err(e) => {
            if let Some(hold) = wero {
                data_custody::release_hold(hold).await;
            }
            return Err(SqueezefsError::InvalidOperation(format!(
                "multi-writer refuses to arm: the allocation-lane hygiene pass failed ({e}). A \
                 lane record that cannot be read or folded is a frontier that cannot be \
                 honoured — arming over it could let this era mint offsets a retired era's \
                 fenced writer may still hold"
            )));
        }
    }
    let authority_lane = assignment.authority_partition();
    if !authority_lane.is_solo() {
        let Some(backend) = backend else {
            if let Some(hold) = wero {
                data_custody::release_hold(hold).await;
            }
            return Err(SqueezefsError::InvalidOperation(format!(
                "multi-writer refuses to arm: this era's claim set enrolls {} co-writer(s), so \
                 the data plane must be partitioned into {} allocation lanes — but this arm was \
                 given no data-plane router to engage them on. An authority that granted lanes \
                 to peers while minting DENSE offsets itself would hand one device offset to two \
                 owners, silently on a passthrough volume",
                assignment.writers().saturating_sub(1),
                assignment.writers(),
            )));
        };
        // The dense-frontier source a served lane OPEN reads: a co-writer
        // never walks the tree, so the authority answers where the live data
        // ends from its own recovered cursor (composed with the durable
        // ledger).
        crate::alloc_lane_grant::install_frontier_source(
            crate::alloc_lane_grant::router_frontier_source(Arc::clone(backend)),
        );
        // The shipped-free EXECUTOR (DLM S9's co-writer free path,
        // `crate::cowriter::execute_shipped_frees`): a co-writer's
        // displaced-block terminal frees travel as publish verbs, and this
        // is the owner half that runs the full ladder — RAM release, tier
        // purge, reclaim enqueue, finish_free with the grace ring and the
        // quarantine composing inside it — against THIS mount's data
        // plane. Installed with the lane machinery because they are the
        // two halves of one rewrite: a lane grants the NEW block, the
        // executor retires the DISPLACED one.
        publish::install_free_executor(crate::cowriter::router_free_executor(
            Arc::clone(backend),
            Arc::clone(meta),
            // Finding 13: the ledger reader follows the LIVE ownership
            // plane — this process's map is this authority's own (and
            // rearm_ownership keeps it current across slot migration), so
            // peer-owned volumes' populations ship to their owners.
            crate::cowriter::live_owner_view(),
        ));
        // The free's REUSE half (rung 10, residual 2): the lane free
        // HARVEST executor — what makes a co-writer's freed supply
        // reachable again. Installed together because a set that returns
        // offsets to a lane's supply but can never hand them back leaks
        // toward ENOSPC on a store with free space.
        publish::install_harvest_executor(crate::cowriter::router_harvest_executor(Arc::clone(
            backend,
        )));
        // The lifetime's other half (finding 51): a served publish that
        // ADOPTS a foreign-lane block is this authority's witness that the
        // co-writer's DMA behind it completed — it re-publishes the word
        // the executor's `begin_free` retired when the offset's previous
        // lifetime was displaced. Without it a recycled co-writer block is
        // unreadable and un-foldable from the authority for ever (the
        // s11-mpiio `read_settle_lost_serialized` storm + fsync EIOs).
        {
            let br = Arc::clone(backend);
            publish::install_binding_witness(Arc::new(move |taken| {
                br.witness_served_bindings(taken);
            }));
        }
        // The served-publish SCREEN's probe (small-file packing PK4,
        // design-small-file-packing §5.6 (2)): a peer's layout publish
        // that would ADOPT a block this authority has RELEASED (free list /
        // grace ring / quarantine) is refused before anything is staged —
        // the belt under the co-writer's per-volume group law.
        publish::install_released_block_probe(crate::cowriter::router_released_block_probe(
            Arc::clone(backend),
        ));
        // The lane-push lever's authority half (finding 15 term 2,
        // `.benchmarks/2026-09-06-free-grace-lane-visible.md`): a binding
        // acknowledgement releases the covered offsets on arrival (every
        // ring harvested to its uncovered front), and the renewal grant
        // carries each co-writer's lane supply — the blocks of its lane
        // on these free lists, read off the free set's per-lane counters
        // (O(volumes) loads in the renewal hot op, never a scan).
        crate::free_grace::install_release_hook(release_hook(Arc::clone(backend)));
        crate::free_grace::install_lane_supply_source(lane_supply_source(
            Arc::clone(backend),
            Arc::clone(&assignment),
        ));
        if let Err(e) =
            crate::alloc_lane_grant::engage_authority_lanes(authority_lane, backend, meta).await
        {
            crate::alloc_lane_grant::uninstall_frontier_source();
            publish::uninstall_free_executor();
            publish::uninstall_harvest_executor();
            publish::uninstall_binding_witness();
            publish::uninstall_released_block_probe();
            crate::free_grace::uninstall_release_hook();
            crate::free_grace::uninstall_lane_supply_source();
            if let Some(hold) = wero {
                data_custody::release_hold(hold).await;
            }
            return Err(e);
        }
    }

    // Rung 9 — the S8 arm's owner half: the shipped `Metadata` verb
    // block (VERB_META_BATCH/VERB_RECLAIM) on the SAME listener. The
    // service adopts the durable era at construction
    // (`crate::dlm::durable_term()` — rung 5 above proved it nonzero),
    // so a successor authority's higher term makes every old-era
    // frame stale by construction (S8's era gate).
    //
    // Scoped to the volumes THIS node appends to (PR 7b): on the all-local
    // map every set in the field derives, that is every volume — the
    // shipped shape, `MetaShipService::new`'s own definition — while under
    // a multi-owner map a frame about a PEER's volume must meet the
    // `not_owner` refusal rather than be executed here, because a set
    // authority holds peer-owned volumes too (§5.7's correction 3).
    let meta_svc = crate::meta_ship::MetaShipService::with_authority(
        Arc::clone(meta),
        &map.local_volume_set(),
    );
    // Rung 12 — the S10 delegation host: the SAME service serves the
    // DelegRecall/DelegReassert block, and installing it is what arms the
    // coherence gate on this backend's mutation surface (grants may only
    // exist where the recall-before-conflicting-publish law is enforced).
    crate::meta_ship::install_delegation_host(Arc::clone(&meta_svc));
    let mut router = AsyncVerbRouter::new()
        .with_custody(Arc::clone(&owner))
        // Scoped like the meta service above (§5.7 correction 3, finding
        // 13): a served block-ref population answers from OWNED volumes
        // only — this node's peer-owned copies are lagged snapshots whose
        // truth belongs to their owners. On an unassigned set the local
        // set is every volume, the shipped shape verbatim.
        .with_publish(publish::PublishService::with_authority(
            Arc::clone(meta),
            &map.local_volume_set(),
        ))
        .with_meta(meta_svc);
    // The symmetric manager's verb block (design-symmetric-metadata §6.3,
    // PR 4 — PR 3's owed mount-path wiring): when any volume of the set
    // armed the symmetric plane (`SQUEEZEFS_SYMMETRIC_META=1` on a bit-17
    // volume), this node is those volumes' manager and serves
    // `JoinAppender` / the grants / the slot leases on this SAME listener,
    // dispatched by the frame's volume ordinal. Dark on every other mount.
    if meta.volumes.iter().any(|v| v.slot_lease_armed()) {
        router = router.with_manager(crate::meta_ship::manager::ManagerSetService::new(
            &meta.volumes,
        ));
        log::info!(
            "symmetric manager verbs served on the S8 listener for {} volume(s) \
             (design-symmetric-metadata §6.3)",
            meta.volumes.len()
        );
    }
    // PR 5 — the read-token verbs (design-symmetric-metadata §5.7): every
    // volume whose symmetric plane armed is a token HOLDER; its readers'
    // grants, standing recall channels, acks and releases ride this same
    // listener, dispatched by volume ordinal. Dark on every other mount.
    if meta.volumes.iter().any(|v| v.token_holder().is_some()) {
        router = router.with_tokens(crate::meta_ship::token_plane::TokenSetService::new(
            &meta.volumes,
        ));
        log::info!(
            "read-token verbs served on the S8 listener for {} volume(s) \
             (design-symmetric-metadata §5.7)",
            meta.volumes.len()
        );
    }
    let listener = match crate::cluster_wire::RpcListener::start_async(
        crate::cluster_wire::RpcListenerConfig {
            bind_addr: bind,
            security: None,
            service_threads: crate::cluster_wire::default_service_threads(),
            ..crate::cluster_wire::RpcListenerConfig::default()
        },
        secret.clone(),
        Arc::new(router),
    ) {
        Ok(l) => l,
        Err(e) => {
            // A host with no listener can never receive an ack — grants
            // must not exist without their recall wire.
            crate::meta_ship::uninstall_delegation_host();
            return Err(e);
        }
    };
    let endpoint = format!(
        "{}:{}",
        crate::cluster_wire::local_advertise_ip(),
        listener.endpoint().port()
    );
    // Rung 18 (the zeros-interleave conviction,
    // `.benchmarks/2026-08-17-s11-zeros-interleave-fix.md`): the §9.2
    // RANGE GEOMETRY source, from the authority's OWN planes. Until this
    // install, `install_range_geometry` had exactly two callers — both
    // test fixtures — so on every production mount `custody_scoped_layout`
    // ran its no-geometry arm (a range holder's full Put applied VERBATIM:
    // finding #3's peer-reverting clobber, the s11-range gate's standing
    // C8 mint) and every ranged acquire ran with `block_size = None`,
    // which also disarmed the §9.3 demotion barrier ("no geometry, no
    // barrier"). Installed for any arm with a data-plane router — the
    // solo arm included, so a later co-writer enrollment never runs a
    // window with grants and no geometry.
    if let Some(backend) = backend {
        owner.install_range_geometry(router_range_geometry(Arc::clone(meta), Arc::clone(backend)));
        // Rung 19 (the width-N refs composition): the authority-composed
        // layout commits — the chain-onto-head merge and the custody-
        // scoped Put — recompute their durable accounting from the
        // composition itself, and this resolver (the router's
        // `block_ref_for` behind the block_refs hook) is how the meta
        // plane names the displaced/inserted keys' `(vol_tag, block_idx)`.
        // Without it the caller's frame stands verbatim — the bc row's
        // swapped-pair C8 mint — so it arms wherever the geometry does.
        let refs_backend = Arc::clone(backend);
        crate::meta_backend::kv::block_refs::install_block_ref_resolver(Arc::new(
            move |key: &str, ino: u64, idx: u32| refs_backend.block_ref_for(key, ino, idx),
        ));
        // Rung 20 residual 1 (the blob-aware owner-side merge): the
        // indirect-map I/O hook — the authority's data router lent to the
        // meta plane, so the three owner-side compose sites (the
        // aggregated conveyor member, the direct chained merge, the
        // custody-scoped Put) REHYDRATE an indirect head's blob and
        // compose onto the FULL map instead of refusing forever (the
        // s11-mpiio row's retried-class fsync-EIO wedge). Arms wherever
        // the refs resolver does; unarmed mounts keep the refusal.
        crate::meta_backend::kv::indirect_map::install_indirect_map_io(indirect_map_io_for(
            Arc::clone(backend),
        ));
    }
    data_grant::install_custody_owner(Arc::clone(&owner));
    let secret_for_router = secret.clone();
    publish::install_client(publish::PublishClient::new(owner.id(), secret));

    // The ownership plane, from the map derived at the top of this arm
    // (assignment ∧ evidence — KD-PV-3). On an unassigned set every entry
    // is local and this is S8's shipped all-local arm verbatim; on an
    // assigned one the foreign entries are real, and `mint_redirects`
    // becomes the health gauge §11.2 describes rather than a structural
    // constant.
    let foreign = peer_owned;
    crate::meta_ship::arm_ownership(Arc::clone(&map));
    if map.multi_owner() {
        // The client half of the same plane: this mount's OWN daemon
        // verbs on a peer-owned volume must SHIP, and the trait boundary
        // is the one place no call site can bypass (rung 9's argument,
        // reached here by a set authority rather than a co-writer).
        crate::meta_ship::install_daemon_verb_router(crate::meta_ship::MetaShipRouter::new(
            Arc::clone(meta),
            &node_id,
            secret_for_router,
        ));
    }
    // Rung 14 (client-owned-slot placement): the policy's migration
    // vehicle — the existing online migrate-meta-slot engine over this
    // authority's own set, re-arming the ownership map at cutover (the
    // migration-while-armed law). On today's all-local maps the policy's
    // candidate inversion never fires (no shipping client owns a volume),
    // so installing the vehicle is reachability, not activity.
    crate::meta_ship::placement::install_migration_executor(
        crate::meta_ship::placement::authority_migration_executor(Arc::clone(meta)),
    );

    let stop = Arc::new(AtomicBool::new(false));
    spawn_cadence(Arc::clone(&owner), wero.clone(), Arc::clone(&stop), renew);
    // PR 7b: the endpoint halves. A set authority under a multi-owner map
    // both PUBLISHES where it serves (its own roster entry carries the
    // membership plane's port, not this listener's) and WAITS for its
    // peers to publish theirs — every peer is admitted after this arm ran,
    // so a map derived here can only be missing them. Both are structural
    // no-ops on the all-local map every set in the field derives.
    if map.multi_owner() {
        publish_owner_endpoint(meta, &map, &node_id, &endpoint, term).await;
        spawn_endpoint_refresh(Arc::clone(meta), None, Arc::clone(&stop), renew);
    }

    log::warn!(
        "MULTI-WRITER ARMED (DLM S9) on {endpoint}: era {term}, custody authority '{}', \
         data-plane fence class {}, {} of {} metadata volume(s) owned elsewhere. Peers may now \
         acquire write custody here and DMA directly to the shared namespaces — only custody \
         travels, never data (spec §6.9 S9)",
        owner.id(),
        data_custody::wero_mode(),
        foreign,
        meta.volumes.len(),
    );
    Ok(Some(MultiWriterArm {
        listener: Some(listener),
        owner,
        wero,
        stop,
        endpoint,
    }))
}

// ===========================================================================
// PR 7b — the PARTIAL-AUTHORITY arm
// (docs/design-per-volume-claim-admission.md §5.7's rev-6 correction 2;
// contracts `tests/pv_partial_arm_tests.rs`).
// ===========================================================================

/// What a partial-authority mount armed, and the teardown that undoes it.
///
/// It holds BOTH halves, which is the posture: the client half's custody
/// lease (and the membership lease + device registration its admission
/// took) and the owner half's listener.
pub struct PartialAuthorityArm {
    listener: Option<Arc<crate::cluster_wire::RpcListener>>,
    client: Arc<data_grant::WriteCustodyClient>,
    membership: Option<crate::membership::MembershipArm>,
    registrant: Option<crate::data_custody::WeroRegistrantJoin>,
    stop: Arc<AtomicBool>,
    endpoint: String,
    authority_endpoint: String,
    owned: usize,
    shipped: usize,
}

impl std::fmt::Debug for PartialAuthorityArm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PartialAuthorityArm")
            .field("endpoint", &self.endpoint)
            .field("authority", &self.authority_endpoint)
            .field("owned", &self.owned)
            .field("shipped", &self.shipped)
            .finish_non_exhaustive()
    }
}

impl PartialAuthorityArm {
    /// The address peers reach the volumes this mount OWNS on (its owner
    /// half's listener).
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The custody client every write of this mount acquires through.
    pub fn client(&self) -> &Arc<data_grant::WriteCustodyClient> {
        &self.client
    }

    /// Stop being a partial authority, **outside in**: stop serving the
    /// volumes we own before we stop holding what lets us write, and stop
    /// both before this node stops being a member — the same order the
    /// two shipped arms tear down in, for the same reason (no window in
    /// which a peer is answered by a mount that has already gone).
    pub async fn disarm(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(listener) = self.listener.take() {
            listener.shutdown();
        }
        crate::meta_ship::uninstall_delegation_host();
        data_grant::uninstall_custody_client();
        publish::uninstall_client();
        crate::meta_ship::uninstall_daemon_verb_router();
        crate::alloc_lane_grant::uninstall_frontier_source();
        crate::meta_backend::kv::block_refs::uninstall_block_ref_resolver();
        crate::meta_backend::kv::indirect_map::uninstall_indirect_map_io();
        crate::extent_ship::uninstall_quiesce_hook();
        crate::extent_ship::uninstall_release_hook();
        crate::extent_ship::uninstall_spill_sink();
        crate::data_grant::uninstall_release_gate();
        crate::meta_ship::disarm_ownership();
        if let Some(arm) = self.membership.take() {
            arm.disarm().await;
        }
        if let Some(join) = self.registrant.take() {
            squeezefs_ipc::sqz_blocking::run_blocking(move || drop(join)).await;
        }
        log::warn!(
            "PARTIAL AUTHORITY DISARMED: the volumes this mount appended to are served by \
             nobody until their owner mounts again, nothing ships, no custody is held, and this \
             node is no longer a registrant of the data namespaces' WERO hold"
        );
    }
}

/// **Arm the partial-authority posture, or refuse naming what is missing**
/// (§5.7's rev-6 correction 2 — the arm no rung owned).
///
/// A partial authority appends to SOME volumes of the set and ships the
/// rest, so it is the only posture that is a CLIENT and an OWNER at once,
/// and both halves must come up together:
///
/// | half | over | what it installs |
/// |---|---|---|
/// | **client** | the volumes a PEER appends to | the S9 custody lease from the SET authority, the allocation lane that lease carries, the publish client, the daemon verb router, the closed local free/reclaim accounting, the renewal cadence — `cowriter::install_client_halves`, composed and never forked |
/// | **owner** | the volumes THIS node appends to | the S8 `MetaShipService` scoped to exactly those volumes (its dedup window, its era gate and its failover grace window come with it), the S9 `PublishService`, the S10 delegation host, and the owner-side compose hooks — on a listener with **no custody service** |
///
/// **D20 draws the line, and it is what this arm must not cross**: the
/// allocation-lane assignment, the S9 custody endpoint, the one WERO
/// hold, the membership OWNER role, the freed-offset grace ring and
/// maintenance coordination are the SET authority's. This arm takes none
/// of them: it installs no custody owner, derives no lane assignment
/// (`derive_lane_assignment` refuses a non-set-authority at the site),
/// serves no custody verb, and runs no free/harvest executor — a peer's
/// shipped free travels to the custody endpoint, which is the set
/// authority by construction.
///
/// **It refuses rather than half-arming** (KD-PV-3's fail-closed law, the
/// arming face). Every refusal below happens before anything is installed
/// except where noted, and the one that happens after tears down what it
/// took:
///
/// 1. the admission is a PARTIAL authority's, over THIS set;
/// 2. every volume carries the six capability bits (shared with the set
///    authority's rung 2);
/// 3. the DERIVED map agrees with the admission — same owned set, and it
///    does not make us the set authority. A disagreement refuses with
///    both sides named, because the map is the only thing that can catch
///    an assignment that moved under a decision already taken;
/// 4. the SET AUTHORITY is REACHED — not merely named. The endpoint comes
///    from the derived map (D20's declaration for the slot-0 owner), and
///    the custody connect is the proof: a mount that armed its owner half
///    without it would serve peers' verbs while unable to write a byte of
///    data, so that failure unwinds BOTH halves;
/// 5. a durable writer era (this mount APPENDS: era-less fencing tokens
///    on the volumes it owns are the shape S2 exists to prevent);
/// 6. a bind for the owner half — `off` is a refusal, because the volumes
///    this node owns would then be unreachable to every peer, which is a
///    set with a hole.
pub async fn arm_partial_authority(
    meta: &Arc<RoutedMetaBackend>,
    router: &crate::routing::DataRouter,
    preflight: crate::partial_authority::SetPreflight,
) -> Result<PartialAuthorityArm> {
    use crate::cowriter::MwRole;

    let crate::partial_authority::SetPreflight {
        admission,
        membership,
        registrant,
        hold,
        secret,
    } = preflight;

    // Rung 1: the decision this arm stands on must be a partial
    // authority's, and it must be about THIS set. A `set-authority`
    // preflight also carries the standing WERO hold its own rung 5 took —
    // RELEASED here rather than dropped, because the reservation ioctls
    // are blocking and a drop would run them on the runtime.
    if admission.role() != MwRole::PartialAuthority || admission.is_set_authority() {
        if let Some(hold) = hold {
            data_custody::release_hold(hold).await;
        }
        return Err(SqueezefsError::InvalidOperation(format!(
            "the partial-authority arm refuses: the admission it was handed is a '{}' \
             decision{}. This arm installs the CLIENT halves toward a set authority and an \
             owner half over a SUBSET — a set authority's arm is `arm_multi_writer`, which \
             takes the planes D20 gives it exclusively",
            admission.role().as_str(),
            if admission.is_set_authority() {
                " that owns the slot-0 volume"
            } else {
                ""
            }
        )));
    }
    if let Some(hold) = hold {
        data_custody::release_hold(hold).await;
        return Err(SqueezefsError::InvalidOperation(
            "the partial-authority arm refuses: its preflight took a WERO HOLD rather than a \
             registrant join. Under D20 the set authority holds the one reservation and every \
             other writer registers under it; a second holder would conflict at the device and \
             silently take the fence away from the authority"
                .to_string(),
        ));
    }
    let paths: Vec<String> = meta
        .volumes
        .iter()
        .map(|v| v.device_path().display().to_string())
        .collect();
    if !admission.covers(&paths) {
        return Err(SqueezefsError::InvalidOperation(format!(
            "the partial-authority arm refuses: the admission was decided over {:?}, and this \
             mount opened {paths:?}. An admission decided over one set may never arm another \
             (the `open_peer_owned` law, one layer up)",
            admission.volumes()
        )));
    }
    // Rung 2: the format's capability bits — before anything is acquired
    // (shared verbatim with the set authority's arm).
    check_capabilities(meta)?;

    // Rung 3: the DERIVED map (assignment ∧ evidence, KD-PV-3) must agree
    // with the decision the open was taken under. The admission was
    // decided over PROBE reads before the set was opened; this reads the
    // OPEN set, including the D0 grants this mount actually holds, so it
    // is the stronger evidence and the last chance to catch an assignment
    // that moved under us.
    let node_id = admission.node_id().to_string();
    let map = derive_ownership(meta, &node_id, Some(&admission)).await?;
    if map.owns_slot_0() {
        return Err(SqueezefsError::InvalidOperation(
            "the partial-authority arm refuses: the derived ownership map says this node \
             appends to the volume hosting slot 0, which makes it the SET AUTHORITY under D20 \
             — the posture that keeps the lane assignment, the custody endpoint, the WERO hold \
             and the grace ring. The admission said otherwise, so assignment and evidence moved \
             apart between the probe and the open: remount, and declare \
             SQUEEZEFS_MW_ROLE=set-authority if the assignment really is this node's"
                .to_string(),
        ));
    }
    let owned = map.local_volume_set();
    let admitted: Vec<usize> = (0..meta.volumes.len())
        .filter(|&v| {
            matches!(
                admission.mode_for(&meta.volumes[v].durable_volume_id()),
                Some(crate::partial_authority::VolumeMode::Own)
            )
        })
        .collect();
    if owned != admitted {
        return Err(SqueezefsError::InvalidOperation(format!(
            "the partial-authority arm refuses: the seven-rung admission decided this mount \
             appends to volume(s) {admitted:?} and the DERIVED ownership map says {owned:?}. \
             Assignment and evidence disagree, and half a posture is not a posture — this mount \
             would either serve a volume it does not own or ship one it does. Re-run `squeezefs \
             volume get-owners` (it prints assignment beside evidence) and remount"
        )));
    }
    if owned.is_empty() || !map.multi_owner() {
        return Err(SqueezefsError::InvalidOperation(format!(
            "the partial-authority arm refuses: the derived map gives this mount {} owned and \
             {} peer-owned volume(s). A partial authority is by definition both — with no \
             peer-owned volume it is an ordinary authority mount, and with no owned volume it \
             is a co-writer (built, measured and cheaper)",
            owned.len(),
            map.volume_count() - owned.len()
        )));
    }

    // Rung 4: WHERE the set authority is. Its endpoint is the custody
    // source, the lane grant's origin and where every peer-owned volume's
    // verbs ship. It comes from the derived map (which resolved it from
    // the durable claim set, or — for the slot-0 owner — from the
    // operator's declaration under D20), and its tail is the admission's
    // own declared endpoint, which rung 1 of the ladder guarantees is
    // present for this posture.
    //
    // REACHABILITY is not asserted here, because a string is not a peer:
    // the custody connect below is the proof, and its failure unwinds
    // BOTH halves rather than leaving one standing.
    let authority_endpoint = map
        .set_authority()
        .map(|p| p.endpoint.clone())
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| admission.set_authority_endpoint().to_string());

    // Rung 5: a durable era. Unlike a co-writer, a partial authority
    // APPENDS — every fencing token it mints on its own volumes carries
    // this era, and a zero era is one S2's ladder never published.
    let term = crate::dlm::durable_term();
    if term == 0 {
        return Err(SqueezefsError::InvalidOperation(
            "the partial-authority arm refuses: this process has no durable writer era \
             (dlm_term = 0), so every fencing token it minted on the volumes it appends to \
             would be era-less and every custody epoch a constant. The era is published by the \
             D0 mount gate's claim barrier on a volume carrying incompat bit 7 — arm after it"
                .to_string(),
        ));
    }

    // Rung 6: where the OWNER half serves. `off` is a refusal for the
    // same reason it is one on a set authority, one grain finer: the
    // volumes this node owns would be unreachable, and a set whose peers
    // cannot reach one of its owners is a set with a hole.
    let bind = match resolve_bind()? {
        Bind::Off => {
            return Err(SqueezefsError::InvalidOperation(format!(
                "the partial-authority arm refuses: {MW_BIND_ENV}=off, so no peer could reach \
                 the {} volume(s) this mount appends to — their metadata verbs and layout \
                 publishes have nowhere to land, and the set would have volumes no node can \
                 serve. Give it `auto` (an ephemeral port on every interface — ruling D2) or an \
                 addr:port",
                owned.len()
            )));
        }
        Bind::Auto => "0.0.0.0:0".parse().expect("literal addr"),
        Bind::Addr(a) => a,
    };
    let refresh_cadence = crate::membership::LeaseClocks::derive(Duration::ZERO)?.renew_interval;

    // ---- Everything demanded is present. Arm, in dependency order. ----

    // The ownership plane FIRST: every routing decision below reads it —
    // the owner half's authority set, the client half's ship targets, and
    // the lane OPEN, which routes on the owner of ino 1.
    crate::meta_ship::arm_ownership(Arc::clone(&map));

    let armed = match arm_partial_halves(
        meta,
        router,
        &map,
        &admission,
        PartialHalves {
            bind,
            node_id: &node_id,
            authority_endpoint: &authority_endpoint,
            secret,
            term,
            refresh_cadence,
        },
    )
    .await
    {
        Ok(a) => a,
        Err(e) => {
            // Fail-closed: a refusal here leaves NOTHING installed. A
            // mount holding one half is the shape §5.7's correction
            // named, and it is worse than a refusal — it grants peers a
            // metadata authority whose data plane cannot write.
            crate::meta_ship::disarm_ownership();
            return Err(e);
        }
    };

    let shipped = map.volume_count() - owned.len();
    log::warn!(
        "PARTIAL AUTHORITY ARMED (per-volume claim admission, D20) on {}: era {term}, node \
         '{node_id}'. It APPENDS to {} volume(s) — served to peers on this endpoint through the \
         S8 metadata verbs and the S9 publish path, with no custody service — and SHIPS {} \
         volume(s) to their owners, holding write custody and allocation lane {} of {} from the \
         SET AUTHORITY at {authority_endpoint}. Terminal frees, device reclaim, the WERO hold, \
         the grace ring and maintenance coordination all stay the set authority's (D20)",
        armed.endpoint,
        owned.len(),
        shipped,
        armed.client.lane_partition().writer_id(),
        armed.client.lane_partition().writers(),
    );
    Ok(PartialAuthorityArm {
        listener: Some(armed.listener),
        client: armed.client,
        membership,
        registrant,
        stop: armed.stop,
        endpoint: armed.endpoint,
        authority_endpoint,
        owned: owned.len(),
        shipped,
    })
}

/// [`arm_partial_authority`]'s inputs after its ladder passed.
struct PartialHalves<'a> {
    bind: std::net::SocketAddr,
    node_id: &'a str,
    authority_endpoint: &'a str,
    secret: Vec<u8>,
    term: u64,
    /// How often the endpoint-refresh pass re-reads while a peer's
    /// address is still unpublished. Resolved in the LADDER, before
    /// anything is installed: a derivation that can fail must never be
    /// the last statement of an arm that has already armed both halves.
    refresh_cadence: Duration,
}

/// What the two halves installed.
struct ArmedHalves {
    listener: Arc<crate::cluster_wire::RpcListener>,
    client: Arc<data_grant::WriteCustodyClient>,
    stop: Arc<AtomicBool>,
    endpoint: String,
}

/// Install the owner half then the client half, unwinding **everything**
/// on any failure — the fail-closed law's arming face, expressed as one
/// function so no early return can leave a mount holding half a posture.
///
/// One durable act survives a failed arm and is meant to: the endpoint
/// this mount published on the volumes it owns. It cannot be unwritten
/// atomically, it is leak-safe (a peer dialling a dead listener refuses
/// loud rather than guessing), and the next successful arm rewrites it.
async fn arm_partial_halves(
    meta: &Arc<RoutedMetaBackend>,
    router: &crate::routing::DataRouter,
    map: &Arc<OwnerMap>,
    admission: &crate::partial_authority::SetAdmission,
    params: PartialHalves<'_>,
) -> Result<ArmedHalves> {
    let PartialHalves {
        bind,
        node_id,
        authority_endpoint,
        secret,
        term,
        refresh_cadence,
    } = params;

    // ---- The OWNER half: the volumes this node appends to. ----
    //
    // Scoped to exactly those volumes, so a frame about a peer's volume
    // meets the `not_owner` refusal instead of being executed by a node
    // the record does not entitle (the S8 service's own gate, given the
    // per-volume answer it was always written for).
    let meta_svc = crate::meta_ship::MetaShipService::with_authority(
        Arc::clone(meta),
        &map.local_volume_set(),
    );
    // The S10 delegation host on the same service: grants may only exist
    // where the recall-before-conflicting-publish law is enforced, and
    // installing it is what arms that gate on this backend's mutation
    // surface (the set authority's arm's rung 12, over a subset).
    crate::meta_ship::install_delegation_host(Arc::clone(&meta_svc));
    // NO custody service, deliberately (D20): custody is granted by the
    // set authority alone, and a second grantor is two nodes handing out
    // the same bytes.
    let svc = AsyncVerbRouter::new()
        // Scoped exactly as the set authority's is (finding 13): a
        // partial answers population reads for the volumes it appends to,
        // never from its lagged copies of a peer's.
        .with_publish(publish::PublishService::with_authority(
            Arc::clone(meta),
            &map.local_volume_set(),
        ))
        .with_meta(meta_svc);
    let listener = crate::cluster_wire::RpcListener::start_async(
        crate::cluster_wire::RpcListenerConfig {
            bind_addr: bind,
            security: None,
            service_threads: crate::cluster_wire::default_service_threads(),
            ..crate::cluster_wire::RpcListenerConfig::default()
        },
        secret.clone(),
        Arc::new(svc),
    )
    .inspect_err(|_| crate::meta_ship::uninstall_delegation_host())?;
    let endpoint = format!(
        "{}:{}",
        crate::cluster_wire::local_advertise_ip(),
        listener.endpoint().port()
    );

    // The owner-side compose hooks (rungs 19/20), for the same reason the
    // set authority installs them: a SERVED layout publish recomputes its
    // durable accounting from the composition, and an indirect head must
    // be rehydrated rather than refused. They are data-router functions
    // over the volumes this node appends to, not accounting acts, so the
    // co-writer latch does not reach them.
    let backend = Arc::clone(&router.backend_router);
    let refs_backend = Arc::clone(&backend);
    crate::meta_backend::kv::block_refs::install_block_ref_resolver(Arc::new(
        move |key: &str, ino: u64, idx: u32| refs_backend.block_ref_for(key, ino, idx),
    ));
    crate::meta_backend::kv::indirect_map::install_indirect_map_io(indirect_map_io_for(
        Arc::clone(&backend),
    ));
    // The dense-frontier source its own lane OPEN and any served lane
    // question read from THIS mount's cursors.
    crate::alloc_lane_grant::install_frontier_source(
        crate::alloc_lane_grant::router_frontier_source(Arc::clone(&backend)),
    );
    // Publish WHERE this mount serves, on the volumes it owns, so peers
    // (the set authority above all) can ship to it at all.
    publish_owner_endpoint(meta, map, node_id, &endpoint, term).await;

    // ---- The CLIENT half: the volumes a PEER appends to. ----
    let stop = Arc::new(AtomicBool::new(false));
    let client = crate::cowriter::install_client_halves(
        meta,
        router,
        &crate::cowriter::ClientHalfParams {
            posture: "partial authority",
            node_id,
            authority_endpoint,
            pr_key: admission.pr_key(),
            lane_remedy: "The set authority derives the lane width from the durable claim set \
                          the offline `squeezefs volume set-owners` verb writes — re-run it so \
                          this node is an enrolled writer member, then re-arm the authority",
        },
        secret,
        &stop,
    )
    .await
    .inspect_err(|_| {
        listener.shutdown();
        crate::meta_ship::uninstall_delegation_host();
        crate::alloc_lane_grant::uninstall_frontier_source();
        crate::meta_backend::kv::block_refs::uninstall_block_ref_resolver();
        crate::meta_backend::kv::indirect_map::uninstall_indirect_map_io();
    })?;

    // The peers this mount ships to may not have published their
    // endpoints yet (the set authority always derives its map before its
    // peers exist — see [`spawn_endpoint_refresh`]).
    spawn_endpoint_refresh(
        Arc::clone(meta),
        Some(authority_endpoint.to_string()),
        Arc::clone(&stop),
        refresh_cadence,
    );

    Ok(ArmedHalves {
        listener,
        client,
        stop,
        endpoint,
    })
}

/// **Derive this mount's ownership map** (§5.10, KD-PV-3): assignment ∧
/// evidence, per volume, fail-closed.
///
/// The endpoint of a peer that appends to one of this set's volumes is
/// resolved from DURABLE state — its own claim-set member record — and,
/// for the owner of the slot-0 volume, from the declared set-authority
/// endpoint (D20's `SQUEEZEFS_MW_AUTHORITY`) or the admission that named
/// it. An unresolvable endpoint is announced, never refused: refusing
/// would make the first node of a fleet unmountable, and a verb toward an
/// empty endpoint refuses loud at the ship site.
async fn derive_ownership(
    meta: &Arc<RoutedMetaBackend>,
    node_id: &str,
    pv: Option<&crate::partial_authority::SetAdmission>,
) -> Result<Arc<OwnerMap>> {
    let book = EndpointBook::gather(meta, pv).await;
    crate::meta_ship::owners::derive_owner_map(meta, node_id, &|id| book.resolve(id)).await
}

/// **Where each durable member id serves its metadata plane** — the one
/// place both arms and the refresh cadence resolve a `PeerOwner`'s
/// endpoint, so the three cannot answer differently about one peer.
pub(crate) struct EndpointBook {
    /// `(member id, endpoint)` as the durable claim sets publish it.
    published: Vec<(String, String)>,
    /// The durable id assigned to append to the slot-0 volume (D20's set
    /// authority), when the record names one.
    slot_0_owner: Option<String>,
    /// The operator-declared set-authority endpoint
    /// (`SQUEEZEFS_MW_AUTHORITY`, or the admission that carried it).
    declared: Option<String>,
}

impl EndpointBook {
    /// Read every volume's durable claim set, plus the declared
    /// set-authority endpoint. One pass over the open set; no wire.
    pub(crate) async fn gather(
        meta: &Arc<RoutedMetaBackend>,
        pv: Option<&crate::partial_authority::SetAdmission>,
    ) -> Self {
        let mut published: Vec<(String, String)> = Vec::new();
        for vol in &meta.volumes {
            let Some(set) = crate::membership::ClaimSet::load(vol).await else {
                continue;
            };
            for m in &set.members {
                if let Some(ep) = m.identity.endpoint.as_ref().filter(|e| !e.is_empty()) {
                    if !published.iter().any(|(id, _)| *id == m.identity.id) {
                        published.push((m.identity.id.clone(), ep.clone()));
                    }
                }
            }
        }
        Self {
            published,
            slot_0_owner: crate::membership::ClaimSet::load(&meta.volumes[meta.route_ino(1).0])
                .await
                .and_then(|set| set.owner),
            // D20: whoever appends to the slot-0 volume is the SET
            // AUTHORITY, and that is the one endpoint an operator declares.
            declared: pv
                .map(|a| a.set_authority_endpoint().to_string())
                .filter(|e| !e.is_empty())
                .or_else(crate::cowriter::declared_authority),
        }
    }

    /// The endpoint to ship `id`'s volumes to, or `None` = not published
    /// yet (announced, never refused — a verb toward an empty endpoint
    /// refuses loud at the ship site, and
    /// [`crate::meta_ship::owners::refresh_peer_endpoints`] fills it in
    /// when the peer publishes).
    ///
    /// **The DECLARED endpoint wins for the slot-0 owner**, and that
    /// order is load-bearing: `SQUEEZEFS_MW_AUTHORITY` names the set
    /// authority's `SQUEEZEFS_MW_BIND` — the listener that serves custody,
    /// the shipped publish path and the S8 metadata verbs — while the
    /// endpoint the claim-set roster carries for a membership OWNER is its
    /// MEMBERSHIP plane's (`arm_owner` writes it there, and that listener
    /// serves membership verbs only). Reading the roster first sent every
    /// metadata verb of a multi-owner set to a port that answers
    /// `RPC_UNKNOWN_VERB`.
    pub(crate) fn resolve(&self, id: &str) -> Option<String> {
        if let (Some(owner), Some(ep)) = (&self.slot_0_owner, &self.declared) {
            if crate::membership::member_id_matches(owner, id) {
                return Some(ep.clone());
            }
        }
        self.published
            .iter()
            .find(|(pid, _)| pid == id)
            .map(|(_, ep)| ep.clone())
    }
}

/// Keep filling in peer endpoints until every owned-elsewhere volume has
/// one (§5.10's "announced, never refused" made temporary).
///
/// The cadence exists because the fleet's own admission order guarantees
/// the window: a partial authority is refused at rung 4 until the set
/// authority's membership plane is live, so the set authority always
/// derives its map before any peer has published an endpoint. Structurally
/// inert on every mount that ships — an unassigned set derives an
/// all-local map, which has no foreign entry to resolve — and it exits for
/// good the moment the last one is filled.
fn spawn_endpoint_refresh(
    meta: Arc<RoutedMetaBackend>,
    pv_endpoint: Option<String>,
    stop: Arc<AtomicBool>,
    cadence: Duration,
) {
    if crate::meta_ship::owners::unresolved_peer_endpoints() == 0 {
        return;
    }
    crate::meta_exec::spawn_meta_contained("mw_endpoint_refresh", async move {
        loop {
            squeezefs_ipc::sqz_time::sleep(cadence).await;
            if stop.load(Ordering::Acquire) {
                return;
            }
            let mut book = EndpointBook::gather(&meta, None).await;
            book.declared = book.declared.or_else(|| pv_endpoint.clone());
            crate::meta_ship::owners::refresh_peer_endpoints(&|id| book.resolve(id));
            if crate::meta_ship::owners::unresolved_peer_endpoints() == 0 {
                log::info!(
                    "ownership map: every peer-owned volume's owner has published an endpoint — \
                     the refresh cadence is done"
                );
                return;
            }
        }
    })
}

/// **Publish where this mount serves the volumes it OWNS**, so peers can
/// resolve it (the other half of [`EndpointBook`]).
///
/// Written into the claim set of each OWNED volume — this mount is that
/// volume's appender, so the commit is its own to make and a peer-owned
/// volume's write gate never sees it (sweep row 8).
///
/// **It EDITS the existing enrollment rather than composing a new one**,
/// and both halves of that matter:
///
/// * the entry's `id` is carried verbatim, because KD-MW-2's bare-node
///   form is a slot WILDCARD — composing this mount's exact
///   `node_….m…` id beside a bare `node_…` enrollment would add a
///   member rather than replace one, and the member count is the
///   allocation partition's width;
/// * `pid`/`boot`/`pr_key` are carried verbatim, because they are other
///   mechanisms' facts: KD-PV-4's pid-less form is what exempts an
///   assignee from the rung-8 same-boot dead-writer prune (pruning a node
///   the record still ASSIGNS a volume to manufactures exactly the
///   assignment-vs-enrollment disagreement rung 3 then refuses the whole
///   set over), and `pr_key` is the registrant a successor's drain proof
///   preempts.
///
/// A node the set does not enroll writes NOTHING: self-enrollment is the
/// self-assertion the whole plane refuses (rung 3 has already proved we
/// are enrolled on every volume, so this arm is unreachable in practice
/// and loud if it ever is not).
async fn publish_owner_endpoint(
    meta: &Arc<RoutedMetaBackend>,
    map: &OwnerMap,
    node_id: &str,
    endpoint: &str,
    term: u64,
) {
    for (v_idx, vol) in meta.volumes.iter().enumerate() {
        if map.owner_of_volume(v_idx).is_some() {
            continue;
        }
        let enrolled = crate::membership::ClaimSet::load(vol)
            .await
            .and_then(|set| {
                set.members
                    .iter()
                    .find(|m| crate::membership::member_id_matches(&m.identity.id, node_id))
                    .map(|m| m.identity.clone())
            });
        let Some(mut identity) = enrolled else {
            log::warn!(
                "ownership map: metadata volume {} does not enroll this node '{node_id}', so \
                 there is no member entry to publish this mount's endpoint {endpoint} on. Peers \
                 will refuse verbs about it until `squeezefs volume set-owners` re-runs",
                vol.device_path().display()
            );
            continue;
        };
        if identity.endpoint.as_deref() == Some(endpoint) {
            continue; // idempotent: a remount at the same address writes nothing
        }
        identity.endpoint = Some(endpoint.to_string());
        if let Err(e) = crate::membership::upsert_writer_member(vol, &identity, term).await {
            log::warn!(
                "ownership map: publishing this mount's endpoint {endpoint} on volume {} failed \
                 ({e}) — peers will refuse verbs about the {} volume(s) this node appends to \
                 until a later mount publishes it",
                vol.device_path().display(),
                map.local_volumes()
            );
        }
    }
}

/// Derive this era's allocation lane map from the volume set's **durable**
/// claim sets (DLM S9 blocker #3's admission).
///
/// The authority's own identity in that record is the **membership owner's**
/// id — the entry `membership::arm_mount_membership` writes for itself — not
/// the custody authority's `mw-{uuid}`: the claim set is §6.2 item 7's
/// membership record, and the roster this arm enrolled sits beside that one
/// entry. Without an armed membership owner there is no such identity, and
/// then this authority runs SOLO rather than guessing which entry is itself
/// (guessing wrong would hand a co-writer the authority's own lane).
///
/// **D20 (§5.7): only the SET AUTHORITY derives.** The width rounds up to
/// a power of two over the union of Writer members, so two nodes deriving
/// it from rosters they read at different instants can hand two writers
/// the same residue class — the collision the data-plane allocation
/// partition exists to prevent. A mount that does not append to the
/// slot-0 volume installs the `(writer_lane, writers)` pair its CUSTODY
/// LEASE carries (`install_mount_partition`, which already refuses a
/// second, different partition loud), so reaching this derivation at all
/// is the error, and it refuses rather than answering.
pub async fn derive_lane_assignment(
    meta: &Arc<RoutedMetaBackend>,
    map: &OwnerMap,
) -> Result<Arc<crate::alloc_lane_grant::LaneAssignment>> {
    if !map.owns_slot_0() {
        return Err(SqueezefsError::InvalidOperation(
            "refusing to derive an allocation-lane assignment: this mount does not append to \
             the volume hosting slot 0, so under D20 it is not the SET AUTHORITY. Only the set \
             authority derives an era's lane width from the durable claim sets; every other \
             writer installs the `(writer_lane, writers)` pair its custody lease carries. Two \
             nodes deriving a width from one roster is exactly the collision the data-plane \
             allocation partition exists to prevent"
                .to_string(),
        ));
    }
    let me = match crate::membership::installed_owner() {
        // Rung-8 finding #4: the ONE-identity law — the derivation must use
        // the same DURABLE claim identity `arm_owner` upserted (the owner's
        // id is the INCARNATION uuid; passing it made the authority's own
        // claim entry read as a foreign co-writer, so every solo MW mount
        // ran a phantom W=2 partition against itself and paid a durable
        // reservation frontier fsck C6 correctly reported as drift).
        Some(owner) => crate::membership::owner_claim_identity(owner.id()),
        None => {
            log::warn!(
                "multi-writer: no membership OWNER is installed on this mount, so its own \
                 claim-set identity is unknown — running SOLO (no data-plane allocation \
                 partition) rather than risking handing a co-writer this node's own lane"
            );
            return crate::alloc_lane_grant::LaneAssignment::derive("", &[]);
        }
    };
    let mut sets = Vec::new();
    for vol in &meta.volumes {
        if let Some(set) = crate::membership::ClaimSet::load(vol).await {
            // Only the DURABLE record is a roster: the projection of a
            // singular `writer_claim` expresses EXCLUSION and cannot name a
            // second member (the co-writer ladder's rung 2, same reason).
            if set.durable {
                sets.push(set);
            }
        }
    }
    crate::alloc_lane_grant::LaneAssignment::derive(&me, &sets)
}

/// Every meta volume must carry every required bit. The refusal names the
/// FIRST missing one, its meaning and its offline stamping path.
fn check_capabilities(meta: &Arc<RoutedMetaBackend>) -> Result<()> {
    if meta.volumes.is_empty() {
        return Err(SqueezefsError::InvalidOperation(
            "multi-writer refuses to arm: the metadata set has no volumes".to_string(),
        ));
    }
    for vol in &meta.volumes {
        let have = vol.superblock().features_incompat;
        let missing = REQUIRED_INCOMPAT & !have;
        if missing != 0 {
            let bit = 1u64 << missing.trailing_zeros();
            return Err(SqueezefsError::InvalidOperation(format!(
                "multi-writer refuses to arm: metadata volume {} does not carry incompat bit {} \
                 ({}), so the format cannot express a second writer safely. Nothing stamps it \
                 today (ruling D9: the bits are built, not stamped) — the Phase-8 reformat \
                 window stamps it offline via {}. Missing mask {:#x} of the required {:#x}.",
                vol.device_path().display(),
                missing.trailing_zeros(),
                bit_name(bit),
                stamping_path(bit),
                missing,
                REQUIRED_INCOMPAT
            )));
        }
    }
    Ok(())
}

async fn cluster_secret(meta: &Arc<RoutedMetaBackend>) -> Option<Vec<u8>> {
    let first = meta.volumes.first()?;
    crate::membership::cluster_secret(first).await
}

/// The authority's cadence task: sweep expired client leases, and turn each
/// death into a **drain proof** where the substrate allows one.
///
/// The proof ladder is the job wire's §5.1.6 rung 2, reused verbatim: PR-
/// preempt the dead co-writer's registrant key under the standing WERO hold
/// (its resumed DMA is then rejected by the DEVICE), and only then release
/// its quarantined offsets. Without a registrant key — a detection-grade
/// peer — the offsets stay quarantined until recovery produces an attested
/// proof, which is honestly-unavailable space rather than a silent
/// reallocation.
fn spawn_cadence(
    owner: Arc<WriteCustodyOwner>,
    wero: Option<WeroHold>,
    stop: Arc<AtomicBool>,
    cadence: Duration,
) {
    crate::meta_exec::spawn_meta_contained("mw_custody_sweep", async move {
        loop {
            squeezefs_ipc::sqz_time::sleep(cadence).await;
            if stop.load(Ordering::Acquire) {
                return;
            }
            // Rung-9 finding #2: RE-VERIFY the standing WERO against the
            // device every sweep — before this, the fence-mode gauge read
            // 1 forever, even over a reservation the device no longer
            // held (a vanished hold re-acquires and counts on
            // `data_plane_wero_reacquires`; a foreign-usurped hold
            // poisons custody and drops the gauge — never healed over).
            if let Some(hold) = wero.clone() {
                let verdict =
                    squeezefs_ipc::sqz_blocking::run_blocking(move || hold.reverify_and_heal())
                        .await;
                match verdict {
                    crate::data_custody::WeroReverify::Held
                    | crate::data_custody::WeroReverify::Healed => {}
                    lost => {
                        log::error!(
                            "S9 custody sweep: the data-plane WERO hold is gone ({lost:?}) — \
                             custody is poisoned and no dead-epoch preempt can be issued from \
                             a fence this mount no longer holds"
                        );
                        continue;
                    }
                }
            }
            for dead in owner.expire_due() {
                log::warn!(
                    "S9: swept co-writer '{}' (lease epoch {}) — {}; {} offset(s) quarantined \
                     under {}",
                    dead.client,
                    dead.lease_epoch,
                    dead.reason,
                    dead.offsets.len(),
                    dead.epoch
                );
                if dead.offsets.is_empty() {
                    continue;
                }
                let Some(hold) = wero.clone() else {
                    continue;
                };
                if dead.pr_key == 0 {
                    log::warn!(
                        "S9: dead co-writer '{}' published no NVMe registrant key, so no \
                         preempt can prove it drained — its {} offset(s) stay quarantined until \
                         recovery attests (honestly-unavailable space, never a reallocation)",
                        dead.client,
                        dead.offsets.len()
                    );
                    continue;
                }
                let victim = dead.pr_key;
                // Rung-9 own-key guard: a CO-LOCATED co-writer's adopted
                // fenceability key IS this authority's own (shared PR
                // arbitration domain — the ops.md honest residual), and
                // preempting it would take down the authority's own fence.
                // Same-host death is the dead-pid ladder's proof, never a
                // PR preempt; its offsets stay quarantined until recovery
                // attests, exactly like the key-less arm above.
                if victim == hold.key() {
                    log::warn!(
                        "S9: dead co-writer '{}' is fenced by THIS authority's own key \
                         {victim:#x} (the co-located adopted shape) — no preempt can prove it \
                         drained without destroying our own fence; its {} offset(s) stay \
                         quarantined until recovery attests",
                        dead.client,
                        dead.offsets.len()
                    );
                    continue;
                }
                let landed =
                    squeezefs_ipc::sqz_blocking::run_blocking(move || hold.preempt(victim)).await;
                match data_grant::DrainProof::preempt_landed(landed) {
                    Some(proof) => {
                        owner.release_dead(&dead, proof);
                    }
                    None => log::error!(
                        "S9: the WERO preempt of dead co-writer '{}' (key {:#x}) landed on ZERO \
                         namespaces — no drain proof exists, so its offsets stay quarantined",
                        dead.client,
                        victim
                    ),
                }
            }
        }
    })
}
