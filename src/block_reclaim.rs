//! Background block-reclaim queue — the overwrite-throughput fix
//! (`.benchmarks/2026-07-27-async-block-reclaim.md`; contract pinned in
//! `tests/async_block_reclaim_tests.rs`).
//!
//! A striped overwrite's displaced blocks used to issue their device
//! reclaim (`BLKDISCARD` on namespaces / `PUNCH_HOLE` on file backings —
//! `free_reclaim_op`) SYNCHRONOUSLY inside `BackendRouter::free_block`.
//! On NVMe-oF every discard is a fabric round-trip (~235 µs measured), so
//! a 4 GiB/s overwrite stream (≈1000 displaced 4 MiB blocks/s) serialized
//! ~2 GB/s of throughput behind space-return work that owns NO
//! correctness: the `begin_free → finish_free` window owns crash-safe
//! free accounting; the reclaim is purely returning space to the device.
//!
//! Design:
//!
//! * A terminal free enqueues a [`ReclaimEntry`] — with `begin_free`
//!   already taken and the offset registered in the allocator's in-flight
//!   registry — and returns. The background worker (or a drain) pops the
//!   entry, issues the reclaim OFF the write path, then `finish_free`s.
//!   The offset is **not reallocatable until after its reclaim**, exactly
//!   the pre-existing law, so a queued discard can never race a new
//!   owner's DMA at the reused offset.
//! * **Exactly-once**: the lock-free `SegQueue` pop is the ownership
//!   transfer — whoever pops an entry (worker batch, ENOSPC valve drain,
//!   unmount drain) processes it; nothing is ever re-queued.
//! * **ENOSPC pressure valve**: `BlockAllocator::allocate_block` invokes
//!   the wired [`ReclaimQueue::drain_sync`] before refusing for space —
//!   a full volume can never be wedged by lazily-queued reclaims.
//! * **Crash posture** (kill-9 with queued entries): the queue is
//!   RAM-only space-return work. The mount recovery walk rebuilds
//!   refcounts/free-list from durable layout maps, so the freed offsets
//!   re-enter the free list; the only loss is the *discard itself* —
//!   un-returned thin-device space, re-covered when the offset is reused
//!   (allocation prefers the free list, and write-before-publish rewrites
//!   the range) or freed terminally again. No journal-adjacent replay is
//!   needed and none is kept.
//! * **io_uring posture**: `BLKDISCARD` and `fallocate(PUNCH_HOLE)` are
//!   synchronous ioctl/fallocate calls with no io_uring opcode on the
//!   kernels we target. Per the io_uring-first policy this is acceptable
//!   ONLY because the work is off the hot path: batches run on the
//!   blocking pool (`spawn_blocking`), never on a tokio worker and never
//!   on the FUSE write path.
//!
//! Knobs (read at queue construction — i.e. per `BackendRouter`):
//! `SQUEEZEFS_RECLAIM_BATCH_BLOCKS` (max entries per worker batch,
//! default 64, clamp 1..=1024), `SQUEEZEFS_RECLAIM_BATCH_MS`
//! (accumulation window after first wake, default 2, clamp 0..=600000 —
//! large values park the worker, used by tests), and
//! `SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS` (bounded-memory cap, default
//! 4096; an enqueue past the cap processes INLINE as backpressure).

use crate::block_allocator::{BlockAllocator, InflightAllocGuard};
use crate::fuse_client::METRICS;
use crate::routing::{free_reclaim_op, FreeReclaimOp};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

fn env_u64(key: &str, default: u64, lo: u64, hi: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
        .clamp(lo, hi)
}

/// One terminally-freed block whose device reclaim + `finish_free` the
/// queue now owns. `begin_free` has already retired the incarnation and
/// the read tiers are already purged; the in-flight guard keeps fsck's
/// C2/C3/C6 machinery from adjudicating the begin_free-limbo offset while
/// the reclaim is queued (a live owner shields it; a crashed owner drops
/// the guard and shields nothing).
pub struct ReclaimEntry {
    pub allocator: Arc<BlockAllocator>,
    /// Dropped (deregistered) after `finish_free` — the entry's natural
    /// drop order, fields being consumed at the end of processing.
    pub inflight: InflightAllocGuard,
    pub device_path: String,
    pub offset: u64,
    pub size: u64,
}

/// The per-router background reclaim queue. Lock-free (`SegQueue` +
/// atomics) — enqueue on the write path is push + notify, no latches, no
/// device I/O.
pub struct ReclaimQueue {
    q: crossbeam::queue::SegQueue<ReclaimEntry>,
    /// Entries currently queued (upper bound during a push window).
    len: AtomicU64,
    /// Entries popped whose reclaim has not completed — drains must wait
    /// for these too, or the ENOSPC valve could observe an empty queue
    /// while the last free blocks are in a worker's hands.
    processing: AtomicU64,
    notify: Arc<tokio::sync::Notify>,
    worker_armed: AtomicBool,
    /// The writer-guard fence probe (`tests/async_block_reclaim_tests.rs`
    /// contract 6): returns `true` when any volume of this mount's meta
    /// set has latched the D0 fail-stop `failed` state — the SAME signal
    /// the journal-barrier escalation sets (`KvMetaBackend::is_failed`,
    /// what `disabled_volumes` mirrors). Wired by
    /// `DataRouter::set_meta_backend`; bare routers (no meta set) have no
    /// probe and never halt.
    fence_signal: std::sync::OnceLock<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// Sticky fence halt: once the probe fires, device reclaims cease
    /// PERMANENTLY for this queue (a fenced holder is dead until remount
    /// — `failed` never clears in-process).
    halted: AtomicBool,
    batch_blocks: u64,
    batch_ms: u64,
    max_queued: u64,
}

impl ReclaimQueue {
    pub fn from_env() -> Arc<Self> {
        Arc::new(Self {
            q: crossbeam::queue::SegQueue::new(),
            len: AtomicU64::new(0),
            processing: AtomicU64::new(0),
            notify: Arc::new(tokio::sync::Notify::new()),
            worker_armed: AtomicBool::new(false),
            fence_signal: std::sync::OnceLock::new(),
            halted: AtomicBool::new(false),
            batch_blocks: env_u64("SQUEEZEFS_RECLAIM_BATCH_BLOCKS", 64, 1, 1024),
            batch_ms: env_u64("SQUEEZEFS_RECLAIM_BATCH_MS", 2, 0, 600_000),
            max_queued: env_u64("SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS", 4096, 1, 1 << 20),
        })
    }

    /// Wire the writer-guard fence probe (see the field doc). Set once at
    /// meta-backend wiring; later calls are no-ops.
    pub fn set_fence_signal(&self, sig: Arc<dyn Fn() -> bool + Send + Sync>) {
        let _ = self.fence_signal.set(sig);
    }

    /// `true` ⇔ device reclaims are (now) fence-halted. Evaluated once
    /// per batch and by the ENOSPC valve — one atomic load when already
    /// halted, one cheap probe (a few atomic loads over the meta set)
    /// otherwise. Latches sticky and loud on the first observation.
    pub(crate) fn fence_halted(&self) -> bool {
        if self.halted.load(Ordering::Acquire) {
            return true;
        }
        let Some(sig) = self.fence_signal.get() else {
            return false;
        };
        if sig() {
            if !self.halted.swap(true, Ordering::AcqRel) {
                log::error!(
                    "block-reclaim: writer guard fenced / volume fail-stopped — ceasing \
                     ALL device reclaims permanently (a fenced zombie's discard can land \
                     on offsets the successor writer has reallocated); queued entries are \
                     dropped WITHOUT finish_free — the successor's recovery owns the \
                     accounting (un-returned thin space, re-covered on reuse; counted in \
                     block_free_reclaim_fence_halts)"
                );
            }
            return true;
        }
        false
    }

    /// Queue one terminal free's reclaim. Past the bounded-memory cap the
    /// entry is processed INLINE (backpressure — the reclaimer is not
    /// keeping up; conservation over latency).
    pub fn enqueue(self: &Arc<Self>, entry: ReclaimEntry) {
        if self.len.load(Ordering::Acquire) >= self.max_queued {
            self.processing.fetch_add(1, Ordering::AcqRel);
            METRICS
                .block_free_reclaim_queue_bytes
                .fetch_add(entry.size, Ordering::Relaxed);
            self.process_entries(vec![entry]);
            return;
        }
        METRICS
            .block_free_reclaim_queued
            .fetch_add(1, Ordering::Relaxed);
        METRICS
            .block_free_reclaim_queue_bytes
            .fetch_add(entry.size, Ordering::Relaxed);
        // len before push: `len` is an upper bound, so a concurrent pop
        // can never underflow it.
        self.len.fetch_add(1, Ordering::AcqRel);
        self.q.push(entry);
        self.ensure_worker();
        self.notify.notify_one();
    }

    /// Spawn the background worker once (lazily — the first enqueue runs
    /// inside a tokio context; if it somehow does not, the arm is retried
    /// and drains/valve still guarantee progress).
    fn ensure_worker(self: &Arc<Self>) {
        if self.worker_armed.swap(true, Ordering::AcqRel) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.worker_armed.store(false, Ordering::Release);
            return;
        };
        // Weak, deliberately (the health-worker sentinel discipline): a
        // strong Arc would keep the queue — and this loop — alive forever
        // after the router dropped. `Drop` notifies so a parked worker
        // wakes, fails its upgrade, and exits.
        let weak = Arc::downgrade(self);
        let notify = self.notify.clone();
        handle.spawn(async move {
            loop {
                notify.notified().await;
                let batch_ms = match weak.upgrade() {
                    Some(q) => q.batch_ms,
                    None => return,
                };
                // Accumulation window: displaced blocks of one overwrite
                // stream arrive back-to-back — waiting a beat lets the
                // batch coalesce adjacent ranges into fewer device
                // commands. (Arc NOT held across the sleep.)
                if batch_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(batch_ms)).await;
                }
                loop {
                    let Some(q) = weak.upgrade() else { return };
                    let batch = q.take_batch(q.batch_blocks as usize);
                    if batch.is_empty() {
                        break;
                    }
                    METRICS
                        .block_free_reclaim_batches
                        .fetch_add(1, Ordering::Relaxed);
                    // Synchronous ioctls — blocking pool, never a tokio
                    // worker (see module doc, io_uring posture).
                    let res = tokio::task::spawn_blocking(move || {
                        let n = batch.len();
                        q.process_entries(batch);
                        n
                    })
                    .await;
                    if res.is_err() {
                        // Panic in the blocking task: the batch guard
                        // already reconciled the counters; entries were
                        // dropped (guards deregistered) — fsck C6 heals
                        // any begin_free limbo. Loud, never silent.
                        log::error!("block-reclaim batch panicked; see fsck C6");
                    }
                }
            }
        });
    }

    /// Pop up to `max` entries, reserving them in `processing` BEFORE the
    /// pop so a concurrent drain never observes empty+idle while entries
    /// sit in a processor's hands.
    fn take_batch(&self, max: usize) -> Vec<ReclaimEntry> {
        let mut out = Vec::new();
        while out.len() < max {
            self.processing.fetch_add(1, Ordering::AcqRel);
            match self.q.pop() {
                Some(e) => {
                    self.len.fetch_sub(1, Ordering::AcqRel);
                    out.push(e);
                }
                None => {
                    self.processing.fetch_sub(1, Ordering::AcqRel);
                    break;
                }
            }
        }
        out
    }

    /// Synchronously drain the queue to empty AND wait out in-flight
    /// batches. Callers: the ENOSPC pressure valve (the caller counts
    /// `block_free_reclaim_sync_drains`), unmount teardown, and tests via
    /// `BackendRouter::reclaim_drain`. Blocking by design — run it on the
    /// blocking pool from async contexts unless you ARE the emergency
    /// path (the valve blocks its worker briefly; ENOSPC is rarer and
    /// worse).
    pub fn drain_sync(&self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let batch = self.take_batch(self.batch_blocks as usize);
            if !batch.is_empty() {
                self.process_entries(batch);
                continue;
            }
            if self.processing.load(Ordering::Acquire) == 0 && self.len.load(Ordering::Acquire) == 0
            {
                return;
            }
            if std::time::Instant::now() > deadline {
                log::error!(
                    "block-reclaim drain timed out waiting for in-flight batches \
                     (processing={}, queued={}) — proceeding; allocation may \
                     refuse StorageFull honestly",
                    self.processing.load(Ordering::Relaxed),
                    self.len.load(Ordering::Relaxed)
                );
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Gauge accessor (tests / stats).
    pub fn queued_len(&self) -> u64 {
        self.len.load(Ordering::Acquire)
    }

    /// Process owned entries: group by device, coalesce adjacent ranges,
    /// issue the reclaim, then `finish_free` each offset. Counters stay
    /// PER-BLOCK (`block_free_discards ≡ displaced blocks` — the field
    /// ledger) even when ranges merge into one device command.
    ///
    /// Panic-safe accounting: the guard reconciles `processing` and the
    /// byte gauge for every entry, processed or unwound.
    fn process_entries(&self, entries: Vec<ReclaimEntry>) {
        struct BatchGuard<'a> {
            q: &'a ReclaimQueue,
            remaining: usize,
            bytes_remaining: u64,
        }
        impl Drop for BatchGuard<'_> {
            fn drop(&mut self) {
                self.q
                    .processing
                    .fetch_sub(self.remaining as u64, Ordering::AcqRel);
                METRICS
                    .block_free_reclaim_queue_bytes
                    .fetch_sub(self.bytes_remaining, Ordering::Relaxed);
            }
        }
        let mut guard = BatchGuard {
            q: self,
            remaining: entries.len(),
            bytes_remaining: entries.iter().map(|e| e.size).sum(),
        };

        // Fence check ONCE per batch, before any device command (contract
        // 6): a fenced holder must never issue destructive device I/O —
        // on detection-grade (non-PR) substrates a zombie's discard can
        // destroy the successor writer's reallocated blocks. Halted
        // entries drop here WITHOUT finish_free (the successor's journal
        // replay/recovery walk owns the accounting — the same posture as
        // the kill-9 crash story: un-returned thin space, re-covered on
        // reuse); the batch guard reconciles `processing` + the byte
        // gauge, and the entries' in-flight registrations deregister on
        // drop.
        if self.fence_halted() {
            METRICS
                .block_free_reclaim_fence_halts
                .fetch_add(entries.len() as u64, Ordering::Relaxed);
            return;
        }

        let mut by_dev: BTreeMap<String, Vec<ReclaimEntry>> = BTreeMap::new();
        for e in entries {
            by_dev.entry(e.device_path.clone()).or_default().push(e);
        }
        for (device_path, mut group) in by_dev {
            group.sort_by_key(|e| e.offset);
            reclaim_device_group(&device_path, &group);
            for e in group {
                // finish_free AFTER the reclaim — the offset becomes
                // reallocatable only now (the pre-existing free-window
                // law, unchanged; a new owner's DMA can never race the
                // discard). The in-flight guard drops with the entry.
                e.allocator.finish_free(e.offset);
                guard.remaining -= 1;
                guard.bytes_remaining -= e.size;
                self.processing.fetch_sub(1, Ordering::AcqRel);
                METRICS
                    .block_free_reclaim_queue_bytes
                    .fetch_sub(e.size, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for ReclaimQueue {
    fn drop(&mut self) {
        // Wake a parked worker so it can observe the dead Weak and exit
        // (no leaked tasks). Entries still queued here drop with the
        // queue: process-exit shape — see the module crash posture.
        self.notify.notify_waiters();
    }
}

/// Issue the device reclaim for one device's sorted entry group,
/// coalescing adjacent `[offset, offset+size)` ranges into single device
/// commands. Per-block counting (see `process_entries`).
///
/// The engine is the shim-write-amplification fix verbatim
/// (`.benchmarks/2026-07-27-shim-write-amplification.md`): `PUNCH_HOLE`
/// on regular-file backings (host-FS sparse reclaim), `BLKDISCARD` on
/// block devices (NVMe DSM Deallocate — no data payload, never
/// write-bandwidth-accounted; the former unconditional PUNCH_HOLE was
/// `blkdev_issue_zeroout` there). Refused/unsupported reclaims are
/// SKIPPED and counted — never degraded into a zeroing write; freed
/// ranges are never read (hole semantics + write-before-publish + the
/// incarnation seqlock).
#[cfg(target_os = "linux")]
fn reclaim_device_group(device_path: &str, group: &[ReclaimEntry]) {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::io::AsRawFd;

    let Ok(file) = std::fs::OpenOptions::new().write(true).open(device_path) else {
        METRICS
            .block_free_reclaim_skipped
            .fetch_add(group.len() as u64, Ordering::Relaxed);
        return;
    };
    let mode = file.metadata().map(|m| m.mode()).unwrap_or(0);
    let fd = file.as_raw_fd();
    let op = free_reclaim_op(mode);
    if matches!(op, FreeReclaimOp::Skip) {
        METRICS
            .block_free_reclaim_skipped
            .fetch_add(group.len() as u64, Ordering::Relaxed);
        return;
    }

    // Coalesce adjacent ranges: (start, len, blocks_covered).
    let mut ranges: Vec<(u64, u64, u64)> = Vec::new();
    for e in group {
        match ranges.last_mut() {
            Some((start, len, k)) if *start + *len == e.offset => {
                *len += e.size;
                *k += 1;
            }
            _ => ranges.push((e.offset, e.size, 1)),
        }
    }

    for (start, len, k) in ranges {
        match op {
            FreeReclaimOp::FilePunch => {
                let r = unsafe {
                    libc::fallocate(
                        fd,
                        libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                        start as libc::off_t,
                        len as libc::off_t,
                    )
                };
                if r == 0 {
                    METRICS
                        .block_free_file_punches
                        .fetch_add(k, Ordering::Relaxed);
                    METRICS
                        .block_free_punch_bytes
                        .fetch_add(len, Ordering::Relaxed);
                } else {
                    METRICS
                        .block_free_reclaim_skipped
                        .fetch_add(k, Ordering::Relaxed);
                }
            }
            FreeReclaimOp::BdevDiscard => {
                // BLKDISCARD = _IO(0x12, 119): REQ_OP_DISCARD (NVMe DSM
                // Deallocate). Not in the libc crate's const table.
                const BLKDISCARD: libc::c_ulong = 0x1277;
                let range: [u64; 2] = [start, len];
                let r = unsafe { libc::ioctl(fd, BLKDISCARD as _, range.as_ptr()) };
                if r == 0 {
                    METRICS.block_free_discards.fetch_add(k, Ordering::Relaxed);
                    METRICS
                        .block_free_discard_bytes
                        .fetch_add(len, Ordering::Relaxed);
                } else {
                    // Unsupported/refused deallocate: skip loud-once in
                    // the counter, NEVER a zeroing-write fallback.
                    METRICS
                        .block_free_reclaim_skipped
                        .fetch_add(k, Ordering::Relaxed);
                }
            }
            FreeReclaimOp::Skip => unreachable!("filtered above"),
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn reclaim_device_group(_device_path: &str, group: &[ReclaimEntry]) {
    METRICS
        .block_free_reclaim_skipped
        .fetch_add(group.len() as u64, Ordering::Relaxed);
}
