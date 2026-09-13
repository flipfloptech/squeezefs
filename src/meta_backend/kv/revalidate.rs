//! **Reader-side metadata revalidation** — the orchestration half of pre-RC
//! engineering spec §6.8 item 2 (*"a metadata revalidation path for the node
//! cache — the hard item, but much easier for a reader than a writer: no
//! dirty nodes, no SMOs, no pinned interior state to preserve"*), and the
//! prerequisite for §6.9 stage **S5** (one writer + N coherent readers).
//!
//! The design is the one the spec calls the cheapest credible one, verbatim:
//! **poll the A/B root ledger at a bounded cadence and drop every cached
//! node not covered by the new roots.** This module owns the poll, the
//! cadence derivation, the root adoption, and the R-6 purge sink; the cache
//! machinery (epoch stamps, the drop pass, the charge accounting) is
//! [`super::node_cache`], and the two-word publication core is
//! [`super::epoch_core`].
//!
//! ## The API an RO mount consumes
//!
//! ```text
//!  open  ─→ KvMetaBackend::arm_reader_revalidation(purge_sink)    (once)
//!  loop  ─→ RevalidationPoller::poll_at(&backend, Instant::now()) (cadence)
//!            └→ read_root_epoch()  one 128 KiB ledger read
//!            └→ revalidate_trees() root adoption + the drop pass
//! ```
//!
//! Or, for a caller that owns a bare [`NodeCache`] and [`KvTree`] set rather
//! than a backend: [`revalidate_trees`] takes exactly those.
//!
//! The S5 mount is that consumer, and its driver is
//! [`crate::ro_coherence`]: `arm_reader_coherence` performs the arming (once
//! per volume, installing `ReaderEpochPurge` as the sink),
//! `spawn_reader_revalidation` is the task this module's deliberately
//! task-less [`RevalidationPoller`] expects, and the reader's TTLs are
//! derived from [`RevalidationPoller::staleness_bound`].
//!
//! ## The consistency model (the operator-facing statement)
//!
//! A reader serves the metadata state of **the most recent checkpoint it has
//! polled**, and nothing else:
//!
//! * **Bounded staleness.** A write mount writes a ledger record at the end
//!   of every checkpoint cycle that had work, at most
//!   `CHECKPOINT_MAX_AGE_MS` (1 s) apart under load. A reader polls every
//!   [`RevalidationPoller::interval`]. Worst case, a record written just
//!   after a poll is observed at the next one, so the bound is
//!   [`RevalidationPoller::staleness_bound`] = `interval + 1 s`.
//! * **Monotone.** Epochs only advance (`epoch_core`'s CAS), so a reader
//!   never moves backwards and never loses a record it has already served.
//! * **Per-operation atomicity, not per-multi-key-operation.** Each node a
//!   reader resolves belongs to exactly one epoch, and an operation starting
//!   after a poll sees only that epoch's nodes; a `readdir` that spans a
//!   poll may mix two adjacent checkpoints. Callers that need one epoch for
//!   a whole multi-step operation read [`NodeCache::revalidation_epoch`]
//!   before and after, seqlock-style, and retry on a change.
//! * **Not durability, not linearizability.** A reader observes only what
//!   the writer has *checkpointed*: committed-but-not-yet-checkpointed
//!   transactions (up to the flush cadence) are invisible by design. That is
//!   what makes the model cheap — no journal replay on the read side.
//! * **Data blocks are a separate promise.** Metadata coherence does not
//!   make block-key bindings coherent (§6.3): the epoch step fires the R-6
//!   purge trigger, and bounding it exactly requires the §6.8 item-3
//!   freed-offset grace period, which is *not* built.
//!
//! ## Why the cadence is derived, never a constant
//!
//! Polling faster than the writer's checkpoint guarantee cannot reduce
//! staleness — records do not exist to be found — while every advance costs
//! a drop pass and a reload of the working set. So the cadence derives from
//! the same cadence knob the writer's checkpoint task uses, floored at the
//! §4.6 pt 2 checkpoint ceiling: see [`resolve_revalidate_interval_ms`].

use super::backend::KvMetaBackend;
use super::checkpoint::{read_newest_ledger, CHECKPOINT_MAX_AGE_MS};
use super::node_cache::{EpochPurgeSink, NodeCache, RevalidateOutcome, RootEpoch};
use super::tree::{KvTree, RootPtr};
use super::KvError;
use crate::cache::TieredCache;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Absolute override for the derived poll cadence (ms). Explicit wins
/// verbatim, per the standing precedence law (absolute > percentage >
/// derived); the registry entry in `src/env_knobs.rs` owns the admissible
/// range and the loud startup refusal.
pub const REVALIDATE_INTERVAL_ENV: &str = "SQUEEZEFS_META_REVALIDATE_MS";

/// Resolve the reader's poll cadence, in ms.
///
/// Derivation (2026-08-05, the standing "resource caps derive" law applied
/// to a *time* cap): the writer publishes a ledger record at the end of any
/// checkpoint cycle that had work, and its cycle decision is
/// `elapsed ≥ CHECKPOINT_MAX_AGE_MS` OR ring pressure OR the dirty-node cap
/// (`checkpoint.rs::tick`). So records cannot appear more slowly than that
/// ceiling under load, and a reader polling faster buys nothing but drop
/// passes. The derived cadence is therefore
/// `max(effective writer cadence, CHECKPOINT_MAX_AGE_MS)`, where the
/// effective writer cadence is the flush knob with strict mode (0) reading
/// as the checkpoint task's own 100 ms tick — the identical derivation
/// `spawn_checkpoint_task` performs, so the two cannot drift.
///
/// `env` wins verbatim when it parses to a nonzero integer; a malformed
/// value keeps the derivation (the process-startup knob gate is where a bad
/// value refuses loud — no call site may panic on one).
pub fn resolve_revalidate_interval_ms(flush_interval_ms: u64, env: Option<&str>) -> u64 {
    if let Some(raw) = env {
        match raw.trim().parse::<u64>() {
            Ok(ms) if ms > 0 => return ms,
            Ok(_) => log::warn!("{REVALIDATE_INTERVAL_ENV}=0 is not a cadence — ignored"),
            Err(e) => {
                log::warn!("{REVALIDATE_INTERVAL_ENV}={raw:?} is not an integer ({e}) — ignored")
            }
        }
    }
    // Strict mode reads as the checkpoint task's own 100 ms tick — the
    // one derivation the task itself rides (`checkpoint_tick_period_ms`).
    super::checkpoint::checkpoint_tick_period_ms(flush_interval_ms)
        .max(CHECKPOINT_MAX_AGE_MS as u64)
}

/// The bounded-cadence poll driver. Owns no task: the RO mount's own loop
/// (or its checkpoint-task equivalent) calls [`Self::poll_at`], which keeps
/// every test deterministic — the clock is a parameter, never a sleep.
#[derive(Debug)]
pub struct RevalidationPoller {
    interval: Duration,
    last: std::sync::Mutex<Option<Instant>>,
}

impl RevalidationPoller {
    /// A poller with an explicit interval (ms).
    pub fn new(interval_ms: u64) -> Self {
        Self {
            interval: Duration::from_millis(interval_ms.max(1)),
            last: std::sync::Mutex::new(None),
        }
    }

    /// A poller on the derived cadence (reads the env override once).
    pub fn derived() -> Self {
        Self::new(resolve_revalidate_interval_ms(
            crate::meta_backend::resolve_flush_interval_ms(),
            std::env::var(REVALIDATE_INTERVAL_ENV).ok().as_deref(),
        ))
    }

    /// The cadence in force.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// **The stated staleness bound**: this poller's interval plus the
    /// writer's checkpoint ceiling — a record written just after a poll is
    /// observed at the next one. Machine-readable on purpose, so the number
    /// in `docs/operations.md` cannot drift from the number in force.
    pub fn staleness_bound(&self) -> Duration {
        self.interval + Duration::from_millis(CHECKPOINT_MAX_AGE_MS as u64)
    }

    /// Whether a poll is due at `now`.
    pub fn due_at(&self, now: Instant) -> bool {
        match *self.last.lock().unwrap_or_else(|p| p.into_inner()) {
            None => true,
            Some(prev) => now.duration_since(prev) >= self.interval,
        }
    }

    /// Record a poll at `now` (the test seam and [`Self::poll_at`]'s own
    /// bookkeeping).
    pub fn mark(&self, now: Instant) {
        *self.last.lock().unwrap_or_else(|p| p.into_inner()) = Some(now);
    }

    /// Poll if due: one ledger read plus, when the record is newer, root
    /// adoption and the drop pass. `Ok(None)` = not due yet.
    ///
    /// This is the ONE-volume form (its `mark` is per-poller state); a
    /// reader mounted over a volume SET drives [`Self::poll_set_at`].
    pub async fn poll_at(
        &self,
        be: &Arc<KvMetaBackend>,
        now: Instant,
    ) -> Result<Option<RevalidateOutcome>, KvError> {
        if !self.due_at(now) {
            return Ok(None);
        }
        self.mark(now);
        be.revalidate_reader().await.map(Some)
    }

    /// One revalidation pass over a reader's volume SET — the composition
    /// `crate::ro_coherence::spawn_reader_revalidation` drives once per
    /// cadence tick. **One cadence decision for the whole pass**: due is
    /// checked ONCE and marked ONCE, then every volume is polled.
    ///
    /// Why this cannot be a per-volume [`Self::poll_at`] loop (rung-6 fleet
    /// finding #2, 2026-08-15): the cadence mark (`last`) is per-POLLER
    /// state, so on a shared poller the first volume's `mark` made
    /// `due_at(pass_start)` false for every sibling on the same pass — and
    /// every subsequent pass repeated the pattern. Only meta volume 0 ever
    /// revalidated on a multi-volume set: the parent's dentry tree advanced
    /// while a child's inode record on another volume stayed frozen at the
    /// reader's mount-time snapshot, the live readdir-sees/lookup-misses
    /// split (`d?????????`), minutes past the published staleness bound.
    ///
    /// Returns one `(volume index, outcome)` entry per volume — empty when
    /// the pass was not due. A failing volume never suppresses its
    /// siblings' entries (the driver logs each failure and the reader keeps
    /// serving that volume's current epoch).
    pub async fn poll_set_at(
        &self,
        volumes: &[Arc<KvMetaBackend>],
        now: Instant,
    ) -> Vec<(usize, Result<RevalidateOutcome, KvError>)> {
        if !self.due_at(now) {
            return Vec::new();
        }
        self.mark(now);
        let mut out = Vec::with_capacity(volumes.len());
        for (idx, vol) in volumes.iter().enumerate() {
            out.push((idx, vol.revalidate_reader().await));
        }
        out
    }
}

/// Adopt `epoch` on a reader's `trees` and run its cache's drop pass — the
/// cache-level entry point (spec §6.8 item 2).
///
/// Order is load-bearing: **roots first, then the epoch step.** The epoch is
/// published with `Release` inside the drop pass, so a thread that observes
/// the new epoch also observes the adopted roots; a thread that observes the
/// old epoch keeps using the old roots and gets a stale-stamped node,
/// i.e. a miss. Either way no traversal mixes a new root with a stale node.
///
/// A tree whose root the record does not name is left alone (a bit-8
/// partitioned volume's non-authority records carry no roots at all).
pub fn revalidate_trees(
    cache: &Arc<NodeCache>,
    trees: &[Arc<KvTree>],
    epoch: &RootEpoch,
) -> RevalidateOutcome {
    if epoch.ledger_seq > cache.revalidation_epoch() {
        for tree in trees {
            // On a forest volume every slot tree carries header id 0 and
            // the ledger names ONE id-0 root — the NATIVE slot's. A guest
            // slot tree's root lives in tree 0 and is adopted by the
            // reader's forest resync AFTER this step
            // (`KvMetaBackend::forest_reader_resync`); adopting the
            // ledger's id-0 root here would re-root every guest at the
            // native tree.
            if tree
                .forest_slot()
                .is_some_and(|s| s != super::record::NATIVE_FOREST_SLOT)
            {
                continue;
            }
            if let Some(root) = epoch.root_of(tree.tree_id()) {
                if let Err(e) = tree.adopt_root(RootPtr {
                    addr: root.node_addr,
                    seq: root.node_seq,
                }) {
                    log::error!(
                        "reader root adoption refused on tree {}: {e}",
                        tree.tree_id()
                    );
                }
            }
        }
    }
    cache.revalidate(epoch)
}

/// The shipped [`EpochPurgeSink`]: routes every suspect block key through
/// **the one** unified purge, `TieredCache::purge_block_key` (the R-6 law —
/// all five block-key stores in one call, with a grep-guard test preventing
/// a sixth from being forgotten). Nothing here addresses a tier directly.
///
/// Scope discipline, stated because it is the honest half: *which* keys are
/// suspect after a metadata epoch step is not a question the metadata plane
/// can answer — §6.3's block-key binding hazard is a device-offset reuse
/// problem, and bounding it exactly is spec §6.8 item **3** (the
/// freed-offset grace period), which is not built. So this sink purges what
/// the reader's data path **registered** through [`Self::note_suspect`].
///
/// **Which sink an S5 mount installs, and why not this one.** No
/// registration site exists yet, so on a mount this sink would purge
/// nothing and leave `meta_kv_revalidate_keys_purged` at 0 while
/// `meta_kv_revalidate_epochs` climbed — a coherence promise silently not
/// kept. The mount therefore installs `ro_coherence::ReaderEpochPurge`,
/// which purges the reader's whole block-key census: complete and never
/// silent, at the cost of being unscoped. This sink is the scoped form for
/// callers that own an `Arc<TieredCache>` **and** a registration site; when
/// one lands it becomes the fast path with the census as its fallback. The
/// live consequence: on a reader, `keys_purged` flat while `epochs` grows is
/// now a BROKEN trigger, not an unwired one.
pub struct TieredEpochPurge {
    tiers: Arc<TieredCache>,
    suspects: scc::HashSet<String>,
}

impl std::fmt::Debug for TieredEpochPurge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `TieredCache` is not `Debug` (it owns device handles); the sink's
        // own state is what a log line wants anyway.
        f.debug_struct("TieredEpochPurge")
            .field("pending_suspects", &self.suspects.len())
            .finish()
    }
}

impl TieredEpochPurge {
    pub fn new(tiers: Arc<TieredCache>) -> Arc<Self> {
        Arc::new(Self {
            tiers,
            suspects: scc::HashSet::default(),
        })
    }

    /// Register a block key whose binding was learned under the current
    /// epoch, so the next epoch step purges it. Idempotent.
    pub fn note_suspect(&self, block_key: &str) {
        let _ = self.suspects.insert_sync(block_key.to_string());
    }

    /// Suspect keys awaiting the next epoch step.
    pub fn pending(&self) -> usize {
        self.suspects.len()
    }
}

impl EpochPurgeSink for TieredEpochPurge {
    fn on_epoch_advance(&self, _from_epoch: u64, _to_epoch: u64) -> u64 {
        let mut keys: Vec<String> = Vec::new();
        self.suspects.retain_sync(|k| {
            keys.push(k.clone());
            false // drained: exactly one purge per registration
        });
        for key in &keys {
            self.tiers.purge_block_key(key);
        }
        keys.len() as u64
    }
}

impl KvMetaBackend {
    /// Declare this mount a **coherent reader** and install the optional R-6
    /// purge trigger (spec §6.8 items 2/5). Arms at the record the mount
    /// opened, so nothing is dropped; from here on the mount may not write
    /// (enforced at the node layer, `meta_kv_node_partition_refusals`).
    pub fn arm_reader_revalidation(
        &self,
        purge: Option<Arc<dyn EpochPurgeSink>>,
    ) -> Result<(), KvError> {
        let epoch = RootEpoch::from_ledger(self.mounted_ledger());
        self.node_cache().arm_revalidation(&epoch, purge)
    }

    /// Read the volume's newest ledger record as a [`RootEpoch`] — **the
    /// poll**: one 128 KiB `read_at`, no locks, no tree work.
    ///
    /// On a partitioned volume (spec §6.2 item 4) the record that carries
    /// tree roots is the root authority's; `read_newest_ledger` returns the
    /// newest valid record in any slot, which for the pre-partition and
    /// solo-authority forms this binary writes is exactly that record.
    pub async fn read_root_epoch(&self) -> Result<RootEpoch, KvError> {
        let rec = read_newest_ledger(self.device_path(), self.superblock().root_ledger.start)
            .await?
            .ok_or_else(|| {
                KvError::Corrupt(format!(
                    "{}: no valid root-ledger record to revalidate against",
                    self.device_path().display()
                ))
            })?;
        Ok(RootEpoch::from_ledger(&rec))
    }

    /// One revalidation pass for a declared reader: poll, adopt roots, drop
    /// stale nodes. Refuses loud on a mount that has not declared itself a
    /// reader — adopting a ledger's roots on a write mount whose SMOs have
    /// moved past them is time travel, so the declaration is mandatory.
    pub async fn revalidate_reader(&self) -> Result<RevalidateOutcome, KvError> {
        if !self.node_cache().is_revalidating() {
            return Err(KvError::Corrupt(
                "revalidate_reader on a mount that has not armed reader revalidation: \
                 call arm_reader_revalidation first (spec §6.8 item 2)"
                    .to_string(),
            ));
        }
        let epoch = self.read_root_epoch().await?;
        let trees = self.all_trees();
        let out = revalidate_trees(self.node_cache(), &trees, &epoch);
        if out.advanced {
            self.forest_reader_resync().await?;
        }
        Ok(out)
    }

    /// The reader's live epoch (0 = not a reader) — the seqlock-style handle
    /// a caller uses to detect that a multi-step read spanned a poll.
    pub fn reader_epoch(&self) -> u64 {
        self.node_cache().revalidation_epoch()
    }

    /// [`Self::reader_epoch`] gated for the kvmap fetch bracket
    /// (design-kvmap-block-map-tree §8 #4, amendment A9): `None` on every
    /// WRITE mount — the read-only latch is checked FIRST, so a writer's
    /// fetch path performs ZERO epoch-word reads (the pinned no-bracket
    /// byte-identity: the one-KvTx head+records commit + the 3.5/4a
    /// serialization + the rebind/currency ladder already own
    /// racing-publish skew there) — and `None` on an un-armed read-only
    /// open (an offline probe serves its frozen mount-time snapshot;
    /// there is no epoch step to bracket against).
    pub fn reader_fetch_epoch(&self) -> Option<u64> {
        if !self.is_read_only() {
            return None;
        }
        match self.reader_epoch() {
            super::epoch_core::UNARMED_EPOCH => None,
            epoch => Some(epoch),
        }
    }
}

/// **The A9 fetch-bracket core** (design-kvmap §8 #4): the seqlock loop
/// control for a multi-read resolution — probe → body → probe, retry
/// while the epoch stepped. Split out of the fetch path so the law is
/// directly pinnable (an epoch step is not injectable mid-fetch):
///
/// * `begin(None)` (a write mount / un-armed probe) commits `Adopt`
///   unconditionally — the single-pass, bracket-free shape;
/// * a stable epoch commits `Adopt`;
/// * a stepped epoch commits `Retry`, once per step, until
///   `max_retries` — then errs LOUD (a probe that never stabilizes under
///   a ≥ 1 s poll cadence is a broken epoch source, never a spin).
#[derive(Debug)]
pub struct FetchBracket {
    before: Option<u64>,
    retries: u32,
    max_retries: u32,
}

/// One bracketed pass's verdict — see [`FetchBracket::commit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BracketVerdict {
    /// The pass ran under one epoch (or none) — its reads compose.
    Adopt,
    /// The epoch stepped mid-pass — re-run the body under a fresh
    /// `begin`.
    Retry,
}

impl FetchBracket {
    pub fn new(max_retries: u32) -> Self {
        Self {
            before: None,
            retries: 0,
            max_retries,
        }
    }

    /// Open a pass at the probed epoch (`None` = un-bracketed).
    pub fn begin(&mut self, epoch: Option<u64>) {
        self.before = epoch;
    }

    /// Close the pass against the re-probed epoch.
    pub fn commit(&mut self, epoch: Option<u64>) -> Result<BracketVerdict, KvError> {
        let Some(before) = self.before else {
            return Ok(BracketVerdict::Adopt);
        };
        if epoch == Some(before) {
            return Ok(BracketVerdict::Adopt);
        }
        self.retries += 1;
        if self.retries > self.max_retries {
            return Err(KvError::Corrupt(format!(
                "reader fetch bracket exhausted after {} retries (epoch {before} → \
                 {epoch:?}): the revalidation epoch never stabilized across a pass, \
                 which a ≥ 1 s poll cadence cannot legitimately produce",
                self.retries - 1
            )));
        }
        Ok(BracketVerdict::Retry)
    }

    /// Retries consumed so far (the pinned once-per-step law).
    pub fn retries(&self) -> u32 {
        self.retries
    }
}

/// Snapshot of the reader-side counters, for the stats surface and for
/// tests. Every field is 0 for the whole life of a write mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevalidationStats {
    pub polls: u64,
    pub epochs: u64,
    pub nodes_dropped: u64,
    pub stale_serves: u64,
    pub dirty_skips: u64,
    pub keys_purged: u64,
    pub load_retries: u64,
    pub partition_refusals: u64,
}

/// Read the reader-side counters (`meta_kv_revalidate_*`,
/// `meta_kv_reader_load_retries`, `meta_kv_node_partition_refusals`).
pub fn revalidation_stats() -> RevalidationStats {
    RevalidationStats {
        polls: super::META_KV_REVALIDATE_POLLS.load(Ordering::Relaxed),
        epochs: super::META_KV_REVALIDATE_EPOCHS.load(Ordering::Relaxed),
        nodes_dropped: super::META_KV_REVALIDATE_NODES_DROPPED.load(Ordering::Relaxed),
        stale_serves: super::META_KV_REVALIDATE_STALE_SERVES.load(Ordering::Relaxed),
        dirty_skips: super::META_KV_REVALIDATE_DIRTY_SKIPS.load(Ordering::Relaxed),
        keys_purged: super::META_KV_REVALIDATE_KEYS_PURGED.load(Ordering::Relaxed),
        load_retries: super::META_KV_READER_LOAD_RETRIES.load(Ordering::Relaxed),
        partition_refusals: super::META_KV_NODE_PARTITION_REFUSALS.load(Ordering::Relaxed),
    }
}
