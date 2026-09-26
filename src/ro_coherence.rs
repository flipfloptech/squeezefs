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
//! | **3 — the freed-offset grace period** | [`crate::free_grace`] — the ring, the bound, the fence; this module drives the READER half ([`crate::ro_coherence::spawn_reader_revalidation`] emits the acknowledgement at the end of every purging pass) |
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
//! staleness bounded by one poll interval. **Eliminating** that window is
//! §6.8 item 3, the freed-offset grace period, and it is now BUILT
//! ([`crate::free_grace`]): a terminally-freed offset does not re-enter the
//! free list until every live registered reader has acknowledged passing
//! it, so there is no instant at which a reader can resolve — or serve
//! cached bytes for — an offset a different file already owns.
//!
//! **Two statements coexist, and both must be told correctly**, because the
//! plane that carries the acknowledgements is opt-in
//! (`SQUEEZEFS_MEMBERSHIP_BIND`, default `off` — ruling D11 defers the
//! measurement that would justify a new default):
//!
//! * **Plane armed** (the writer serves membership and the reader joined):
//!   the data window is **eliminated**, not merely bounded. Live proof is
//!   `free_grace_mode == "armed"` on the writer's stats inode with
//!   `free_grace_forced_releases == 0` — a forced release is the one arm
//!   that trades a reader's coherence for the writer's progress, and it
//!   only ever happens together with that reader's eviction.
//! * **Plane off** (the shipped default): unchanged from S5 — the window is
//!   bounded by one revalidation interval, loud on a transformed volume
//!   (AEAD tag failure) and **silent on a passthrough volume**.
//!
//! Why the channel had to be S6 and not the spec's `client:` heartbeat: a
//! reader **cannot write that record** — it is an xattr commit on ino 1
//! under an exclusive `I{1}` guard, i.e. a metadata write, which item 1
//! refuses by contract (and §6.5 pt 3 measures that plane saturating at
//! ~4,550 clients anyway). Full pre-item-3 assessment:
//! `.benchmarks/2026-08-05-dlm-s5-readonly-mount.md` §4. `docs/operations.md`
//! states both postures to operators in these terms. Do not describe an S5
//! reader as "coherent" without saying which of the two it is.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cache::TieredCache;
use crate::meta_backend::kv::backend::{KvMetaBackend, ReadOnlyCause};
use crate::meta_backend::kv::node_cache::EpochPurgeSink;
use crate::meta_backend::kv::revalidate::RevalidationPoller;

// ---------------------------------------------------------------------------
// The reader's purge GENERATION — the epoch-step stamp on the layout cache
// (ladder re-derivation item 2, 2026-09-06 —
// `.benchmarks/2026-09-06-free-grace-ladder-rederivation.md`)
// ---------------------------------------------------------------------------

/// The reader's purge generation: bumped once per epoch step, AFTER that
/// volume's root adoption, drop pass and R-6 purge ([`ReaderEpochPurge`]).
/// Every layout-cache entry is stamped with the generation read BEFORE its
/// backend read ([`crate::routing::CachedMetadata::reader_step_gen`]), and
/// an entry stamped below the current generation is a MISS
/// ([`layout_entry_pre_step`]) — so no daemon cache can serve a
/// pre-step binding after the step, and the acknowledgement ladder's drain
/// no longer has to wait out the caches' TTL (`S`).
///
/// Never advances on a mount that revalidates nothing (a plain writer):
/// every entry there is stamped 0 and the gate is `0 < 0`, false — one
/// relaxed load and a compare, no behaviour change.
///
/// Ordering: the bump is `SeqCst` and the fetch-side capture is `Acquire`.
/// A fetch whose capture read the bumped value synchronizes with the bump,
/// which is sequenced after the root adoption, so its KV read sees the
/// adopted roots (stamped new, content new); a fetch that read the old
/// value is stamped old and misses after the bump regardless of what its
/// read saw. Either way a pre-step binding never carries a post-step stamp.
static READER_STEP_GEN: AtomicU64 = AtomicU64::new(0);

/// Layout-cache entries the step gate refused (`reader_layout_step_misses`
/// — item 2's engagement: each is one re-resolve a TTL would have deferred).
static LAYOUT_STEP_MISSES: AtomicU64 = AtomicU64::new(0);

/// The generation to STAMP a layout-cache entry with — read at the start
/// of the entry's backend resolution, never after it (a step landing
/// mid-read would otherwise give pre-step content a post-step stamp).
#[inline]
pub fn reader_step_generation() -> u64 {
    READER_STEP_GEN.load(Ordering::Acquire)
}

/// The gate: `true` ⇔ an entry stamped `stamped_gen` predates the last
/// epoch step and may not be served (the lever on). One relaxed load and a
/// compare on the read hot path; the gate's own cost when unarmed is that
/// load against a generation that never moved.
#[inline]
pub fn layout_entry_pre_step(stamped_gen: u64) -> bool {
    if stamped_gen >= READER_STEP_GEN.load(Ordering::Relaxed) {
        return false;
    }
    if !drain_epoch_stamp_enabled() {
        return false;
    }
    LAYOUT_STEP_MISSES.fetch_add(1, Ordering::Relaxed);
    true
}

/// `reader_layout_step_gen` (the generation; 0 on every mount that never
/// stepped an epoch).
pub fn reader_layout_step_gen() -> u64 {
    READER_STEP_GEN.load(Ordering::Relaxed)
}

/// `reader_layout_step_misses`.
pub fn reader_layout_step_misses() -> u64 {
    LAYOUT_STEP_MISSES.load(Ordering::Relaxed)
}

/// The epoch step's bump — the purge sink's last act, after that volume's
/// root adoption, drop pass and block-key purge. `SeqCst`: it is the
/// ladder's half of the Dekker pair with every serve stamp
/// ([`ServeStamp::begin`]). With the serve ledger armed the new
/// generation's slot is re-stamped, and a slot still holding occupants
/// (a serve alive across [`SERVE_GEN_SLOTS`] steps) is POISONED: counted
/// as pre-step for every candidate until it reads zero — over-conservative
/// (a saturated reader may then wait for the whole slot), never unsafe.
fn note_reader_epoch_step() {
    let new = READER_STEP_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    if !LEDGER_ARMED.load(Ordering::Relaxed) {
        return;
    }
    let slot = (new % SERVE_GEN_SLOTS as u64) as usize;
    if slot_total(slot) != 0 {
        POISONED_SLOTS.fetch_or(1u64 << slot, Ordering::SeqCst);
        SERVE_SLOT_OVERRUNS.fetch_add(1, Ordering::Relaxed);
        crate::note_invariant_tripwire(
            "reader_serve_slot_overrun",
            &format!(
                "a read serve stamped with purge generation {} is still in flight {} epoch \
                 steps later — its slot is poisoned (counted pre-step until it drains) and the \
                 acknowledgement ladder waits; a serve this old is a wedged I/O, not load",
                new.saturating_sub(SERVE_GEN_SLOTS as u64),
                SERVE_GEN_SLOTS
            ),
        );
    }
    SLOT_GEN[slot].store(new, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// The reader's in-flight SERVE ledger — the OBSERVED drain (ladder
// re-derivation item 3, 2026-09-06)
// ---------------------------------------------------------------------------
//
// Gate 3's second half — "every serve that resolved a binding before the
// step has finished" — was a TIMER: §6.7's `D_purge = 2 × P` (2,000 ms on
// the fleet), a lease-clock fail-stop reserve reused as a serve drain. No
// timer bounds a serve (one stuck behind a fabric timeout takes seconds),
// and the serves themselves finish in milliseconds, so the drain is made
// EXACT instead: every read serve stamps the purge generation it started
// under and counts itself in that generation's slot; the ladder's drained
// condition for a candidate qualified after step `G` is "every slot whose
// generation is below `G` reads zero". `D_purge` survives only as a loud
// tripwire (`free_grace_drain_overdue`) that never shortens the wait.
//
// Why "reads zero" is a proof and not a race, in three parts:
//
// 1. **Pairs land on ONE word.** A stamp increments the word
//    `shards[its thread's shard][gen % SLOTS]` and its completion
//    decrements the SAME word (the stamp carries its shard), so every word
//    is a non-negative count in its own modification order — a decrement
//    can never be observed ahead of its increment — and a sum of zero over
//    the pre-step slots means every observed start has completed.
// 2. **A start the ladder did not observe is post-step (Dekker).** The
//    step's generation bump is a `SeqCst` RMW on the revalidation task,
//    sequenced before the ladder's `SeqCst` slot loads on that task; a
//    stamp is a `SeqCst` increment followed by a `SeqCst` re-read of the
//    generation. In the single total order of `SeqCst` operations, if the
//    ladder's load precedes the increment then the bump precedes the
//    re-read, so the re-read sees the bumped generation — and a load that
//    reads the bump synchronizes with it, which is sequenced after the
//    volume's root adoption: everything that serve resolves afterwards
//    sees the adopted roots and the current generation
//    ([`layout_entry_pre_step`]'s relaxed load is coherent after it).
//    Either the ladder counts the serve or the serve is post-step.
// 3. **Slot aliasing is detected and conservative.** A slot is reused
//    every [`SERVE_GEN_SLOTS`] steps (≥ 64 s at one step per second per
//    volume); a serve still alive then is a wedged I/O, its slot is
//    poisoned at the re-stamp and counted pre-step for every candidate
//    until the slot reads zero, and the tripwire names it.
//
// Per-op cost: one relaxed armed-word load, one relaxed generation load,
// one thread-local shard index, one `SeqCst` increment, one `SeqCst`
// generation re-read at the start; one `SeqCst` decrement and one relaxed
// wait-word load at the end (a `lock xadd` is the same instruction at any
// ordering on x86; the ARM fence is the price of the proof). No lock, no
// copy, no clock read. Unarmed (every plain writer): one relaxed load.

/// Generation slots: a serve stamped `g` is counted in slot `g % SLOTS`.
/// `u64::BITS`, the poison mask's width — a representation bound, not a
/// tuning: the slot count only has to exceed the number of epoch steps
/// a serve can span before the fail-stop tripwire has long fired.
const SERVE_GEN_SLOTS: usize = u64::BITS as usize;

/// One thread's shard: its own 64 slot words on its own cache lines (the
/// fuse3 `ShardedCounter` law — shards never share a line).
#[repr(align(64))]
struct ServeShard([std::sync::atomic::AtomicI64; SERVE_GEN_SLOTS]);

/// `true` ⇔ a revalidating mount armed the ledger (the cadence task's
/// spawn, with the observed-drain lever on). THE hot-path word.
static LEDGER_ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SERVE_SHARDS: std::sync::OnceLock<Vec<ServeShard>> = std::sync::OnceLock::new();
/// The generation each slot currently counts (re-stamped at the step).
static SLOT_GEN: [AtomicU64; SERVE_GEN_SLOTS] = [const { AtomicU64::new(0) }; SERVE_GEN_SLOTS];
/// Bitmask of slots that still held occupants when re-stamped.
static POISONED_SLOTS: AtomicU64 = AtomicU64::new(0);
/// The highest generation the ladder is waiting to drain below (0 = none):
/// a completion below it re-sums its slot and wakes the revalidation task
/// when the slot reached zero.
static DRAIN_WAIT_GEN: AtomicU64 = AtomicU64::new(0);
/// The revalidation task's drain wake (parked beside its cadence).
static DRAIN_WAKE: squeezefs_ipc::sqz_notify::Notify = squeezefs_ipc::sqz_notify::Notify::new();
/// Stamps whose generation re-read saw a step land between the stamp's
/// generation load and its increment (`reader_serve_step_races` — the
/// Dekker recheck engaging; such a serve is counted one slot early,
/// conservative).
static SERVE_STEP_RACES: AtomicU64 = AtomicU64::new(0);
/// Slots poisoned at re-stamp (`reader_serve_slot_overruns`, must-stay-0).
static SERVE_SLOT_OVERRUNS: AtomicU64 = AtomicU64::new(0);
/// Completions that woke the revalidation task (`free_grace_drain_wakes`).
static DRAIN_WAKES: AtomicU64 = AtomicU64::new(0);

/// Shard count: the daemon's divided sizing root
/// (`cpu::process_parallelism` — the KD-MW-14 fleet share applied) rounded
/// up to a power of two, railed to [1, 64] (the fuse3 `read_phase::
/// shard_count` derivation over the daemon's root).
fn serve_shard_count() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        crate::cpu::process_parallelism()
            .next_power_of_two()
            .clamp(1, 64)
    })
}

/// This thread's shard — assigned once per thread, round-robin (no
/// `sched_getcpu` per op; a migrating thread keeps its line).
fn serve_shard_index() -> usize {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    thread_local! {
        static MINE: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
    }
    MINE.with(|c| {
        let mut v = c.get();
        if v == usize::MAX {
            v = NEXT.fetch_add(1, Ordering::Relaxed) % serve_shard_count();
            c.set(v);
        }
        v
    })
}

fn serve_shards() -> &'static [ServeShard] {
    SERVE_SHARDS.get_or_init(|| {
        (0..serve_shard_count())
            .map(|_| {
                ServeShard(std::array::from_fn(|_| {
                    std::sync::atomic::AtomicI64::new(0)
                }))
            })
            .collect()
    })
}

/// One slot's total across shards (`SeqCst` loads — part of the proof's
/// total order).
fn slot_total(slot: usize) -> i64 {
    serve_shards()
        .iter()
        .map(|s| s.0[slot].load(Ordering::SeqCst))
        .sum()
}

/// A read serve's in-flight stamp: `begin` at the serve's START (before it
/// resolves any binding), dropped when the serve — the bytes it delivers,
/// or the fill it deposits — is complete. Inert (a no-op pair) when the
/// ledger is not armed. Not `Clone` on purpose: a second increment for one
/// serve would let the ladder observe the pair out of order across shards.
#[derive(Debug)]
pub struct ServeStamp {
    /// The stamped generation + 1; `0` = inert.
    gen_plus_one: u64,
    /// The shard the increment landed on — the decrement's target.
    shard: u32,
}

impl ServeStamp {
    /// Stamp one serve (see the module block above for the ordering law).
    #[inline]
    pub fn begin() -> Self {
        if !LEDGER_ARMED.load(Ordering::Relaxed) {
            return Self::inert();
        }
        let gen = READER_STEP_GEN.load(Ordering::Relaxed);
        let shard = serve_shard_index();
        serve_shards()[shard].0[(gen % SERVE_GEN_SLOTS as u64) as usize]
            .fetch_add(1, Ordering::SeqCst);
        // The Dekker re-read: a step that landed between the load and the
        // increment is observed here — the serve stays counted one slot
        // early (pre-step for the candidate, conservative) and the ladder
        // either saw the increment or this read saw the bump.
        if READER_STEP_GEN.load(Ordering::SeqCst) != gen {
            SERVE_STEP_RACES.fetch_add(1, Ordering::Relaxed);
        }
        Self {
            gen_plus_one: gen + 1,
            shard: shard as u32,
        }
    }

    /// A stamp that counts nothing (the unarmed mount's, and the value a
    /// caller holds before it decides to serve).
    pub const fn inert() -> Self {
        Self {
            gen_plus_one: 0,
            shard: 0,
        }
    }
}

impl Drop for ServeStamp {
    #[inline]
    fn drop(&mut self) {
        if self.gen_plus_one == 0 {
            return;
        }
        let gen = self.gen_plus_one - 1;
        let slot = (gen % SERVE_GEN_SLOTS as u64) as usize;
        serve_shards()[self.shard as usize].0[slot].fetch_sub(1, Ordering::SeqCst);
        // Wake the ladder when this completion may have drained a slot it
        // waits on: the wait word is stored `SeqCst` BEFORE the ladder's
        // slot loads, so a completion the ladder's read missed sees the
        // wait and re-sums (the last completion in the total order reads
        // zero) — no lost wake by construction; the ticked park is the
        // backstop regardless.
        if gen < DRAIN_WAIT_GEN.load(Ordering::SeqCst) && slot_total(slot) == 0 {
            DRAIN_WAKES.fetch_add(1, Ordering::Relaxed);
            DRAIN_WAKE.notify_one();
        }
    }
}

/// Arm the ledger — the revalidation cadence task's spawn on a mount whose
/// observed-drain lever is on. Never disarmed for the mount's life.
pub fn arm_serve_ledger() {
    LEDGER_ARMED.store(true, Ordering::SeqCst);
}

/// `true` ⇔ serves are being counted (the ladder's observed arm is live).
pub fn serve_ledger_armed() -> bool {
    LEDGER_ARMED.load(Ordering::Relaxed)
}

/// **The ladder's drained condition** for a candidate qualified after the
/// pass whose steps left the generation at `gen`: every slot whose
/// generation is below `gen` — or that is poisoned — reads zero. Clears the
/// poison of a slot that reads zero (every occupant it ever held is done).
/// `true` on an unarmed ledger (nothing was ever counted).
pub fn serve_drained_below(gen: u64) -> bool {
    if !LEDGER_ARMED.load(Ordering::Relaxed) {
        return true;
    }
    let poisoned = POISONED_SLOTS.load(Ordering::SeqCst);
    let mut inflight = 0i64;
    let mut cleared = 0u64;
    for slot in 0..SERVE_GEN_SLOTS {
        let bit = 1u64 << slot;
        if SLOT_GEN[slot].load(Ordering::SeqCst) >= gen && poisoned & bit == 0 {
            continue;
        }
        let total = slot_total(slot);
        if total == 0 {
            cleared |= poisoned & bit;
        } else {
            inflight += total;
        }
    }
    if cleared != 0 {
        POISONED_SLOTS.fetch_and(!cleared, Ordering::SeqCst);
    }
    inflight == 0
}

/// The ladder publishes the highest generation it is waiting to drain
/// below (0 = nothing waiting) BEFORE it reads the slots.
pub fn set_drain_wait_gen(gen: u64) {
    DRAIN_WAIT_GEN.store(gen, Ordering::SeqCst);
}

/// The drain wake the revalidation task parks on beside its cadence.
pub fn drain_wake() -> &'static squeezefs_ipc::sqz_notify::Notify {
    &DRAIN_WAKE
}

/// `reader_serves_inflight`: read serves counted right now, all slots.
pub fn serves_inflight() -> i64 {
    if !LEDGER_ARMED.load(Ordering::Relaxed) {
        return 0;
    }
    (0..SERVE_GEN_SLOTS).map(slot_total).sum()
}

/// `reader_serve_step_races`.
pub fn serve_step_races() -> u64 {
    SERVE_STEP_RACES.load(Ordering::Relaxed)
}

/// `reader_serve_slot_overruns` (must-stay-0).
pub fn serve_slot_overruns() -> u64 {
    SERVE_SLOT_OVERRUNS.load(Ordering::Relaxed)
}

/// `free_grace_drain_wakes`.
pub fn drain_wakes() -> u64 {
    DRAIN_WAKES.load(Ordering::Relaxed)
}

/// The `SQUEEZEFS_FREE_GRACE_DRAIN_OBSERVED` lever's latch: item 3 — the
/// drain is the observed in-flight condition and `D_purge` a tripwire.
/// `0` = the `D_purge` timer (the pre-change shape verbatim; the ledger
/// is not armed, so the hot path pays one relaxed load).
static DRAIN_OBSERVED: AtomicU8 = AtomicU8::new(0);

pub fn drain_observed_enabled() -> bool {
    match DRAIN_OBSERVED.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = crate::env_knobs::bool_knob("SQUEEZEFS_FREE_GRACE_DRAIN_OBSERVED", true);
            DRAIN_OBSERVED.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            on
        }
    }
}

/// Test seam (the `free_grace::test_set_ack_pipeline` shape).
pub fn test_set_drain_observed(on: Option<bool>) {
    DRAIN_OBSERVED.store(
        match on {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        },
        Ordering::Relaxed,
    );
}

/// Test seam: `true` ⇔ a preset was in force.
pub fn test_clear_drain_observed() -> bool {
    DRAIN_OBSERVED.swap(0, Ordering::Relaxed) != 0
}

/// Test seam: arm the ledger without a revalidation task.
pub fn test_arm_serve_ledger() {
    arm_serve_ledger();
}

/// Test seam: an epoch step without a volume (the loop model and the
/// cache-gate contracts).
pub fn test_note_epoch_step() {
    note_reader_epoch_step();
}

/// **The token recall's drain** (design-symmetric-metadata §5.7.3, PR
/// 5): a recall is a step on the reader's layout cache — every layout
/// entry stamped before it misses ([`layout_entry_pre_step`]) — followed
/// by the OBSERVED drain of every serve that began under an earlier
/// generation ([`serve_drained_below`]; with the ledger unarmed there is
/// nothing to wait for). Parked on the drain wake, never a timer; what a
/// reader runs BEFORE it acks a recall, so the holder's terminal free
/// can never race a DMA the reader still has in flight.
pub async fn drain_in_flight_serves() {
    note_reader_epoch_step();
    let gen = reader_step_generation();
    set_drain_wait_gen(gen);
    loop {
        let notified = DRAIN_WAKE.notified();
        if serve_drained_below(gen) {
            break;
        }
        // The wake is `notify_one` and the revalidation task parks on the
        // same word: the ticked park is the backstop (the ladder's own
        // law), the tick the timer grain's order.
        let _ = squeezefs_ipc::sqz_time::timeout(Duration::from_millis(10), notified).await;
    }
    set_drain_wait_gen(0);
}

/// The `SQUEEZEFS_FREE_GRACE_DRAIN_EPOCH_STAMP` lever's latch (the
/// `free_grace::ACK_PIPELINE` pattern): item 2 — the layout-cache step
/// gate is live and the ladder's drain drops its `S` term. `0` = the
/// caches serve to their TTL and the drain keeps `S` (the pre-change
/// shape verbatim; the generation keeps counting — the instrument is not
/// the mechanism).
static DRAIN_EPOCH_STAMP: AtomicU8 = AtomicU8::new(0);

pub fn drain_epoch_stamp_enabled() -> bool {
    match DRAIN_EPOCH_STAMP.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = crate::env_knobs::bool_knob("SQUEEZEFS_FREE_GRACE_DRAIN_EPOCH_STAMP", true);
            DRAIN_EPOCH_STAMP.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            on
        }
    }
}

/// Test seam (the `free_grace::test_set_ack_pipeline` shape).
pub fn test_set_drain_epoch_stamp(on: Option<bool>) {
    DRAIN_EPOCH_STAMP.store(
        match on {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        },
        Ordering::Relaxed,
    );
}

/// Test seam: `true` ⇔ a preset was in force.
pub fn test_clear_drain_epoch_stamp() -> bool {
    DRAIN_EPOCH_STAMP.swap(0, Ordering::Relaxed) != 0
}

/// Test seam: return the generation, the gate's gauges and the serve
/// ledger to a fresh mount's (`free_grace::reset_for_test` calls it). A
/// stamp still held across a reset would decrement a zeroed word — the
/// contracts drop every stamp first.
pub fn reset_for_test() {
    READER_STEP_GEN.store(0, Ordering::SeqCst);
    LAYOUT_STEP_MISSES.store(0, Ordering::Relaxed);
    test_set_drain_epoch_stamp(None);
    test_set_drain_observed(None);
    LEDGER_ARMED.store(false, Ordering::SeqCst);
    if let Some(shards) = SERVE_SHARDS.get() {
        for s in shards {
            for w in &s.0 {
                w.store(0, Ordering::SeqCst);
            }
        }
    }
    for g in SLOT_GEN.iter() {
        g.store(0, Ordering::SeqCst);
    }
    POISONED_SLOTS.store(0, Ordering::SeqCst);
    DRAIN_WAIT_GEN.store(0, Ordering::SeqCst);
    for c in [&SERVE_STEP_RACES, &SERVE_SLOT_OVERRUNS, &DRAIN_WAKES] {
        c.store(0, Ordering::Relaxed);
    }
}

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

/// Whether this `-o ro` mount is a READ-TOKEN client (PR 5,
/// design-symmetric-metadata §5.7.2 — EVERY read-only mount since the
/// PR-14 flip: the S5 bounded-staleness posture retired with it; R-SYM-4
/// names tokens the ONLY foreign-read method). The volume gate (bit 17) is
/// the arm's to refuse, naming `enable-symmetric`.
pub fn token_reader_requested() -> bool {
    crate::fuse_client::read_only_mount()
}

/// **The user-visible METADATA staleness bound** in force for this
/// mount's caches (R-SYM-4): **zero** under tokens — a kernel or daemon
/// cache may hold an entry exactly as long as freshness is proven, and
/// under a token that is "until the recall", which no TTL can express, so
/// every TTL derives to 0 and every resolve reaches the token cache — else
/// the S5 posture's [`reader_staleness_bound`]. The poll keeps its own
/// bound as the control-plane cadence.
pub fn metadata_staleness_bound() -> Duration {
    if token_reader_requested() {
        Duration::ZERO
    } else {
        reader_staleness_bound()
    }
}

/// [`metadata_staleness_bound`] as the stats face publishes it
/// (`reader_staleness_bound_ms`): 0 under tokens — by the mount's request
/// or by an armed plane on any volume (the contracts arm the plane
/// directly) — else the S5 bound in ms.
pub fn metadata_staleness_bound_ms(volumes: &[Arc<KvMetaBackend>]) -> u64 {
    if token_reader_requested() || volumes.iter().any(|v| v.token_reader().is_some()) {
        return 0;
    }
    reader_staleness_bound().as_millis() as u64
}

/// **Explicit kernel TTLs on a token reader are refused** (review round
/// 1, Issue 13): every class derives to 0 under tokens
/// ([`metadata_staleness_bound`]), and the standing precedence lets an
/// explicit `SQUEEZEFS_FUSE_*_TTL_MS` or `-o *_timeout=` lengthen it —
/// which on a token reader re-creates a bounded-staleness dcache, the
/// second read method R-SYM-4 forbids, by lever. Loud, naming the class;
/// the owed kernel-side `notify_inval_entry` / `notify_inval_inode` on
/// recall is what would make a non-zero TTL legal.
pub fn refuse_explicit_ttls_under_tokens(
    ttls: &crate::fuse_client::KernelCacheTtls,
) -> std::result::Result<(), String> {
    if !token_reader_requested() {
        return Ok(());
    }
    let nonzero: Vec<&str> = [
        ("attr_timeout / SQUEEZEFS_FUSE_ATTR_TTL_MS", ttls.attr),
        ("entry_timeout / SQUEEZEFS_FUSE_ENTRY_TTL_MS", ttls.entry),
        (
            "dir_entry_timeout / SQUEEZEFS_FUSE_DIR_ENTRY_TTL_MS",
            ttls.dir_entry,
        ),
        (
            "negative_timeout / SQUEEZEFS_FUSE_NEGATIVE_TTL_MS",
            ttls.negative,
        ),
    ]
    .into_iter()
    .filter(|(_, d)| !d.is_zero())
    .map(|(name, _)| name)
    .collect();
    if nonzero.is_empty() {
        return Ok(());
    }
    Err(format!(
        "a -o ro mount with an explicit non-zero kernel cache TTL ({}): a read-only mount is a \
         READ-TOKEN client (every one since the PR-14 flip) and under read tokens every kernel \
         TTL derives to 0 — a dentry or attribute the kernel keeps past a recall is a \
         bounded-staleness read method, which design-symmetric-metadata R-SYM-4 forbids. Drop \
         the option",
        nonzero.join(", ")
    ))
}

/// **The mount-path arm of the token client** (§5.7.2 — `-o ro` =
/// member-reader + token client; called from `fuse_client::init` beside
/// the S5 arms): under [`token_reader_requested`] every read-only volume
/// of the set arms its [`crate::meta_ship::token_plane::TokenReaderPlane`]
/// against the volume's holder with the mount's data-plane recall sink
/// (the in-flight serve drain + the R-6 purge before every ack). Returns
/// the number of volumes armed — every read-only volume, since the PR-14
/// flip made the token client the ONE `-o ro` posture (no knob value
/// names another).
///
/// Refused LOUD (the mount fails), naming the remedy, when anything the
/// posture needs is absent: a bit-17-absent volume (no holder exists to
/// grant a token; a silent fall-back to the poll would be the second
/// method R-SYM-4 forbids), no membership lease (the holder judges an
/// unacked recall by the reader's lease — a leaseless reader would be
/// treated as dead at every recall while still serving from its cache),
/// no declared holder endpoint, no cluster secret. The holder's endpoint
/// is the set authority's S8 listener (`SQUEEZEFS_MW_AUTHORITY`, the
/// co-writer's declaration — a durable endpoint record and the per-slot
/// holder → endpoint binding are PR 12's join ladder, §5.1.6); the
/// reader's identity is its membership member id, the key the holder's
/// lease oracle reads.
pub async fn arm_token_readers(
    volumes: &[Arc<KvMetaBackend>],
    router: &crate::routing::DataRouter,
) -> std::result::Result<usize, String> {
    use crate::meta_ship::token_plane::{MountRecallSink, TokenClientConfig};
    if !token_reader_requested() {
        return Ok(0);
    }
    let Some(first) = volumes.first() else {
        return Ok(0);
    };
    for v in volumes {
        if !v.symmetric_forest() {
            return Err(format!(
                "{}: a -o ro mount of a volume that does not carry the symmetric forest \
                 (incompat bit 17 — a `--single-writer` format, or a pre-flip volume not yet \
                 converted): no holder exists to grant a read token on it, and a reader that \
                 fell back to the bounded-staleness poll would be the second read method \
                 design-symmetric-metadata R-SYM-4 forbids (the S5 projection retired with the \
                 PR-14 flip). A `--single-writer` volume has ONE writer by format class and no \
                 coherent reader; a pre-flip multi-writer-class set converts offline with \
                 `squeezefs volume enable-symmetric`",
                v.device_path().display()
            ));
        }
    }
    // The ack law's two terms are UNCONDITIONAL under tokens (review round
    // 1, Issue 6): the observed drain of in-flight serves and the layout
    // cache's epoch step are what the reader's ack ATTESTS — under S5 the
    // ring's timers stood behind their A/B levers; under tokens the recall
    // IS the qualification and there is no timer. A lever at `0` refuses
    // the mount (the ENG-10 way), and the serve ledger is armed here
    // whatever the S5 cadence task decided.
    if !drain_observed_enabled() {
        return Err(
            "a -o ro mount with SQUEEZEFS_FREE_GRACE_DRAIN_OBSERVED=0: a token reader acks a \
             recall only after the OBSERVED drain of its in-flight serves, and no timer stands \
             behind that ack (R-SYM-4 — no bounded second method). Unset the lever"
                .to_string(),
        );
    }
    if !drain_epoch_stamp_enabled() {
        return Err(
            "a -o ro mount with SQUEEZEFS_FREE_GRACE_DRAIN_EPOCH_STAMP=0: a recall steps the \
             reader's layout cache and every pre-step layout must miss, and no timer stands \
             behind that miss (R-SYM-4 — no bounded second method). Unset the lever"
                .to_string(),
        );
    }
    arm_serve_ledger();
    let Some(member) = crate::membership::installed_member() else {
        return Err(
            "a -o ro mount whose reader holds no membership lease: a token client IS a \
             member-reader (design-symmetric-metadata §5.7.2) — the holder judges an unacked \
             recall by the reader's lease, so a leaseless reader would be treated as dead at \
             every recall while serving from its cache. Arm SQUEEZEFS_MEMBERSHIP_BIND on the \
             writer (and its cluster listener, SQUEEZEFS_JOB_WIRE_BIND) so this mount joins as \
             member-reader"
                .to_string(),
        );
    };
    let Some(secret) = crate::membership::cluster_secret(first).await else {
        return Err(format!(
            "{}: a -o ro mount of a volume set that carries no job:enroll record — the cluster wire's root of trust (possession of volume access \
             IS cluster membership, ruling D2). Enable the writer's cluster listener \
             (SQUEEZEFS_JOB_WIRE_BIND) so the secret exists",
            first.device_path().display()
        ));
    };
    let mut armed = 0usize;
    let mut endpoint = String::new();
    for (ordinal, v) in volumes.iter().enumerate() {
        if !v.is_read_only() {
            continue;
        }
        // PR 12 — the MANAGER's endpoint (appender 0, the holder of every
        // unleased tree and its own) off the binding or DURABLE state: its
        // page's identity → its claim-set entry's published listener (the
        // join ladder's rung 7 writes it). Never a knob: the declared
        // authority is a RETIRED spelling under the plane (§6.1). Objects
        // in slots OTHER appenders lease are served by per-holder planes
        // the backend dials at the first resolve (`token_reader_for`).
        endpoint = match v.reader_holder_endpoint(0) {
            Some(e) => e.to_string(),
            None => match crate::sym_join::resolve_holder_endpoint(v, 0).await {
                Some(e) => e,
                None => {
                    return Err(format!(
                        "{}: a -o ro mount of a volume whose manager (appender 0) has published \
                         no listener — its claim-set entry carries no endpoint. The writer must \
                         be mounted (its join ladder publishes the S8 listener every token, \
                         custody and shipped step rides — design-symmetric-metadata §5.1.6 / \
                         §7.3); a reader dials no declared authority",
                        v.device_path().display()
                    ));
                }
            },
        };
        // One sink per volume: its recalls name LOCAL key inos, and the
        // router's layout cache is keyed by the global ino the volume's
        // ordinal maps them to.
        let sink = MountRecallSink::new(router.clone(), ordinal);
        let plane = v
            .arm_token_reader(TokenClientConfig {
                endpoint: endpoint.clone(),
                secret: secret.clone(),
                client_id: member.id().to_string(),
                volume: u16::try_from(ordinal).map_err(|_| {
                    format!(
                        "{}: volume ordinal {ordinal} exceeds the wire's u16",
                        v.device_path().display()
                    )
                })?,
            })
            .map_err(|e| e.to_string())?;
        plane.install_data_sink(sink as Arc<dyn crate::meta_ship::token_plane::RecallDataSink>);
        // The holder answers the arm's probe or the mount refuses: a
        // writer with its plane unarmed serves no token verbs, and a
        // reader that mounted anyway would answer EIO to every resolve.
        if let Err(e) = plane.probe().await {
            return Err(format!(
                "{}: a -o ro mount whose holder at {endpoint} does not serve read tokens for \
                 volume {ordinal} ({e}) — the writer must be mounted (its join ladder's \
                 listener carries the token verbs; the endpoint it published is what this \
                 reader dialed)",
                v.device_path().display()
            ));
        }
        armed += 1;
    }
    log::info!(
        "-o ro mount: {armed} metadata volume(s) read under TOKENS from {endpoint} as member \
         '{}' — user-visible metadata is exact at the next resolve (reader_staleness_bound_ms \
         = 0); the S5 poll stays the control plane (design-symmetric-metadata §5.7.2)",
        member.id()
    );
    Ok(armed)
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
        // Item 2: the step's LAST act — after this volume's root adoption
        // (the node cache's `publish`, before this sink runs), its drop
        // pass and the block-key purge — so a layout fetch that observes
        // the new generation observes the adopted roots.
        note_reader_epoch_step();
        log::debug!(
            "reader epoch {from_epoch} → {to_epoch}: {purged} cached block key(s) dropped \
             (R-6 unified purge); layout-cache generation {}",
            reader_layout_step_gen()
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

/// **Sweep row 15 (per-volume claim admission §5.4, risk R18): does THIS
/// volume need a revalidation cadence?**
///
/// The predicate is the volume's own read-only CAUSE, not a process latch.
///
/// `UnknownRoFeatureBits` is excluded deliberately: §4.11 is a WRITE mount
/// holding Layer A whose volume has no other appender, so there are no
/// foreign checkpoints for it to track.
pub fn volume_wants_revalidation(cause: ReadOnlyCause) -> bool {
    cause == ReadOnlyCause::ReaderMount
}

/// The subset of `volumes` this mount does NOT append to — the exact set
/// [`arm_reader_coherence`] and [`spawn_reader_revalidation`] operate over
/// (empty on an ordinary write mount, every volume on a reader).
pub fn revalidating_volumes(volumes: &[Arc<KvMetaBackend>]) -> Vec<Arc<KvMetaBackend>> {
    volumes
        .iter()
        .filter(|v| volume_wants_revalidation(v.read_only_cause()))
        .cloned()
        .collect()
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
    wake: Arc<squeezefs_ipc::sqz_notify::Notify>,
) -> squeezefs_ipc::sqz_channel::oneshot::Receiver<()> {
    let poller = RevalidationPoller::derived();
    let interval = poller.interval();
    // Item 3: from here every read serve on this mount counts itself in
    // the purge generation it started under, so the acknowledgement
    // ladder's drain is OBSERVED (the pre-step in-flight count reaching
    // zero) rather than timed. The lever's `0` leaves the ledger unarmed
    // — one relaxed load per serve — and the ladder on the `D_purge`
    // timer.
    if drain_observed_enabled() {
        arm_serve_ledger();
    }
    log::info!(
        "reader revalidation armed: {} volume(s), every {:?} (the derived cadence — one \
         ledger read per volume per pass; staleness bound {:?}; serve ledger {})",
        volumes.len(),
        interval,
        poller.staleness_bound(),
        if serve_ledger_armed() {
            "armed (observed drain)"
        } else {
            "off (D_purge timer)"
        },
    );
    crate::meta_exec::spawn_meta_join("reader_revalidation", async move {
        loop {
            // Park until the poll cadence elapses or a wake nudges us
            // early (the retired two-arm `select!` — a notify is only a
            // promptness hint, so the timeout IS the authority's cadence).
            // L2b (design-free-grace-sustain §5.2b): under a prodded
            // renewal the cadence tightens toward the checkpoint ceiling —
            // the qualify/promote vehicle runs more often; the qualify and
            // drain WINDOWS stay at their routine derivations, so the S5
            // staleness contract (an upper bound) only ever tightens.
            // Item 3: the drain wake is the third arm — the completion of
            // the last pre-step serve the ladder waits on wakes this task
            // so the promotion follows the drain by a round trip, not by
            // the rest of the pass cadence (the poller skips every volume
            // not yet due, so an early pass costs no ledger read).
            let member_now = crate::membership::installed_member()
                .map(|s| s.now_ms())
                .unwrap_or(0);
            let sleep_for = crate::free_grace::reader_pass_interval(interval, member_now);
            let _ = squeezefs_ipc::sqz_time::timeout(
                sleep_for,
                squeezefs_ipc::sqz_future::race2(wake.notified(), drain_wake().notified()),
            )
            .await;
            if stop_flag.load(Ordering::Acquire) {
                log::info!("reader revalidation stopping (dismount)");
                return;
            }
            // Spec §6.8 item 3: the pass's START is what qualifies an
            // acknowledgement (the ledger read must post-date the label
            // by the staleness bound), so it is captured BEFORE the
            // first volume is polled.
            let pass_start = Instant::now();
            let mut advanced_any = false;
            // One PASS over the whole set (a volume the cadence skipped —
            // a wake arrived early — contributes no entry).
            for (idx, res) in poller.poll_set_at(&volumes, pass_start).await {
                match res {
                    Ok(out) if out.advanced => {
                        advanced_any = true;
                        log::debug!(
                            "reader revalidation: {} epoch {} → {} ({} node(s) dropped, {} \
                                 retained, {} block key(s) purged)",
                            volumes[idx].device_path().display(),
                            out.from_epoch,
                            out.epoch,
                            out.dropped,
                            out.retained,
                            out.keys_purged,
                        );
                    }
                    // The inert poll — the common case under an idle
                    // writer, and deliberately free (no drop, no purge).
                    Ok(_) => {}
                    Err(e) => log::warn!(
                        "reader revalidation pass failed on {}: {e} (the reader keeps \
                         serving its current epoch and retries next pass)",
                        volumes[idx].device_path().display()
                    ),
                }
            }
            // **Spec §6.8 item 3 — where the acknowledgement is
            // emitted.** Right here, at the END of a pass, and only
            // when that pass ran the R-6 purge (`advanced_any`): the
            // ack means "I have FINISHED using anything freed at or
            // before this label", so it may not be emitted before the
            // purge that makes it true, nor before the drain window
            // that lets pre-purge serves and daemon-cached layouts
            // expire. `free_grace::reader_pass_completed` owns that
            // ladder; the value then rides the next lease renewal, so
            // the reader still writes nothing, anywhere. Inert on a
            // mount that is not a plane member.
            if let Some(label) =
                crate::free_grace::reader_pass_completed(reader_clock_ms(&pass_start), advanced_any)
            {
                log::debug!(
                    "reader acknowledged freed-offset label {label}: the writer may \
                         reallocate everything it freed at or before it (spec §6.8 item 3)"
                );
            }
        }
    })
}

/// The pass-start instant in the MEMBER's clock frame, in ms.
///
/// The ladder compares `pass_start` against `learned_at`, both of which are
/// member-clock readings, so the conversion has to go through the session's
/// own clock rather than a fresh `Instant` origin. A member-less mount has
/// no frame to convert into and the answer is unused.
fn reader_clock_ms(pass_start: &Instant) -> u64 {
    match crate::membership::installed_member() {
        Some(session) => session
            .now_ms()
            .saturating_sub(pass_start.elapsed().as_millis() as u64),
        None => 0,
    }
}
