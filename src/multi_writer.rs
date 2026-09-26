//! The **writer planes** — metadata ownership (S8), the data-plane custody
//! fence and its device-enforced WERO hold (S7), and the remote
//! write-custody plane ([`crate::data_grant`]) as a coherent whole. Since
//! PR 14 (the symmetric default flip) the one caller is the symmetric JOIN
//! LADDER ([`crate::sym_join`]): every RW mount of a set walks it and stands
//! up exactly these planes on ONE listener — the declared
//! `SQUEEZEFS_MULTI_WRITER=1` arm, the co-writer client posture and the
//! per-volume-owner recipe retired with it (1.3.0).
//!
//! # Why the arm is one function
//!
//! S8 declined to arm ownership and said why: *"a mount that ships metadata
//! but cannot ship data custody is not a product."* The converse is equally
//! true — a mount that grants remote data custody but publishes layouts
//! locally would append to a tree whose journal ring, extent bitmap and
//! root ledger belong to another writer. The three planes are only sound
//! **together**, so there is exactly one place that turns them on.
//!
//! The ladder's rungs (the substrate posture, the bind, the durable era,
//! the cluster secret) are decided by the join ladder under its own law;
//! `arm_authority_planes` refuses loud on what it is handed missing.

use crate::data_custody::{self, WeroHold};
use crate::data_grant::{self, AsyncVerbRouter, CustodyQuarantine, WriteCustodyOwner};
use crate::error::{Result, SqueezefsError};
use crate::membership::{LeaseClock, LeaseClocks};
use crate::meta_backend::kv::superblock as sb;
use crate::meta_backend::RoutedMetaBackend;
use crate::meta_ship::{publish, OwnerMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Where this mount serves the custody + publish authority
/// (`SQUEEZEFS_MW_BIND`).
pub const MW_BIND_ENV: &str = "SQUEEZEFS_MW_BIND";

/// The `features_incompat` bits the multi-writer CLASS requires (the join
/// ladder's rung 2 demands them beside bit 17 — [`crate::sym_join::
/// required_bits`]), and the reason each one is not optional:
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
                    crate::shipped_free::with_authority_accounting(router.free_block(&key)).await;
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

/// `resolve_bind` for the symmetric join ladder's rung 7
/// (`crate::sym_join`): `Ok(Some(addr))` = where to serve (`auto` = ruling
/// D2's `0.0.0.0:0`), `Ok(None)` = an explicit `off` (the ladder refuses
/// it under its own text), `Err` = a malformed address.
pub fn resolve_bind_public() -> Result<Option<std::net::SocketAddr>> {
    Ok(match resolve_bind()? {
        Bind::Off => None,
        Bind::Auto => Some("0.0.0.0:0".parse().expect("literal addr")),
        Bind::Addr(a) => Some(a),
    })
}

/// `SQUEEZEFS_DELEGATION` set on a mount whose ownership plane is not armed
/// (a `--single-writer` volume) is ANNOUNCED-INERT — a startup notice,
/// never a refusal (rung 12, design §11): the S10 delegation lever engages
/// only on an armed plane.
pub fn announce_delegation_inert() {
    if std::env::var(crate::meta_ship::DELEGATION_ENV)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        log::info!(
            "SQUEEZEFS_DELEGATION is set but no ownership plane is armed on this mount — the \
             S10 delegation lever is INERT here (every dlm_delegation gauge stays 0 by \
             construction); it engages only on mounts whose ownership plane is armed"
        );
    }
}

/// The symmetric join ladder's binding half (`crate::sym_join`, §5.1.6):
/// publish this writer's S8 listener into its own claim-set entry on
/// EVERY volume of the set (a symmetric map is all-local), so any mount
/// resolves this appender's endpoint off durable state. Idempotent at the
/// same address; a volume that does not enroll this node is announced.
pub async fn publish_symmetric_endpoint(meta: &Arc<RoutedMetaBackend>, endpoint: &str) {
    let Ok(node_id) = crate::member_id::node_member_id() else {
        return;
    };
    publish_owner_endpoint(meta, &node_id, endpoint, crate::dlm::durable_term()).await;
}

/// The ownership map a SYMMETRIC writer arms its planes over — all-local
/// (`OwnerMap::for_volumes` with no peers).
pub async fn derive_symmetric_ownership(meta: &Arc<RoutedMetaBackend>) -> Result<Arc<OwnerMap>> {
    derive_ownership(meta).await
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
        crate::meta_ship::uninstall_delegation_host();
        // Rung 14: the placement policy's vehicle dies with the authority
        // (its runtime state dies inside disarm_ownership).
        crate::meta_ship::placement::uninstall_migration_executor();
        crate::meta_ship::disarm_ownership();
        // The shipped-free EXECUTOR dies with the authority too: a served
        // free with no executor refuses loud rather than stranding half a
        // ladder on a disarmed mount — and rung 17's extent ASSEMBLER pair
        // (a served extent with no assembler refuses loud rather than
        // acking bytes nobody merges).
        publish::uninstall_free_executor();
        publish::uninstall_released_block_probe();
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

/// The authority planes proper — everything after the substrate and the
/// bind are decided: the durable era, the custody owner, the
/// S8/publish/manager/token services on ONE listener, the ownership plane
/// and the cadence. The symmetric JOIN LADDER ([`crate::sym_join`], PR 12)
/// decides the substrate posture and the bind under its own law and then
/// stands up exactly these planes on every armed writer.
pub(crate) async fn arm_authority_planes(
    meta: &Arc<RoutedMetaBackend>,
    wero: Option<WeroHold>,
    bind: std::net::SocketAddr,
    quarantine: Option<Arc<dyn CustodyQuarantine>>,
    backend: Option<&Arc<crate::routing::BackendRouter>>,
    map: Arc<OwnerMap>,
) -> Result<Option<MultiWriterArm>> {
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

    // The shipped-free EXECUTOR: a peer's displaced-block terminal frees
    // travel as publish verbs to the data volume's allocation-lease holder
    // (PR 8, `block_grant::free_target_for`), and this is the holder half
    // that runs the full ladder — RAM release, tier purge, reclaim
    // enqueue, `finish_free` with the grace ring and the quarantine
    // composing inside, ending in the bitmap CLEAR on a grant-armed
    // allocator — against THIS mount's data plane.
    if let Some(backend) = backend {
        publish::install_free_executor(crate::shipped_free::router_free_executor(
            Arc::clone(backend),
            Arc::clone(meta),
        ));
        // The served-publish SCREEN (design-small-file-packing §5.6 (2)): a
        // peer's layout publish that would ADOPT a block this mount has
        // RELEASED (free list / grace ring / quarantine) is refused before
        // anything is staged — the belt under every served publish.
        publish::install_released_block_probe(crate::shipped_free::router_released_block_probe(
            Arc::clone(backend),
        ));
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
    // A JOINED appender (PR 12b) serves NO manager verb: its listener
    // carries its slots' tokens, custody and shipped steps; the manager
    // verbs are page 0's daemon's, and a peer dialing them here would
    // meet `refuse_joined_control` (the must-stay-0 class) instead of the
    // wire's own "no such verb".
    if meta
        .volumes
        .iter()
        .any(|v| v.slot_lease_armed() && !v.is_joined_appender())
    {
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
    let endpoint = crate::cluster_wire::advertised_endpoint(bind, listener.endpoint().port());
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
    publish::install_client(publish::PublishClient::new(owner.id(), secret));

    // The ownership plane — all-local under the symmetric plane (ownership
    // is the slot lease; `mint_redirects` stays a structural 0).
    crate::meta_ship::arm_ownership(Arc::clone(&map));
    // Rung 14 (client-owned-slot placement): the policy's migration
    // vehicle — the existing online migrate-meta-slot engine over this
    // authority's own set, re-arming the ownership map at cutover (the
    // migration-while-armed law). On an all-local map the policy's
    // candidate inversion never fires, so installing the vehicle is
    // reachability, not activity.
    crate::meta_ship::placement::install_migration_executor(
        crate::meta_ship::placement::authority_migration_executor(Arc::clone(meta)),
    );

    let stop = Arc::new(AtomicBool::new(false));
    spawn_cadence(
        Arc::clone(&owner),
        wero.as_ref().map(WeroHold::downgrade),
        Arc::clone(&stop),
        renew,
    );

    log::warn!(
        "WRITER PLANES ARMED on {endpoint}: era {term}, custody authority '{}', data-plane \
         fence class {}, {} metadata volume(s). Peers may now acquire write custody here and \
         DMA directly to the shared namespaces — only custody travels, never data (spec §6.9 \
         S9)",
        owner.id(),
        data_custody::wero_mode(),
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

/// **Derive this mount's ownership map**: every volume of the set is
/// LOCAL — under the symmetric plane ownership is the slot lease, so the
/// S8 map is all-local by construction and its client half is
/// structurally inert (`dlm_rpcs == 0` on a solo mount).
async fn derive_ownership(meta: &Arc<RoutedMetaBackend>) -> Result<Arc<OwnerMap>> {
    OwnerMap::for_volumes(meta, Vec::new())
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
    node_id: &str,
    endpoint: &str,
    term: u64,
) {
    for vol in meta.volumes.iter() {
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
                 cannot resolve this writer's listener on that volume until a later mount \
                 publishes it",
                vol.device_path().display()
            );
            continue;
        };
        // The registrant key the entry carries is the one the MEMBERSHIP
        // arm read when it enrolled this node — under the symmetric join
        // ladder that is rung 3, BEFORE rung 4 takes the data WERO hold,
        // so the entry read 0 and every co-located joiner's adoption found
        // no enrolled key to cross-check the standing holder against
        // (found by the fidelity tier's first real second daemon). The
        // publish runs at rung 7: the live hold's key is known here.
        let key = crate::data_custody::live_wero_key().unwrap_or(identity.pr_key);
        if identity.endpoint.as_deref() == Some(endpoint) && identity.pr_key == key {
            continue; // idempotent: a remount at the same address writes nothing
        }
        identity.endpoint = Some(endpoint.to_string());
        identity.pr_key = key;
        if let Err(e) = crate::membership::upsert_writer_member(vol, &identity, term).await {
            log::warn!(
                "ownership map: publishing this mount's endpoint {endpoint} on volume {} failed \
                 ({e}) — peers cannot resolve this writer's listener until a later mount \
                 publishes it",
                vol.device_path().display(),
            );
        }
    }
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
    wero: Option<crate::data_custody::WeroHoldRef>,
    stop: Arc<AtomicBool>,
    cadence: Duration,
) {
    crate::meta_exec::spawn_meta_contained("mw_custody_sweep", async move {
        loop {
            squeezefs_ipc::sqz_time::sleep(cadence).await;
            if stop.load(Ordering::Acquire) {
                return;
            }
            // The sweep OBSERVES the hold (a weak reference upgraded per
            // tick): a strong clone here outlived `disarm` by up to one
            // cadence and took the release ioctl with it out of the
            // process — the arm's own reference is the LAST one, so its
            // drop inside `disarm` is what releases the reservation.
            let wero = wero.as_ref().and_then(|w| w.upgrade());
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
