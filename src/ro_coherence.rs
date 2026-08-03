//! **DLM stage S5 — reader coherence** (pre-RC engineering spec §6.8
//! items 2-seam, 4 and 5; §6.9 S5; execution-plan Phase 4 / D8).
//!
//! "One writer plus N coherent readers requires no distributed lock
//! manager. **Readers take no leases.**" (§6.8.) What it does require is a
//! bounded-cadence revalidation of everything a reader caches, and a
//! purge when that revalidation observes the writer moving. This module is
//! the reader's side of that: the cadence, the trigger, the purge, and the
//! seam the node-cache revalidation arm plugs into.
//!
//! # The three pieces, and which of them ships here
//!
//! | §6.8 item | Where |
//! |---|---|
//! | 1 — the read-only mount mode | `KvMetaBackend::open_read_only`, `fuse_client::read_only_mount`, the allocator/reclaim gates |
//! | **2 — node-cache revalidation** | **NOT here.** The hard, weeks-scale item (`feat/mw-node-cache-coherence`). This module owns the *driver* and calls it through [`NodeCacheRevalidate`] |
//! | 3 — the freed-offset grace period | **not taken** — see the §Bounded-vs-eliminated note below |
//! | 4 — TTL alignment | `fuse_client::{KernelCacheTtls::read_only_defaults, reader_daemon_cache_ttl}`, driven by [`checkpoint_cadence`] |
//! | **5 — purge on revalidation** | [`purge_reader_block_keys`], triggered by [`revalidate_volume`] |
//! | 6 — reader-side data-plane lockdown | [`arm_reader_data_plane`] + the latch gates |
//!
//! # The revalidation pass
//!
//! §6.8 item 2's "cheapest credible design", verbatim: *poll the A/B root
//! ledger (a 4 KiB read) at a bounded cadence and drop every cached node
//! not covered by the new roots.* [`revalidate_volume`] is the poll half —
//! it reads the newest valid ledger record and reports whether the roots
//! have advanced past the snapshot this mount serves. The drop half is
//! item 2's, because the KV node cache is load-once RAM-authoritative
//! (`node_cache.rs`) and a node can be APPENDED to in place inside its
//! 256 KiB extent, so a cached (addr, seq) entry is not immutable across a
//! writer's bset append — which is exactly why the spec calls it the hard
//! item and not a one-liner.
//!
//! # Bounded vs eliminated staleness (the honest statement — §6.12)
//!
//! The purge converts §6.3's *unbounded* cross-file staleness into
//! staleness bounded by one revalidation interval: a reader can serve
//! bytes it fetched at most one interval ago for an offset the writer has
//! since freed and reallocated to a different file. It does not
//! **eliminate** that window — eliminating it is §6.8 item 3, the
//! freed-offset grace period (refuse to reallocate an offset until every
//! registered reader has acknowledged passing that epoch), which needs
//! epoch-acknowledgement machinery on the `client:` heartbeat and a
//! writer-side allocation quarantine. On a transformed (AEAD) volume the
//! window is loud (tag failure); on a passthrough volume — the default —
//! it is silent. `docs/operations.md` §Read-only coherent mounts states
//! this to operators in those terms, and the RC guarantee table carries
//! the row. Do not describe an S5 reader as "coherent" without the
//! interval.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use crate::cache::TieredCache;
use crate::fuse_client::METRICS;
use crate::meta_backend::kv::backend::KvMetaBackend;

/// One revalidation pass's outcome for one metadata volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReaderEpoch {
    /// The newest valid A/B root-ledger sequence on the device.
    pub ledger_seq: u64,
    /// The sequence this mount's bootstrap read (the snapshot it serves).
    pub mounted_seq: u64,
    /// The writer has checkpointed past our snapshot: caches keyed on
    /// anything derived from it are now, in principle, stale.
    pub roots_advanced: bool,
    /// Whether the node-cache revalidation arm (§6.8 item 2) ran. `false`
    /// means no revalidator is installed and this mount is serving a
    /// point-in-time metadata snapshot — see the module docs.
    pub node_cache_revalidated: bool,
}

/// **The §6.8 item-2 seam.** The node-cache revalidation arm
/// (`feat/mw-node-cache-coherence`) installs an implementation with
/// [`install_node_cache_revalidator`]; the reader cadence below calls it
/// once per pass whose roots advanced.
///
/// Contract expected of an implementation: given the volume and the newest
/// ledger sequence just observed, drop every cached node not covered by
/// the new roots and re-root the reader's trees, returning the number of
/// nodes dropped. It runs on the revalidation task (never on a handler
/// lane), takes no DLM lease (readers take none — §6.8), and must be safe
/// to call concurrently with reads in flight.
pub trait NodeCacheRevalidate: Send + Sync {
    fn revalidate_to_newest_roots(&self, volume: &KvMetaBackend, ledger_seq: u64) -> u64;
}

static NODE_CACHE_REVALIDATOR: std::sync::OnceLock<Arc<dyn NodeCacheRevalidate>> =
    std::sync::OnceLock::new();

/// Install the node-cache revalidation arm (§6.8 item 2). Idempotent-ish:
/// the FIRST installation wins, and a second one is reported rather than
/// silently dropped.
pub fn install_node_cache_revalidator(arm: Arc<dyn NodeCacheRevalidate>) {
    if NODE_CACHE_REVALIDATOR.set(arm).is_err() {
        log::warn!(
            "a node-cache revalidation arm was already installed — keeping the first \
             (spec §6.8 item 2)"
        );
    }
}

/// Whether the item-2 arm is present. A reader mount logs this LOUDLY at
/// arm: without it the metadata view is frozen at mount time, which is an
/// honest posture but not the documented one-checkpoint-interval lag.
pub fn node_cache_revalidation_available() -> bool {
    NODE_CACHE_REVALIDATOR.get().is_some()
}

/// The writer's journal/checkpoint cadence — the interval a reader's view
/// can be behind by, and therefore the derivation source for every reader
/// TTL (§6.8 item 4) and for the revalidation interval itself.
pub fn checkpoint_cadence() -> Duration {
    Duration::from_millis(crate::meta_backend::resolve_flush_interval_ms())
}

/// One revalidation pass over one volume: read the newest valid A/B root
/// ledger record (one 4 KiB device read — §6.8 item 2's "cheapest credible
/// design") and report whether the roots advanced past our snapshot.
///
/// Errors are the device's, never a refusal: a failed poll leaves the
/// reader on its current snapshot and is retried next pass.
pub async fn revalidate_volume(volume: &KvMetaBackend) -> crate::error::Result<ReaderEpoch> {
    METRICS.ro_revalidate_passes.fetch_add(1, Ordering::Relaxed);
    let ledger_base = volume.superblock().root_ledger.start;
    let newest =
        crate::meta_backend::kv::checkpoint::read_newest_ledger(volume.device_path(), ledger_base)
            .await
            .map_err(|e| {
                crate::error::SqueezefsError::InvalidOperation(format!(
                    "reader revalidation: root-ledger poll failed on {}: {e}",
                    volume.device_path().display()
                ))
            })?;
    let mounted_seq = volume.mounted_ledger().seq;
    let ledger_seq = newest.map(|l| l.seq).unwrap_or(mounted_seq);
    let roots_advanced = ledger_seq > mounted_seq;
    if roots_advanced {
        METRICS.ro_revalidate_epochs.fetch_add(1, Ordering::Relaxed);
    }
    let node_cache_revalidated = match NODE_CACHE_REVALIDATOR.get() {
        Some(arm) if roots_advanced => {
            let dropped = arm.revalidate_to_newest_roots(volume, ledger_seq);
            METRICS
                .ro_node_cache_nodes_dropped
                .fetch_add(dropped, Ordering::Relaxed);
            true
        }
        _ => false,
    };
    Ok(ReaderEpoch {
        ledger_seq,
        mounted_seq,
        roots_advanced,
        node_cache_revalidated,
    })
}

/// **§6.8 item 5 — purge on revalidation.** "Where the codebase is best
/// prepared: `purge_block_key` is one call covering all five block-key
/// stores, with a grep-guard test preventing a sixth from being forgotten.
/// The invalidation primitive already exists and is complete; only the
/// remote trigger is missing." This is the trigger.
///
/// Why the whole census and not a scoped set: a reader cannot know WHICH
/// offsets the writer freed and reallocated — that attribution is exactly
/// what the durable block-reference tree could answer for a writer and
/// what item 3's epoch acknowledgement would bound for a reader. So the
/// pass drops every block key this mount has cached, through the ONE legal
/// purge, and the read path refetches. The cost is paid only on passes
/// that observed the roots advance (an idle writer costs a reader nothing),
/// and the interval derives from the checkpoint cadence.
///
/// Returns the number of keys purged (`ro_purged_block_keys`) — the
/// engagement instrument: zero on every pass while a writer is committing
/// means the trigger is not reaching the stores.
pub fn purge_reader_block_keys(cache: &TieredCache) -> u64 {
    // The three key-addressed stores that can ENUMERATE. `purge_block_key`
    // then covers all five for each key (read LRU, hot tier, read-lane
    // hold, NVMe read cache, GDS cache) — the R-6 law.
    let mut keys = cache.read_lru.keys();
    keys.extend(cache.hot_block.keys());
    keys.extend(cache.nvme.list_cached_blocks());
    keys.sort_unstable();
    keys.dedup();
    for key in &keys {
        cache.purge_block_key(key);
    }
    // The read-lane hold is deliberately ledger-INVISIBLE (no key census
    // exists, by design — `src/read_lane.rs`), so it is dropped whole
    // through its own budget clamp rather than key by key. A held fill
    // that survived a revalidation epoch is exactly the stale serve this
    // pass exists to prevent.
    cache.read_lane_hold.trim_to(0);
    let n = keys.len() as u64;
    METRICS
        .ro_purged_block_keys
        .fetch_add(n, Ordering::Relaxed);
    n
}

/// **§6.8 item 6 — reader-side data-plane lockdown**, the arming half.
///
/// The latch (`fuse_client::set_read_only_mount`) refuses every mutation
/// at its chokepoint; this additionally LATCHES the reclaim queue halted,
/// so even a straggler enqueue from a racing teardown can never issue a
/// destructive `BLKDISCARD`/`PUNCH_HOLE` against a range the writer owns
/// (§6.3's reclaim/discard hazard — the one reader failure mode that
/// destroys data instead of reading it stale).
pub fn arm_reader_data_plane(router: &crate::routing::DataRouter) {
    router.backend_router.reclaim_cease();
    log::warn!(
        "reader data plane armed (DLM S5): block allocation, terminal frees, device \
         reclaim, the W1 in-place patch and in-place overwrites are all refused on this \
         mount; the reclaim queue is latched halted"
    );
    if !node_cache_revalidation_available() {
        log::warn!(
            "reader coherence: the node-cache revalidation arm (pre-RC engineering spec \
             §6.8 item 2) is NOT installed in this build. This mount serves the metadata \
             snapshot its bootstrap read and will not observe the writer's new files, \
             renames or size changes; cached DATA blocks are still dropped on every \
             revalidation epoch that sees the roots advance. See docs/operations.md \
             §Read-only coherent mounts."
        );
    }
}

/// The reader's revalidation cadence task: one pass per volume per
/// interval, purging the block-key stores on every volume whose ledger
/// sequence MOVED since the last pass.
///
/// Cadence = [`crate::fuse_client::reader_revalidate_interval`] over the
/// checkpoint cadence (never a constant — the derivation law), so the poll
/// can never run faster than the roots can advance.
///
/// The task holds the volumes it polls (a reader's whole point is to keep
/// serving), and exits when `stop` fires — the dismount path.
pub fn spawn_reader_revalidation(
    volumes: Vec<Arc<KvMetaBackend>>,
    router: crate::routing::DataRouter,
    stop: Arc<tokio::sync::Notify>,
) -> tokio::task::JoinHandle<()> {
    let interval = crate::fuse_client::reader_revalidate_interval(checkpoint_cadence());
    log::info!(
        "reader revalidation armed: {} volume(s), every {:?} (the writer's checkpoint \
         cadence — one 4 KiB root-ledger read per volume per pass)",
        volumes.len(),
        interval
    );
    tokio::spawn(async move {
        let mut last_seen: Vec<u64> = volumes.iter().map(|v| v.mounted_ledger().seq).collect();
        loop {
            tokio::select! {
                _ = stop.notified() => {
                    log::info!("reader revalidation stopping (dismount)");
                    return;
                }
                _ = tokio::time::sleep(interval) => {}
            }
            let mut moved = false;
            for (i, vol) in volumes.iter().enumerate() {
                match revalidate_volume(vol.as_ref()).await {
                    Ok(epoch) => {
                        if epoch.ledger_seq != last_seen[i] {
                            last_seen[i] = epoch.ledger_seq;
                            moved = true;
                        }
                    }
                    Err(e) => log::warn!("reader revalidation pass failed: {e}"),
                }
            }
            if moved {
                // One purge per pass, not per volume: the stores are
                // mount-wide and the keys are not volume-attributable
                // without the layout walk this design exists to avoid.
                let purged = purge_reader_block_keys(&router.cache);
                log::debug!(
                    "reader revalidation epoch: roots moved, {purged} cached block key(s) \
                     dropped"
                );
            }
        }
    })
}
