//! Root-ledger records (PR K3) **and the per-volume checkpoint/writeback
//! task (PR K6b)** — design §4.1 "Root ledger", §4.6.
//!
//! ## The K6b checkpoint task (§4.6)
//!
//! One background task per mounted v3 volume, riding the **existing
//! flusher cadence** (`SQUEEZEFS_META_FLUSH_INTERVAL_MS`; the v2 deferred
//! flusher's knob, reused as-is per §5.1). Each tick it:
//!
//! 1. drains the trees' maintenance queues (threshold writebacks +
//!    SMOs — all structure modifications run here, serialized on this
//!    task's `SmoContext`, §4.6);
//! 2. issues the deferred-mode flush barrier when a commit flagged one
//!    (the v2 `needs_flush` discipline, via the volume's `SyncCoalescer`);
//! 3. runs a **checkpoint cycle** when due — every ≤ 1 s, or journal
//!    distance > ring/2, or dirty-node count >
//!    `SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES` (default 4096, the §3
//!    mount-replay bound), or on shutdown.
//!
//! ## One checkpoint cycle (§4.6 pt 2, pinned order)
//!
//! `H = head` → flush every dirty node once (freeze under the node lock,
//! take its `oldest_dirty_seq` floor, append **outside** the lock; full
//! logs compact/split through the SMO path) → write dirty bitmap pages
//! (generation = this checkpoint's seq) → **barrier** (everything above +
//! every completed journal write + any previously-written ledger record
//! is now durable — the §4.6 pt 3 pending-reclaim drains here) → compute
//! **`tail = min(H, min in-flight reservation start, min dirty floor)`**
//! → write the ledger slot naming the synced roots + that tail → the
//! record's own durability rides the next barrier (or an immediate one
//! under ring pressure / shutdown), after which `reusable_upto` advances
//! to its tail and admission parkers wake.
//!
//! Why the three-way tail min is exact: a reservation `r < H` was taken
//! inside its node-lock window, so the flush pass (which takes the same
//! write locks) either blocked until its records were applied — flushing
//! them — or the tx is still in-window and `min_inflight_start ≤ r` holds
//! the tail back; records applied after the pass visited their node carry
//! reservations ≥ H by the same argument, and their floors re-lower the
//! min. The tail is always an entry boundary (H is the next reservation
//! start; inflight starts are entry starts; and floors round DOWN to
//! entry starts — FIND-SMO-TAIL, docs/design-smo-replay-currency.md §1b:
//! record seqs are `entry_start + i` across a multi-leaf tx, so folding
//! raw seqs let a leaf holding only `rec[j>0]` — its sibling flushed or
//! reserve-skip floor-restored — pin the tail STRICTLY INSIDE the entry,
//! which replay's chain walk (parsing AT the tail) dropped with
//! collateral entries to the resync point. Every `apply_locked` now
//! folds the member's entry start; SMO flips already pinned at
//! `res.start`).
//!
//! ## R10: the drain always makes progress
//!
//! The cycle consumes **zero ring bytes** for everything except SMO
//! records, which draw from the checkpoint-task reserve
//! ([`super::journal::checkpoint_reserve_bytes`]) and **never park**: an
//! exhausted reserve surfaces as
//! [`KvError::JournalReserveExhausted`], the cycle skips that node for
//! this pass (restoring its floor — the tail keeps respecting it),
//! finishes, advances `reusable_upto`, and retries next tick with freed
//! budget. No task ever blocks on ring space while holding a node lock;
//! parked user commits hold nothing the drain needs (§4.4 pt 5).
//!
//! The §4.7 pending-free cap rides the same discipline (design-smo-
//! replay-currency PR 4): an SMO refused at the FIFO-headroom admission
//! check surfaces as [`KvError::PendingFreeFull`] — the flush pass
//! skips-and-defers that node exactly like the reserve arm (clause c),
//! and the two `run_maintenance` arms remedy it with progress-audited
//! forced cycles (`force_pending_free_cycle`, clause b): each forced
//! cycle completes its non-SMO flushes, writes the ledger, barriers, and
//! drains the FIFO via `after_durable_barrier`; `PENDING_FREE_FORCE_CYCLES`
//! stalls without a `pending_count` decrease fail the volume loud (the
//! genuinely-wedged-tail terminal — never an unbounded retry loop).
//!
//! ---
//!
//! **PR K3's half** — slot encode/decode, round-robin slot placement,
//! durable write, and newest-valid-wins selection with torn-slot
//! fallback:
//!
//! ## Layout
//!
//! 32 round-robin 4 KiB slots (128 KiB total). A record with checkpoint
//! seq `s` lives in slot `s % 32`; mount picks the newest slot whose
//! checksum verifies. A torn newest slot (a checkpoint racing power loss)
//! therefore falls back to its predecessor — correct by the pending-free
//! rule (§4.7) and its ring twin (§4.6 pt 3): nothing either record
//! references has been overwritten.
//!
//! ## Slot format (little-endian; §4.1's field list)
//!
//! ```text
//! [0..4)   magic: u32      ROOT_LEDGER_MAGIC
//! [4..8)   len: u32        payload byte length
//! [8..16)  seq: u64        checkpoint sequence (slot = seq % 32)
//! [16..24) xxh3_64: u64    over the slot header (checksum zeroed) + payload
//! payload:
//!   journal_tail_seq: u64
//!   next_ino: u64
//!   alloc_bitmap_generation: u64
//!   n_roots: u16
//!   n_roots × { tree_id: u8, node_addr: u64, node_seq: u64 }
//! ```
//!
//! Every length is bounds-checked against its container before use (§9);
//! slots are written as full zero-padded 4 KiB images so a shorter record
//! can never leave stale bytes of a longer predecessor parseable.

use super::backend::KvMetaBackend;
use super::tree::SmoContext;
use super::KvError;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Number of round-robin ledger slots (§4.1).
pub const ROOT_LEDGER_SLOTS: u64 = 32;
/// Slot size in bytes.
pub const ROOT_LEDGER_SLOT_LEN: u64 = 4096;
/// Total ledger extent length (128 KiB — the §3 mount-read unit).
pub const ROOT_LEDGER_LEN: u64 = ROOT_LEDGER_SLOTS * ROOT_LEDGER_SLOT_LEN;
/// Ledger slot magic (`"KVRL"`).
pub const ROOT_LEDGER_MAGIC: u32 = 0x4B56_524C;
/// Fixed slot header length (`magic | len | seq | xxh3`).
pub const ROOT_LEDGER_HDR_LEN: usize = 24;

/// Fixed payload prefix: `journal_tail_seq | next_ino |
/// alloc_bitmap_generation | node_seq_watermark | n_roots`.
///
/// Format note (forward-only): the watermark widened this prefix by 8
/// bytes; pre-watermark slots fail the `n_roots` length-consistency
/// check and decode as absent. The superblock incompat bit
/// [`super::superblock::FEATURE_INCOMPAT_NODE_SEQ_WATERMARK`] refuses
/// pre-watermark volumes loud (reformat required) before any slot is
/// read, per the standing no-backwards-compatibility directive.
const PAYLOAD_FIXED_LEN: usize = 8 + 8 + 8 + 8 + 2;
/// Encoded size of one tree root: `tree_id | node_addr | node_seq`.
const ROOT_ENC_LEN: usize = 1 + 8 + 8;

/// One tree root named by a ledger record: `(tree_id, node_addr, node_seq)`
/// (§4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeRoot {
    pub tree_id: u8,
    pub node_addr: u64,
    pub node_seq: u64,
}

/// A root-ledger record (§4.1): the per-tree roots plus the journal tail,
/// the monotonic ino watermark (§4.8), and the allocator-bitmap generation
/// (§4.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRecord {
    /// Checkpoint sequence; strictly monotonic per volume. Slot = `seq % 32`.
    pub seq: u64,
    /// Per-tree roots.
    pub tree_roots: Vec<TreeRoot>,
    /// Replay starts here (§4.6 pt 2 tail rule — computed by K6b).
    pub journal_tail_seq: u64,
    /// Monotonic ino allocation watermark (§4.8).
    pub next_ino: u64,
    /// Allocator bitmap generation (§4.7 — consumed by K4).
    pub alloc_bitmap_generation: u64,
    /// Node-seq mint watermark: the per-volume mint counter at record
    /// build time. A mount reseeds the counter **at or above** this
    /// value, so node incarnation seqs never repeat within a generation
    /// — the invariant the §4.5 `node_seq_at_write == node_seq` frame
    /// admission relies on. Before this field existed, a clean shutdown
    /// (empty replay window) collapsed the reseed floor to the ROOT
    /// seqs and re-minted every non-root seq; recycled extents still
    /// holding frames stamped with a re-minted seq then chained the
    /// previous incarnation's checksummed records into the new node
    /// (the 2026-07-13 release-gate Finding A).
    pub node_seq_watermark: u64,
}

/// xxh3 over a slot image with the checksum field (bytes 16..24) zeroed —
/// the header-then-payload discipline the superblock and bsets use (§4.3).
fn slot_checksum(image: &[u8], payload_len: usize) -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(&image[..16]);
    h.update(&[0u8; 8]);
    h.update(&image[ROOT_LEDGER_HDR_LEN..ROOT_LEDGER_HDR_LEN + payload_len]);
    h.digest()
}

impl LedgerRecord {
    /// The slot index this record occupies (`seq % 32`).
    pub fn slot_index(&self) -> u64 {
        self.seq % ROOT_LEDGER_SLOTS
    }

    /// Encode into a full zero-padded 4 KiB slot image. Errors when the
    /// record cannot fit a slot (structurally impossible for the ≤ 5 trees
    /// of §4.2 — defense in depth for the record count).
    pub fn encode_slot(&self) -> Result<Vec<u8>, KvError> {
        let payload_len = PAYLOAD_FIXED_LEN + self.tree_roots.len() * ROOT_ENC_LEN;
        if ROOT_LEDGER_HDR_LEN + payload_len > ROOT_LEDGER_SLOT_LEN as usize
            || self.tree_roots.len() > usize::from(u16::MAX)
        {
            return Err(KvError::Corrupt(format!(
                "ledger record with {} tree roots does not fit a {}-byte slot",
                self.tree_roots.len(),
                ROOT_LEDGER_SLOT_LEN
            )));
        }
        let mut image = vec![0u8; ROOT_LEDGER_SLOT_LEN as usize];
        image[0..4].copy_from_slice(&ROOT_LEDGER_MAGIC.to_le_bytes());
        image[4..8].copy_from_slice(&(payload_len as u32).to_le_bytes());
        image[8..16].copy_from_slice(&self.seq.to_le_bytes());
        // Checksum stamped below.
        let mut pos = ROOT_LEDGER_HDR_LEN;
        image[pos..pos + 8].copy_from_slice(&self.journal_tail_seq.to_le_bytes());
        pos += 8;
        image[pos..pos + 8].copy_from_slice(&self.next_ino.to_le_bytes());
        pos += 8;
        image[pos..pos + 8].copy_from_slice(&self.alloc_bitmap_generation.to_le_bytes());
        pos += 8;
        image[pos..pos + 8].copy_from_slice(&self.node_seq_watermark.to_le_bytes());
        pos += 8;
        image[pos..pos + 2].copy_from_slice(&(self.tree_roots.len() as u16).to_le_bytes());
        pos += 2;
        for root in &self.tree_roots {
            image[pos] = root.tree_id;
            image[pos + 1..pos + 9].copy_from_slice(&root.node_addr.to_le_bytes());
            image[pos + 9..pos + 17].copy_from_slice(&root.node_seq.to_le_bytes());
            pos += ROOT_ENC_LEN;
        }
        let sum = slot_checksum(&image, payload_len);
        image[16..24].copy_from_slice(&sum.to_le_bytes());
        Ok(image)
    }

    /// Decode + verify one slot image: magic, bounds-checked lengths (§9),
    /// checksum. Any failure means "this slot holds no valid record" —
    /// the selection logic treats it as absent, never loud.
    pub fn decode_slot(buf: &[u8]) -> Result<Self, KvError> {
        if buf.len() < ROOT_LEDGER_HDR_LEN {
            return Err(KvError::Corrupt(format!(
                "truncated ledger slot: {} of {ROOT_LEDGER_HDR_LEN} header bytes",
                buf.len()
            )));
        }
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if magic != ROOT_LEDGER_MAGIC {
            return Err(KvError::Corrupt(format!(
                "bad ledger slot magic {magic:#010x} (expected {ROOT_LEDGER_MAGIC:#010x})"
            )));
        }
        let payload_len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
        // §9: the length is validated against the container before any
        // byte it governs is touched.
        if payload_len < PAYLOAD_FIXED_LEN || ROOT_LEDGER_HDR_LEN + payload_len > buf.len() {
            return Err(KvError::Corrupt(format!(
                "ledger payload length {payload_len} does not fit its slot"
            )));
        }
        let seq = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        let stored = u64::from_le_bytes(buf[16..24].try_into().unwrap());
        let computed = slot_checksum(buf, payload_len);
        if stored != computed {
            return Err(KvError::ChecksumMismatch { stored, computed });
        }
        let payload = &buf[ROOT_LEDGER_HDR_LEN..ROOT_LEDGER_HDR_LEN + payload_len];
        let journal_tail_seq = u64::from_le_bytes(payload[0..8].try_into().unwrap());
        let next_ino = u64::from_le_bytes(payload[8..16].try_into().unwrap());
        let alloc_bitmap_generation = u64::from_le_bytes(payload[16..24].try_into().unwrap());
        let node_seq_watermark = u64::from_le_bytes(payload[24..32].try_into().unwrap());
        let n_roots = usize::from(u16::from_le_bytes(payload[32..34].try_into().unwrap()));
        if PAYLOAD_FIXED_LEN + n_roots * ROOT_ENC_LEN != payload_len {
            return Err(KvError::Corrupt(format!(
                "ledger n_roots {n_roots} inconsistent with payload length {payload_len}"
            )));
        }
        let mut tree_roots = Vec::with_capacity(n_roots);
        let mut pos = PAYLOAD_FIXED_LEN;
        for _ in 0..n_roots {
            tree_roots.push(TreeRoot {
                tree_id: payload[pos],
                node_addr: u64::from_le_bytes(payload[pos + 1..pos + 9].try_into().unwrap()),
                node_seq: u64::from_le_bytes(payload[pos + 9..pos + 17].try_into().unwrap()),
            });
            pos += ROOT_ENC_LEN;
        }
        Ok(Self {
            seq,
            tree_roots,
            journal_tail_seq,
            next_ino,
            alloc_bitmap_generation,
            node_seq_watermark,
        })
    }
}

/// Write `rec` to its round-robin slot at `ledger_base` in `path` via
/// `uring_fs` (one full-slot `write_at`). Durability rides the caller's
/// barrier (§4.6 pt 2: "the root record's own durability rides the next
/// barrier").
pub async fn write_ledger_slot(
    path: &Path,
    ledger_base: u64,
    rec: &LedgerRecord,
) -> Result<(), KvError> {
    let image = rec.encode_slot()?;
    let off = ledger_base + rec.slot_index() * ROOT_LEDGER_SLOT_LEN;
    crate::uring_fs::write_at(path, off, image).await?;
    Ok(())
}

/// Read all 32 slots (one 128 KiB `uring_fs::read_at`) and return the
/// newest-seq record whose checksum verifies, or `None` when no slot holds
/// a valid record (fresh volume — the all-slots-invalid *loud* policy on a
/// non-fresh volume is mount wiring, PR K6a). A torn newest slot simply
/// loses the race to its intact predecessor (§4.1). `Err` is real device
/// I/O failure only ([`KvError::Io`]) — ledger *contents* never fail loud
/// here.
pub async fn read_newest_ledger(
    path: &Path,
    ledger_base: u64,
) -> Result<Option<LedgerRecord>, KvError> {
    let got = crate::uring_fs::read_at(path, ledger_base, ROOT_LEDGER_LEN as usize).await?;
    let mut newest: Option<LedgerRecord> = None;
    for slot in 0..ROOT_LEDGER_SLOTS as usize {
        let start = slot * ROOT_LEDGER_SLOT_LEN as usize;
        if start >= got.len() {
            break; // short extent: the rest reads as absent
        }
        let end = (start + ROOT_LEDGER_SLOT_LEN as usize).min(got.len());
        if let Ok(rec) = LedgerRecord::decode_slot(&got[start..end]) {
            if newest.as_ref().is_none_or(|n| rec.seq > n.seq) {
                newest = Some(rec);
            }
        }
    }
    Ok(newest)
}

// ---------------------------------------------------------------------------
// PR K6b: the checkpoint/writeback task (module docs above).
// ---------------------------------------------------------------------------

/// `SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES` (§5.1; default 4096 — the
/// §3 mount-replay working-set bound).
pub const CHECKPOINT_MAX_DIRTY_NODES_ENV: &str = "SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES";

fn max_dirty_nodes() -> u64 {
    std::env::var(CHECKPOINT_MAX_DIRTY_NODES_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(4096)
}

/// Checkpoint cadence ceiling: a cycle runs at least this often even when
/// no other trigger fires (§4.6 pt 2 "every ≤ 1 s").
const CHECKPOINT_MAX_AGE_MS: u128 = 1000;

/// §4.7 at-cap forced-cycle bound (design-smo-replay-currency PR 4
/// clause b; the [`KvMetaBackend::checkpoint_past`] precedent's shape):
/// consecutive forced cycles without `pending_count` decreasing before
/// the volume fails loud. Genuine wedges present here — a tail pinned by
/// a skip loop fills the production cap in ≈ 70 s of sustained storm,
/// too fast to leave to "the cadence will get to it", and a healthy
/// convergence needs at most a couple of cycles (the first discharges
/// dying floors, the second's tail covers the parked frees).
const PENDING_FREE_FORCE_CYCLES: u64 = 8;

/// The §4.7 at-cap remedy shared by both `run_maintenance` arms
/// (design-smo-replay-currency PR 4 clause b): "pressure forces a
/// checkpoint rather than unsafe reuse", made mechanism. One forced
/// cycle (`barrier_now` — the FIFO drains via `after_durable_barrier`
/// inside it), progress-audited on the backend so the bound spans
/// maintenance passes: any `pending_count` decrease resets the stall
/// counter; [`PENDING_FREE_FORCE_CYCLES`] stalls without progress fail
/// the volume loud (never an unbounded retry loop — and never the
/// pre-fix silent post-swap leak).
async fn force_pending_free_cycle(
    be: &Arc<KvMetaBackend>,
    smo: &mut SmoContext,
) -> Result<(), KvError> {
    let before = be.allocator().pending_count();
    be.checkpoint_cycle(smo, true).await?;
    if be.allocator().pending_count() < before {
        be.pending_free_stalled_cycles.store(0, Ordering::Release);
        return Ok(());
    }
    let stalled = be
        .pending_free_stalled_cycles
        .fetch_add(1, Ordering::AcqRel)
        + 1;
    if stalled >= PENDING_FREE_FORCE_CYCLES {
        let msg = format!(
            "pending-free FIFO saturated ({} parked) and {stalled} forced checkpoint \
             cycles released nothing — the durable tail is wedged below every parked \
             retirement (§4.7 at-cap bound, design-smo-replay-currency PR 4)",
            be.allocator().pending_count()
        );
        be.fail_stop_loud(&msg);
        return Err(KvError::Corrupt(msg));
    }
    Ok(())
}

/// Spawn the per-volume checkpoint/writeback task (called by
/// `KvMetaBackend::open`). The task holds a `Weak` backend reference —
/// dropping the backend without `shutdown` reaps it on its next tick (the
/// v2 flusher's sentinel discipline; pinned by
/// `tests/dismount_teardown_tests.rs`) — plus an owned liveness token the
/// teardown tests probe through `checkpoint_alive_probe`.
pub(super) fn spawn_checkpoint_task(be: &Arc<KvMetaBackend>) {
    // PR M1 ordering pin (design-metadata-throughput §5.0 B2): the mount
    // gate committed + barriered the writer_claim BEFORE this call — the
    // trace event is the spawn hook the ordering assertion reads.
    be.trace_guard_event("checkpoint_task_spawned");
    let weak = Arc::downgrade(be);
    let alive = Arc::new(());
    let probe = Arc::downgrade(&alive);
    let wake = be.checkpoint_wake();
    // The EXISTING flusher cadence (§4.6): strict mode (interval 0) still
    // needs the background cycle for ring reclamation and SMO service —
    // it ticks at 100 ms; commits barrier themselves.
    let interval = match crate::meta_backend::resolve_flush_interval_ms() {
        0 => 100,
        ms => ms,
    };
    let handle = tokio::spawn(checkpoint_task(weak, alive, wake, interval));
    be.install_checkpoint_task(handle, probe);
}

/// Spawn the per-volume **pending-times drain** task (PR M6, design-
/// metadata-throughput §5.4 D4 — called by `KvMetaBackend::open` beside
/// the checkpoint task). It makes absorbed SETATTR-echo refinements
/// durable in batched transactions on the flush cadence (strict mode
/// ticks at 100 ms like the checkpoint task; commits inside the drain
/// barrier themselves there), waking early on cap crossings.
///
/// A DEDICATED task, deliberately not a checkpoint-tick step: the drain
/// commits through `commit_tx`, whose ring admission may park — parking
/// the checkpoint task on admission would deadlock the very drain that
/// frees ring space (§4.4 pt 5's liveness shape holds precisely because
/// parked committers and the checkpoint drain are different tasks). Same
/// `Weak` sentinel discipline: dropping the backend without `shutdown`
/// reaps it on its next tick; `shutdown` wakes and JOINS it (no leaked
/// tasks).
pub(super) fn spawn_times_drain_task(be: &Arc<KvMetaBackend>) {
    let weak = Arc::downgrade(be);
    let wake = be.times_drain_wake_handle();
    let interval = match crate::meta_backend::resolve_flush_interval_ms() {
        0 => 100,
        ms => ms,
    };
    let handle = tokio::spawn(times_drain_task(weak, wake, interval));
    be.install_times_drain_task(handle);
}

async fn times_drain_task(
    weak: std::sync::Weak<KvMetaBackend>,
    wake: Arc<tokio::sync::Notify>,
    interval_ms: u64,
) {
    let period = std::time::Duration::from_millis(interval_ms);
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = wake.notified() => {}
        };
        let Some(be) = weak.upgrade() else {
            return; // backend dropped without shutdown: exit, leak nothing
        };
        if be.is_shutting_down() {
            return; // shutdown drained inline and joins us
        }
        if be.pending_times_len() == 0 {
            continue;
        }
        if let Err(e) = be.drain_pending_times_now().await {
            log::warn!(
                "kv pending-times drain failed on {:?}: {e} (refinements stay \
                 parked; the next tick retries)",
                be.device_path()
            );
        }
    }
}

async fn checkpoint_task(
    weak: std::sync::Weak<KvMetaBackend>,
    alive: Arc<()>,
    wake: Arc<tokio::sync::Notify>,
    interval_ms: u64,
) {
    // Owned for the task's lifetime: `checkpoint_alive_probe` upgrades
    // iff this task is still running.
    let _alive = alive;
    let mut last_checkpoint = std::time::Instant::now();
    // A FIXED cadence (not sleep-in-select, which would re-arm on every
    // wake): §4.6 pt 1 threshold wakes must never starve the cadence's
    // barriers/checkpoints under a sustained storm.
    let period = std::time::Duration::from_millis(interval_ms);
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let cadence = tokio::select! {
            _ = ticker.tick() => true,
            _ = wake.notified() => false,
        };
        let Some(be) = weak.upgrade() else {
            return; // backend dropped without shutdown: exit, leak nothing
        };
        let shutting_down = be.is_shutting_down();
        // PR M1 (design-metadata-throughput §5.0, Issue 14): a FAILED
        // volume — fenced at a barrier (reservation conflict) or latched
        // by repeated write/barrier failures — is fail-stopped: mutations
        // already return EIO, and re-running maintenance + barriers every
        // tick against a fenced/dead device is pure log spam (and, post-
        // fence, writes past a foreign reservation). Idle until shutdown,
        // whose final attempt still runs loud. This applies to the
        // journal-durability paths ONLY — the data-path writeback ladder
        // lives elsewhere and stays retry-forever (never-lossy contract).
        if be.is_failed() && !shutting_down {
            continue;
        }
        if cadence || shutting_down {
            if let Err(e) = tick(&be, &mut last_checkpoint, shutting_down).await {
                log::warn!(
                    "kv checkpoint tick failed on {:?}: {e} (state stays RAM-consistent; \
                     retrying next tick — barrier failures additionally escalate through \
                     the sync_device rungs, design-metadata-throughput §5.0)",
                    be.device_path()
                );
            }
            if shutting_down {
                return; // final checkpoint ran inside the tick
            }
        } else {
            // §4.6 pt 1's threshold trigger (a commit crossed a bset
            // worth of open delta): appends only — no barrier, no ledger,
            // both stay on the cadence. Commit RAM-apply cost is O(open
            // delta), so the drain must not wait out the tick.
            if let Err(e) = maintenance_pass(&be).await {
                log::warn!(
                    "kv maintenance pass failed on {:?}: {e} (state stays RAM-consistent; \
                     the cadence tick retries)",
                    be.device_path()
                );
            }
            // Work arrived while draining (or a reserve drain deferred
            // it): re-arm and return to the select so the ticker still
            // gets its turn — never spin the cadence out.
            if be.trees().into_iter().any(|t| t.maintenance_pending()) {
                wake.notify_one();
            }
        }
    }
}

/// The maintenance-only wake body: drain every tree's threshold queue
/// (bset appends + any compact/split the appends force). Journal-reserve
/// exhaustion runs one full checkpoint cycle — the §4.4 pt 5 zero-ring-
/// byte drain — exactly like the cadence tick's maintenance step; a
/// pending-free-FIFO refusal (the SMO admission headroom check,
/// design-smo-replay-currency PR 4 clause a) forces the same cycle
/// through the progress-audited at-cap remedy (clause b).
async fn maintenance_pass(be: &Arc<KvMetaBackend>) -> Result<(), KvError> {
    let mut smo = be.smo.lock().await;
    for tree in be.trees() {
        loop {
            match tree.run_maintenance(&mut smo).await {
                Ok(_) => break,
                Err(KvError::JournalReserveExhausted { .. }) => {
                    be.checkpoint_cycle(&mut smo, true).await?;
                }
                Err(KvError::PendingFreeFull { .. }) => {
                    force_pending_free_cycle(be, &mut smo).await?;
                }
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

/// One task tick: maintenance → deferred flush barrier → checkpoint when
/// due. `final_cycle` (shutdown) drains in-flight commits first and
/// forces a full cycle with an immediate post-ledger barrier, leaving
/// `tail == head` — an empty replay window for the next mount.
async fn tick(
    be: &Arc<KvMetaBackend>,
    last_checkpoint: &mut std::time::Instant,
    final_cycle: bool,
) -> Result<(), KvError> {
    let mut smo = be.smo.lock().await;

    if final_cycle {
        // New mutations are already refused (`write_gate`); wait out the
        // in-flight ones so the final flush pass sees every applied
        // record and the tail lands exactly on the head. Drain UNTIL THE
        // HEAD IS STABLE, not one sampled head: a commit that passed the
        // gate before the flag can reserve ring space after a one-shot
        // sample, and its entry then sits past the final checkpoint's
        // tail — the next mount replays it (kill-9 soak, "clean shutdown
        // must leave an empty replay window", ~1/200 deep-churn rounds).
        // Gated writers are finite, so the head converges; the iteration
        // bound is a belt against a pathological writer, after which the
        // final cycle proceeds with the freshest head it saw.
        for _ in 0..64 {
            let head = be.journal_ring().core().head();
            be.journal_ring().wait_completed_upto(head).await;
            if be.journal_ring().core().head() == head {
                break;
            }
        }
    }

    // 1. Threshold maintenance (appends + SMOs, serialized here — §4.6).
    //    Reserve exhaustion runs a drain cycle and retries; a pending-
    //    free-FIFO refusal (SMO admission headroom, design-smo-replay-
    //    currency PR 4 clause a) forces the progress-audited at-cap
    //    cycle (clause b) and retries the same way.
    for tree in be.trees() {
        loop {
            match tree.run_maintenance(&mut smo).await {
                Ok(_) => break,
                Err(KvError::JournalReserveExhausted { .. }) => {
                    be.checkpoint_cycle(&mut smo, true).await?;
                    *last_checkpoint = std::time::Instant::now();
                }
                Err(KvError::PendingFreeFull { .. }) => {
                    force_pending_free_cycle(be, &mut smo).await?;
                    *last_checkpoint = std::time::Instant::now();
                }
                Err(e) => return Err(e),
            }
        }
    }

    // 2. The deferred-mode flush barrier (the v2 flusher tick). Also
    //    drains the §4.6 pt 3 pending-reclaim for previously-written
    //    ledger records.
    if be.take_needs_flush() {
        crate::fuse_client::METRICS
            .meta_flush_deferred
            .fetch_add(1, Ordering::Relaxed);
        be.sync_device().await.map_err(KvError::Io)?;
    }

    // 3. Checkpoint decision (§4.6 pt 2): cadence, journal distance,
    //    dirty-node cap, shutdown.
    let core = be.journal_ring().core();
    let distance = core.head().saturating_sub(core.reusable_upto());
    let ring_pressure = distance > core.geometry().logical_len() / 2;
    let mut dirty_nodes = 0u64;
    be.node_cache().for_each_node(|n| {
        if n.dirty_floor() != u64::MAX {
            dirty_nodes += 1;
        }
    });
    let due = final_cycle
        || ring_pressure
        || dirty_nodes > max_dirty_nodes()
        || last_checkpoint.elapsed().as_millis() >= CHECKPOINT_MAX_AGE_MS;
    if due && (final_cycle || dirty_nodes > 0 || distance > 0) {
        // Immediate post-ledger barrier under pressure or at shutdown:
        // reclamation must not lag a cycle when parkers wait on it.
        be.checkpoint_cycle(&mut smo, ring_pressure || final_cycle)
            .await?;
        *last_checkpoint = std::time::Instant::now();
    }
    if final_cycle {
        // Shutdown guarantee (`KvMetaBackend::shutdown`: "tail == head ⇒
        // an empty replay window"): a cycle's flush pass may itself
        // journal — a node whose log area filled compacts through the SMO
        // path, whose claim/free records land at positions PAST the `H`
        // that cycle's tail was computed from, and an SMO's parent-pointer
        // apply re-dirties an already-visited node. On the cadence path
        // the NEXT cycle covers them ("reclamation lags one cycle"), but
        // at shutdown there is no next cycle — the residue would replay at
        // the next mount. Iterate to the fixpoint: every extra cycle
        // flushes what the previous one dirtied and covers what it
        // journaled; a cycle only journals when it rewrites a full node
        // log (strictly consumed), so this converges within the SMO
        // cascade height. The bound is defensive.
        for _ in 0..16 {
            let core = be.journal_ring().core();
            if core.head() == core.reusable_upto() {
                return Ok(());
            }
            be.checkpoint_cycle(&mut smo, true).await?;
            *last_checkpoint = std::time::Instant::now();
        }
        let core = be.journal_ring().core();
        if core.head() != core.reusable_upto() {
            log::warn!(
                "shutdown checkpoint did not converge to an empty replay window \
                 (head={}, reusable_upto={}): the next mount will replay the residue \
                 (sound, but the shutdown tail==head guarantee was missed)",
                core.head(),
                core.reusable_upto()
            );
        }
    }
    Ok(())
}

impl KvMetaBackend {
    /// One §4.6 pt 2 checkpoint cycle (module docs pin the order).
    /// Serialized by the SMO mutex the caller holds. `barrier_now` makes
    /// the freshly-written ledger record durable inside this cycle
    /// (shutdown / ring pressure); otherwise its durability rides the
    /// next barrier and reclamation lags one cycle (§4.6 pt 2's "rides
    /// the next barrier" default).
    pub(super) async fn checkpoint_cycle(
        &self,
        smo: &mut SmoContext,
        barrier_now: bool,
    ) -> Result<(), KvError> {
        let h = self.journal_ring().core().head();

        // ---- Flush pass: every dirty node once, snapshot-then-write.
        // SMO-reserve exhaustion skips the node (floor restored — the
        // tail keeps respecting it) and retries next cycle with the
        // budget this cycle frees.
        let mut dirty: Vec<(u8, u64)> = Vec::new();
        self.node_cache().for_each_node(|n| {
            if n.dirty_floor() != u64::MAX && !n.state().is_superseded() {
                dirty.push((n.tree_id(), n.addr()));
            }
        });
        for (tree_id, addr) in dirty {
            let tree = self
                .trees()
                .into_iter()
                .find(|t| t.tree_id() == tree_id)
                .expect("dirty node belongs to a mounted tree");
            match tree.checkpoint_flush_node(smo, addr).await {
                Ok(()) => {}
                Err(KvError::JournalReserveExhausted { needed }) => {
                    log::debug!(
                        "checkpoint: SMO reserve exhausted ({needed} B) at node {addr:#x}; \
                         deferred to the next cycle"
                    );
                }
                Err(KvError::PendingFreeFull { pending }) => {
                    // design-smo-replay-currency PR 4 clause c: identical
                    // skip-and-defer to the reserve arm (the floor was
                    // restored inside checkpoint_flush_node), so a FORCED
                    // cycle under a cap-saturated storm still completes
                    // its non-SMO flushes, writes the ledger, barriers,
                    // and drains the FIFO via after_durable_barrier —
                    // aborting the whole cycle here would livelock the
                    // at-cap remedy against the very pressure it exists
                    // to relieve (ring reclamation wedging behind it).
                    log::debug!(
                        "checkpoint: pending-free FIFO full ({pending} parked) at node \
                         {addr:#x}; SMO deferred to the next cycle"
                    );
                }
                Err(e) => return Err(e),
            }
        }

        // ---- Dirty bitmap pages, stamped with this checkpoint's seq
        // (§4.7: the generation the ledger record names).
        let ckpt_seq = self.checkpoint_seq.load(Ordering::Acquire) + 1;
        self.allocator()
            .write_dirty_pages(
                self.device_path(),
                self.superblock().alloc_bitmap.start,
                ckpt_seq,
            )
            .await?;

        // ---- Barrier #1: node appends + bitmap pages + every completed
        // journal write + any previously-written ledger record become
        // durable (the §4.6 pt 3 pending-reclaim drains inside).
        self.sync_device().await.map_err(KvError::Io)?;

        // ---- The tail rule (module docs; §4.6 pt 2), plus the FIND-VS-A
        // dying-floor clamp: floors of nodes whose mappings LEFT the cache
        // since the last drain (SMO retires, evictions, publish-replaces)
        // are invisible to the live walk below, but their records' durable
        // *reachability* is only tied down by a ledger record written
        // after the departure — so they clamp this record's tail, keeping
        // every such record inside the replay window. Drained here; folded
        // back on any failure past this point (the ledger slot never
        // landed, so the next cycle must still respect the floor).
        let dying_floors = self.node_cache().take_dying_floors();
        let mut tail = h
            .min(self.journal_ring().min_inflight_start())
            .min(dying_floors);
        self.node_cache().for_each_node(|n| {
            tail = tail.min(n.dirty_floor());
        });

        // ---- The ledger record naming the synced roots + that tail.
        let [inodes, dentries, xattrs] = self.trees();
        let tree_roots = vec![
            TreeRoot {
                tree_id: inodes.tree_id(),
                node_addr: inodes.root().addr,
                node_seq: inodes.root().seq,
            },
            TreeRoot {
                tree_id: dentries.tree_id(),
                node_addr: dentries.root().addr,
                node_seq: dentries.root().seq,
            },
            TreeRoot {
                tree_id: xattrs.tree_id(),
                node_addr: xattrs.root().addr,
                node_seq: xattrs.root().seq,
            },
        ];
        let rec = LedgerRecord {
            seq: ckpt_seq,
            tree_roots,
            journal_tail_seq: tail,
            next_ino: self.next_ino(),
            alloc_bitmap_generation: ckpt_seq,
            // Captured AFTER the flush loop (whose SMOs mint): every seq
            // stamped into a frame that an extent freed ≤ this record can
            // carry is ≤ this watermark; mints after capture are covered
            // by replay floors (journaled SMO pointers) until the NEXT
            // record's watermark — see the extent-reuse chain argument
            // on [`LedgerRecord::node_seq_watermark`].
            node_seq_watermark: inodes.node_seq_snapshot(),
        };
        if let Err(e) = write_ledger_slot(
            self.device_path(),
            self.superblock().root_ledger.start,
            &rec,
        )
        .await
        {
            // The covering ledger record never landed: the drained dying
            // floors are still uncovered — fold them back so the next
            // cycle's tail keeps clamping to them (FIND-VS-A).
            self.node_cache().restore_dying_floors(dying_floors);
            return Err(e);
        }
        self.checkpoint_seq.store(ckpt_seq, Ordering::Release);
        self.last_ledger_tail.store(tail, Ordering::Release);
        // The NEXT record's seq still stamps the historical retire tag in
        // free-record VALUES (byte-for-byte format compat) — the release
        // GATE itself rides the tail (design-smo-replay-currency §2-A).
        self.retire_seq.store(ckpt_seq + 1, Ordering::Release);
        self.pending_reclaim.lock().unwrap().push(tail);
        super::META_KV_CHECKPOINTS.fetch_add(1, Ordering::Relaxed);

        if barrier_now {
            // Make THIS record durable now: reclamation (reusable_upto,
            // pending-free, cache durable tail) advances before we
            // return — the R10 drain shape and the shutdown guarantee.
            self.sync_device().await.map_err(KvError::Io)?;
        }
        Ok(())
    }
}
