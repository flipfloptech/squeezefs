//! **DLM stage S5 — reader coherence, the mount side** (pre-RC engineering
//! spec §6.8 items 4, 5, 6 and the wiring of item 2; §6.9 S5;
//! execution-plan Phase 4 / D8).
//!
//! "One writer plus N coherent readers requires no distributed lock
//! manager. **Readers take no leases.**" (§6.8.) What it does require is a
//! bounded-cadence revalidation of everything a reader caches, and a purge
//! when that revalidation observes the writer moving. This module is the
//! MOUNT's side of that: it arms the reader, drives the cadence, owns the
//! data-plane lockdown, and provides the R-6 purge sink.
//!
//! # The division of labour, post-wiring
//!
//! | §6.8 item | Where |
//! |---|---|
//! | 1 — the read-only mount mode | `KvMetaBackend::open_read_only`, `fuse_client::read_only_mount`, the allocator/reclaim gates |
//! | **2 — node-cache revalidation** | [`crate::meta_backend::kv::revalidate`] (the epoch protocol, root adoption, the drop pass, the derived cadence) — **this module is its driver**: [`crate::ro_coherence::arm_reader_coherence`] declares each volume a reader, [`crate::ro_coherence::spawn_reader_revalidation`] is the task its deliberately task-less `RevalidationPoller` expects |
//! | 3 — the freed-offset grace period | **not built** — see §Bounded-vs-eliminated below |
//! | 4 — TTL alignment | [`crate::ro_coherence::reader_staleness_bound`] feeds `fuse_client::{KernelCacheTtls::read_only_defaults, reader_daemon_cache_ttl}` |
//! | **5 — purge on revalidation** | [`crate::ro_coherence::ReaderEpochPurge`] — the `EpochPurgeSink` the mount installs; body [`crate::ro_coherence::purge_reader_block_keys`] |
//! | 6 — reader-side data-plane lockdown | [`crate::ro_coherence::arm_reader_data_plane`] + the latch gates |
//!
//! # The consistency model, in one paragraph
//!
//! A reader serves the metadata state of the most recent checkpoint it has
//! polled. Staleness is **bounded by [`crate::ro_coherence::reader_staleness_bound`]** (the poll
//! interval plus the writer's ≤ 1 s checkpoint ceiling) and monotone
//! (epochs only advance). Every epoch step drops the cached nodes the new
//! roots do not cover AND fires the R-6 purge over the reader's block-key
//! census. Full statement, including what is *not* promised (durability,
//! linearizability, one epoch across a multi-key operation):
//! `crate::meta_backend::kv::revalidate`'s module docs and
//! `docs/operations.md` §Read-only coherent mounts.
//!
//! # Bounded vs eliminated staleness (the honest statement — §6.12)
//!
//! The purge converts §6.3's *unbounded* cross-file staleness into
//! staleness bounded by one poll interval: a reader can serve bytes it
//! fetched at most one interval ago for an offset the writer has since
//! freed and reallocated to a different file. It does not **eliminate**
//! that window — eliminating it is §6.8 item 3, the freed-offset grace
//! period, which is **NOT implemented**. Full assessment:
//! `.benchmarks/2026-08-05-dlm-s5-readonly-mount.md` §4. The short version,
//! because it is a structural finding rather than a scheduling choice: the
//! spec's mechanism rides the `client:` heartbeat, and a reader **cannot
//! write that record** — it is an xattr commit on ino 1 under an exclusive
//! `I{1}` guard, i.e. a metadata write, which item 1 refuses by contract
//! (and §6.5 pt 3 already measures that plane saturating at ~4,550
//! clients). Item 3 therefore needs a reader→writer acknowledgement channel
//! that is not a metadata write (S6 membership) plus a writer-side
//! epoch-keyed quarantine between `begin_free` and `finish_free`. Its
//! release condition did get cheaper this wave: durable block refcounts
//! (§6.2 item 1) answer "is this block still referenced?" as a prefix
//! population count instead of an inode-tree walk. On a transformed (AEAD)
//! volume the window is loud (tag failure); on a passthrough volume — the
//! default — it is silent. `docs/operations.md` states this to operators in
//! those terms, and the RC guarantee table carries the row. Do not describe
//! an S5 reader as "coherent" without the bound.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cache::TieredCache;
use crate::meta_backend::kv::backend::KvMetaBackend;
use crate::meta_backend::kv::node_cache::EpochPurgeSink;
use crate::meta_backend::kv::revalidate::RevalidationPoller;

/// The reader's poll cadence — the landed derivation
/// (`revalidate::resolve_revalidate_interval_ms`: `max(writer flush
/// cadence, CHECKPOINT_MAX_AGE_MS)`, strict mode reading as the checkpoint
/// task's own tick, env override verbatim). Never a constant here: polling
/// faster than the writer's checkpoint guarantee cannot reduce staleness
/// (records do not exist to be found) and pays a drop pass for it.
pub fn reader_revalidate_interval() -> Duration {
    RevalidationPoller::derived().interval()
}

/// **The stated staleness bound** — poll interval + the writer's ≤ 1 s
/// checkpoint ceiling, straight from the machinery in force
/// (`RevalidationPoller::staleness_bound`), so the number an operator reads
/// on the stats inode and the number in `docs/operations.md` cannot drift
/// from the number the reader actually honours.
///
/// This is the derivation source for every reader-side TTL (§6.8 item 4):
/// a kernel or daemon cache may not hold an entry longer than the interval
/// over which the reader can prove freshness.
pub fn reader_staleness_bound() -> Duration {
    RevalidationPoller::derived().staleness_bound()
}

/// **§6.8 item 5 — purge on revalidation.** "Where the codebase is best
/// prepared: `purge_block_key` is one call covering all five block-key
/// stores, with a grep-guard test preventing a sixth from being forgotten.
/// The invalidation primitive already exists and is complete; only the
/// remote trigger is missing." This is the trigger's body; the trigger
/// itself is [`ReaderEpochPurge`], fired by the node cache's epoch step.
///
/// Why the whole census and not a scoped set: a reader cannot know WHICH
/// offsets the writer freed and reallocated — that attribution is exactly
/// what the durable block-reference tree answers for a writer and what item
/// 3's epoch acknowledgement would bound for a reader. So the pass drops
/// every block key this mount has cached, through the ONE legal purge, and
/// the read path refetches. The cost is paid only on an epoch step (an idle
/// writer costs a reader nothing) and is priced in
/// `.benchmarks/2026-08-05-dlm-s5-readonly-mount.md` §2.
///
/// Returns the number of keys purged — it rides the epoch outcome as
/// `meta_kv_revalidate_keys_purged`, the trigger's engagement instrument.
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
    keys.len() as u64
}

/// The `EpochPurgeSink` an S5 **mount** installs (spec §6.8 item 5).
///
/// Why this and not `revalidate::TieredEpochPurge`: that sink purges the
/// keys a data-path site *registered* through `note_suspect`, and **no such
/// registration site is built** — so installing it on a mount would leave
/// `meta_kv_revalidate_keys_purged` reading 0 while
/// `meta_kv_revalidate_epochs` climbed, i.e. a coherence promise that is
/// silently not kept, which is the worst failure mode available here. This
/// sink runs the complete-but-unscoped census pass instead: never silent,
/// never wrong, just more work than a scoped purge would be. When the
/// registration site lands, the scoped drain becomes the fast path and this
/// census becomes its fallback — one call site to change.
///
/// It holds the `DataRouter` (a cheap `Arc` clone) rather than an
/// `Arc<TieredCache>`, because the mount's tier cache lives *inside* the
/// router and cannot be handed out as its own `Arc`.
pub struct ReaderEpochPurge {
    router: crate::routing::DataRouter,
}

impl std::fmt::Debug for ReaderEpochPurge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Neither `DataRouter` nor `TieredCache` is `Debug` (device
        // handles); the sink carries no state of its own to print.
        f.write_str("ReaderEpochPurge(census)")
    }
}

impl ReaderEpochPurge {
    pub fn new(router: crate::routing::DataRouter) -> Arc<Self> {
        Arc::new(Self { router })
    }
}

impl EpochPurgeSink for ReaderEpochPurge {
    fn on_epoch_advance(&self, from_epoch: u64, to_epoch: u64) -> u64 {
        let purged = purge_reader_block_keys(&self.router.cache);
        log::debug!(
            "reader epoch {from_epoch} → {to_epoch}: {purged} cached block key(s) dropped \
             (R-6 unified purge)"
        );
        purged
    }
}

/// **§6.8 item 6 — reader-side data-plane lockdown**, the arming half.
///
/// The latch (`fuse_client::set_read_only_mount`) refuses every mutation at
/// its chokepoint; this additionally LATCHES the reclaim queue halted, so
/// even a straggler enqueue from a racing teardown can never issue a
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
}

/// **§6.8 item 2's declaration** — make every mounted volume a *coherent
/// reader* and install the R-6 purge trigger.
///
/// One `arm_reader_revalidation` per volume, at the ledger record the mount
/// opened (so nothing is dropped by the arming itself). Arming is what
/// makes the poll legal at all: `revalidate_reader` refuses on an un-armed
/// mount, because adopting a ledger's roots on a mount whose own SMOs may
/// have moved past them is time travel. It is also what closes the writer
/// half — from here on the node layer refuses every node mutation loudly
/// (`meta_kv_node_partition_refusals`), a second, structural line of
/// defence behind the metadata write gate and the data-plane latch.
///
/// Returns the number of volumes armed. A refusal on one volume is loud but
/// never fatal: that volume keeps serving its mount-time snapshot (a
/// *frozen* view is stale, not wrong), and its polls will refuse loudly
/// too, so the condition cannot hide.
pub fn arm_reader_coherence(
    volumes: &[Arc<KvMetaBackend>],
    router: &crate::routing::DataRouter,
) -> usize {
    let sink = ReaderEpochPurge::new(router.clone());
    let mut armed = 0usize;
    for vol in volumes {
        match vol.arm_reader_revalidation(Some(sink.clone())) {
            Ok(()) => armed += 1,
            Err(e) => log::error!(
                "meta volume {}: reader revalidation could NOT be armed: {e}. This volume \
                 will serve its mount-time metadata snapshot and never observe the \
                 writer's checkpoints — remount to advance it (spec §6.8 item 2)",
                vol.device_path().display()
            ),
        }
    }
    log::info!(
        "reader coherence armed on {armed}/{} volume(s): staleness bound {:?} \
         (poll {:?} + the writer's ≤1 s checkpoint ceiling), R-6 purge sink installed",
        volumes.len(),
        reader_staleness_bound(),
        reader_revalidate_interval(),
    );
    armed
}

/// The reader's revalidation cadence task — the **driver** for the landed
/// [`RevalidationPoller`], which deliberately owns no task of its own
/// ("the RO mount's own loop calls `poll_at`", `revalidate.rs`).
///
/// Per pass, per volume: one ledger read; on a newer record, root adoption
/// then the drop pass then the R-6 purge — in that order, which is the
/// landed contract (roots are adopted before the epoch is published
/// `Release`, so no traversal can mix a new root with a stale node).
/// Nothing in this loop re-implements any of it; the poller owns "is a poll
/// due", the cache owns the epoch step, and this task owns only *when to
/// ask*.
///
/// Stop discipline: `stop_flag` is the AUTHORITY (the mount's
/// `dismount_once` latch, checked once per pass) and `wake` is only a
/// promptness hint. A `Notify::notify_waiters()` reaches only tasks already
/// parked on it, so a notify that fires between two of this loop's
/// `notified()` registrations is LOST — using it as the authority would
/// strand the task for the process's life. Detached-panic accounting rides
/// `detached::contain` (RES-8: nothing joins this task, so
/// `detached_task_panics` is the only record if it unwinds).
pub fn spawn_reader_revalidation(
    volumes: Vec<Arc<KvMetaBackend>>,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
    wake: Arc<tokio::sync::Notify>,
) -> tokio::task::JoinHandle<()> {
    let poller = RevalidationPoller::derived();
    let interval = poller.interval();
    log::info!(
        "reader revalidation armed: {} volume(s), every {:?} (the derived cadence — one \
         ledger read per volume per pass; staleness bound {:?})",
        volumes.len(),
        interval,
        poller.staleness_bound(),
    );
    tokio::spawn(crate::detached::contain(
        "reader_revalidation",
        async move {
            loop {
                tokio::select! {
                    _ = wake.notified() => {}
                    _ = tokio::time::sleep(interval) => {}
                }
                if stop_flag.load(Ordering::Acquire) {
                    log::info!("reader revalidation stopping (dismount)");
                    return;
                }
                for vol in &volumes {
                    match poller.poll_at(vol, Instant::now()).await {
                        // Not due yet (a wake arrived early) — nothing to do.
                        Ok(None) => {}
                        Ok(Some(out)) if out.advanced => log::debug!(
                            "reader revalidation: {} epoch {} → {} ({} node(s) dropped, {} \
                         retained, {} block key(s) purged)",
                            vol.device_path().display(),
                            out.from_epoch,
                            out.epoch,
                            out.dropped,
                            out.retained,
                            out.keys_purged,
                        ),
                        // The inert poll — the common case under an idle
                        // writer, and deliberately free (no drop, no purge).
                        Ok(Some(_)) => {}
                        Err(e) => log::warn!(
                            "reader revalidation pass failed on {}: {e} (the reader keeps \
                         serving its current epoch and retries next pass)",
                            vol.device_path().display()
                        ),
                    }
                }
            }
        },
    ))
}
