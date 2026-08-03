//! **Spec §6.8 item 3 — the freed-offset grace period**
//! (`docs/pre-rc-engineering-spec.md` §6.3, §6.8 item 3, §6.9 S5/S9;
//! contracts in `tests/reader_free_grace_tests.rs`; operator surface
//! `docs/operations.md` §Read-only coherent mounts).
//!
//! # The hole
//!
//! §6.3, verbatim: the read path's serve proof is *"bytes for key K serve
//! for block b iff (a) the fetch was incarnation-valid and (b) the current
//! map still binds b → K"*, and **both premises are process-local**. So
//! when the writer overwrites a block (CoW), frees the offset, and the
//! allocator reissues it to a **different file**, a reader whose cached map
//! still binds `b → K` serves the other file's bytes with no error and no
//! counter. Worse, block keys are bare reusable device offsets (§6.2
//! item 6's gap), so even a reader with FRESH metadata can serve stale
//! CACHED BYTES for a reused key. On a transformed volume the AEAD tag
//! fails — loud, and the one honest degradation; on a **passthrough
//! volume, which is the default, it is silent**.
//!
//! S5 bounded that window to one revalidation interval (every epoch step
//! drops the reader's whole block-key census —
//! [`crate::ro_coherence::purge_reader_block_keys`]). This module
//! ELIMINATES it, which is why §6.8 calls item 3 *"the highest-value single
//! item in the coherence analysis"* and why §6.3 makes it a multi-writer
//! prerequisite (two writers each derive a private answer about a reused
//! offset; the same staleness becomes cross-writer corruption).
//!
//! # The mechanism, and where each half lives
//!
//! The spec's sentence is the design: *"the writer already maintains the
//! freed-offset log (the reclaim queue); refuse to reallocate an offset
//! until every registered reader has acknowledged passing that epoch,
//! riding the existing heartbeat. A reader that fails to acknowledge is
//! fenced, not waited on."*
//!
//! | Half | Where |
//! |---|---|
//! | the acknowledgement channel | DLM **S6** membership ([`crate::membership::MemberSession::ack_free_epoch`] → [`crate::membership::MembershipOwner::min_acked_free_epoch`]) — a reader performs no metadata write, so the `client:` heartbeat the spec named could never have carried it |
//! | the gate | [`GraceRing`], one per [`crate::block_allocator::BlockAllocator`]: a terminally-freed offset enters the ring INSTEAD of the free list, and only a satisfied bound (or a fence) publishes it |
//! | the enforcement point | `BlockAllocator::finish_free` (the ONE free-list publish) and the allocation funnel's harvest — structural, never asserted after the allocator has answered |
//! | the reader's side | [`ReaderAckLadder`], driven by the S5 revalidation task ([`crate::ro_coherence::spawn_reader_revalidation`]) |
//! | "fenced, not waited on" | [`MembershipOwner::evict`](crate::membership::MembershipOwner::evict) via [`force_progress`] |
//!
//! # Epoch identity: an owner-minted causal label
//!
//! The label a free is stamped with is **the owner's own monotonic instant**
//! ([`crate::membership::Grant::granted_at_owner_ms`], already on the wire
//! since S6), and a reader only ever echoes a label the owner HANDED it.
//! That makes the comparison **causal, not temporal**: two values of one
//! clock are compared, and no member ever reads a foreign clock as a
//! deadline (§6.7's law, which S6 states for lease deadlines, is
//! untouched).
//!
//! **Why not the revalidation epoch itself**, which is what the reader's
//! purge is keyed to and the obvious candidate: it is the per-volume A/B
//! root-ledger sequence — *N independent counters* — while the
//! acknowledgement channel is one `u64`. The composition that would make a
//! scalar sound (reader's MIN over volumes ≥ writer's MAX over volumes)
//! starves structurally in an unbalanced set, which is a measured field
//! shape and not a hypothetical: the 2026-07-30 meta-plane conviction found
//! one volume at ~21–25k device-writes/s beside a sibling at 0.00, and a
//! volume that never checkpoints never advances its sequence. A per-volume
//! VECTOR would be sound, but it needs a wider channel than S6 built, and
//! inventing a second reader→writer channel is exactly what item 3 was
//! blocked on for a wave.
//!
//! What the revalidation epoch DOES own is the **qualification**: the
//! reader may echo a label only after an epoch-step purge whose pass began
//! late enough for the writer's dereference to be in the record that pass
//! adopts (see [`ReaderAckLadder`]). So the honest reading is: the label
//! NAMES the acknowledgement, the revalidation epoch EARNS it.
//!
//! # Why this is not a second quarantine
//!
//! S7's [`crate::data_custody::BlockQuarantine`] is a close cousin and is
//! deliberately NOT reused. The two answer different questions with
//! different lifecycles:
//!
//! | | S7 quarantine | item 3 grace |
//! |---|---|---|
//! | question | can a possibly-live zombie still DMA into this offset? | can a reader still be holding a binding to it? |
//! | admission | a death event (rare, recovery-window population) | **every terminal free** (steady-state, per displaced block) |
//! | release | an external **drain proof** (PR preempt / proof of death) | a **monotone bound** that advances on its own, every beat |
//! | if it never comes | honestly-unavailable space forever (ENOSPC) | impossible: the bound is time-bounded by the fence |
//! | shape | unordered set keyed by cohort; release scans it | FIFO ordered by label; release pops the front |
//!
//! Sharing the quarantine would mean minting a `DeadEpoch` per free epoch
//! and paying `BlockQuarantine::release`'s full-map scan per release — a
//! per-free O(live entries) cost on the write path where the FIFO is O(1),
//! and a cohort-shaped release protocol for something that is a
//! high-water-mark comparison. What IS shared is the **enforcement seam**:
//! both gates hook the same two places in the allocator (defer the
//! free-list publish; publish on release), and they compose in a fixed
//! order — custody proof first, reader coherence second, so an offset
//! released by a drain proof enters grace rather than the free list.
//!
//! # The pressure ruling
//!
//! **A grace period never releases an unacknowledged offset.** A full store
//! whose free list is entirely in grace refuses `StorageFull` — promptly,
//! loudly, counted (`free_grace_alloc_stalls`) — exactly like S7's
//! quarantine, and for the same reason: handing out an offset a reader may
//! still resolve is silent cross-file corruption on a passthrough volume,
//! and a bounded availability loss is the lesser failure. The difference
//! from S7 is that this wait always ends by itself: allocation under
//! pressure evaluates the **pressure deadline** (one honest ack cycle,
//! rather than the routine bound of two), and past it the laggard is
//! FENCED — never bypassed. Progress therefore comes from an eviction that
//! is logged and counted, never from a silently broken promise.
//!
//! # Cost when unarmed (the shipped default)
//!
//! `SQUEEZEFS_MEMBERSHIP_BIND=off` is the default, so the common mount must
//! pay nothing: every entry point is one relaxed load of [`ARMED`] feeding
//! a never-taken branch, and no ring memory is ever touched
//! (`tests/reader_free_grace_tests.rs` contract 1 pins the behaviour, and
//! `benches/write_path_bench.rs`'s `free_grace` group prices the load).

use crate::error::{Result, SqueezefsError};
use crate::membership::{LeaseClock, LeaseClocks};
use arc_swap::ArcSwapOption;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// The process-wide plane: armed state, the published bound, the label clock
// ---------------------------------------------------------------------------

/// `true` ⇔ a membership OWNER is installed and at least one member holds a
/// lease. THE hot-path word: every gate entry point loads it relaxed and
/// branches away, so an unarmed mount pays one load and no memory traffic.
static ARMED: AtomicBool = AtomicBool::new(false);

/// The published reallocation bound — the minimum acknowledged label across
/// live members. `u64::MAX` = nobody can be holding a stale binding (no
/// plane, or no members), which releases everything.
static BOUND: AtomicU64 = AtomicU64::new(u64::MAX);

/// Live member count as of the last publish (the `off`/`armed` word's
/// input and a diagnostic).
static MEMBERS: AtomicU64 = AtomicU64::new(0);

static DEFERRALS: AtomicU64 = AtomicU64::new(0);
static RELEASES: AtomicU64 = AtomicU64::new(0);
static FORCED_RELEASES: AtomicU64 = AtomicU64::new(0);
static LAGGARD_FENCES: AtomicU64 = AtomicU64::new(0);
static ALLOC_STALLS: AtomicU64 = AtomicU64::new(0);
static HELD_OFFSETS: AtomicU64 = AtomicU64::new(0);
static HELD_BYTES: AtomicU64 = AtomicU64::new(0);
static READER_ACKS: AtomicU64 = AtomicU64::new(0);

/// The owner-side plane parameters. Held behind an `ArcSwapOption` (the
/// `membership::INSTALLED` / `dlm_slot::SLOT_OWNERS` precedent): replaced
/// wholesale at arm, dropped at disarm, read lock-free.
struct Plane {
    /// The SAME clock instance the [`crate::membership::MembershipOwner`]
    /// stamps its grants with — labels and acknowledgements must come from
    /// one clock or they are not comparable.
    clock: LeaseClock,
    /// The routine fence bound: how long an offset may wait on a laggard
    /// before the laggard is evicted.
    fence_ms: u64,
    /// The pressure fence bound (≤ `fence_ms`, never below one ack cycle):
    /// what allocation evaluates when it is about to refuse `StorageFull`.
    pressure_fence_ms: u64,
}

static PLANE: once_cell::sync::Lazy<ArcSwapOption<Plane>> =
    once_cell::sync::Lazy::new(ArcSwapOption::empty);

/// `true` ⇔ the gate is live (a plane is armed AND at least one member
/// holds a lease). One relaxed load.
#[inline]
pub fn armed() -> bool {
    ARMED.load(Ordering::Relaxed)
}

/// The published reallocation bound: an offset freed at label `L` may be
/// reallocated once this is `>= L`. `u64::MAX` when no reader can hold a
/// stale binding.
#[inline]
pub fn bound() -> u64 {
    BOUND.load(Ordering::Relaxed)
}

/// The routine fence bound in ms (`0` = no plane).
pub fn fence_bound_ms() -> u64 {
    PLANE.load().as_ref().map(|p| p.fence_ms).unwrap_or(0)
}

/// The pressure fence bound in ms (`0` = no plane).
pub fn pressure_bound_ms() -> u64 {
    PLANE
        .load()
        .as_ref()
        .map(|p| p.pressure_fence_ms)
        .unwrap_or(0)
}

/// The label a free happening NOW is stamped with, on the owner's clock
/// (`None` = no plane).
///
/// `+1` is load-bearing: a grant minted in the SAME millisecond may have
/// been handed out *before* this free, and a reader that echoes that label
/// would be claiming to have finished with something it never saw freed. A
/// label strictly greater than every already-issued grant's is the honest
/// stamp.
fn label_now() -> Option<u64> {
    PLANE.load().as_ref().map(|p| p.clock.now_ms() + 1)
}

/// The owner clock's current reading (`None` = no plane) — the deadline
/// arithmetic's input.
fn owner_now_ms() -> Option<u64> {
    PLANE.load().as_ref().map(|p| p.clock.now_ms())
}

/// Arm the writer-side plane with the DERIVED bounds (the production
/// path: [`crate::membership`]'s owner arm).
///
/// Refuses when `SQUEEZEFS_FREE_GRACE_MAX_MS` is set below one honest
/// acknowledgement cycle — that configuration would fence readers that are
/// behaving exactly as designed (the [`LeaseClocks::with_params`] refusal
/// discipline: an unsafe configuration must not arm).
pub fn arm_owner_plane(clock: LeaseClock, clocks: &LeaseClocks) -> Result<()> {
    let fence = resolve_fence_bound(clocks)?;
    // The pressure deadline is half the routine bound, floored at one ack
    // cycle: allocation is entitled to progress sooner than the routine
    // sweep, but never sooner than a healthy reader can answer.
    let cycle = ack_cycle(clocks);
    let pressure = (fence / 2).max(cycle).min(fence);
    arm_owner_plane_with(clock, fence, pressure);
    Ok(())
}

/// Arm with explicit bounds — the deterministic test seam (and the form
/// [`arm_owner_plane`] resolves into).
pub fn arm_owner_plane_with(clock: LeaseClock, fence: Duration, pressure: Duration) {
    PLANE.store(Some(Arc::new(Plane {
        clock,
        fence_ms: fence.as_millis() as u64,
        pressure_fence_ms: pressure.as_millis() as u64,
    })));
    log::info!(
        "freed-offset grace period armed (spec §6.8 item 3): a terminally-freed offset is not \
         reallocatable until every live reader has acknowledged passing it. Grace bound {:?} \
         (under space pressure {:?}), ring cap {} offsets per volume — past the bound a laggard \
         is FENCED, not waited on",
        fence,
        pressure,
        derived_ring_cap(),
    );
}

/// Disarm (owner disarm / unmount): the bound goes to `u64::MAX`, so every
/// held offset is released by the next harvest — nothing is stranded by
/// teardown.
pub fn disarm_owner_plane() {
    PLANE.store(None);
    MEMBERS.store(0, Ordering::Relaxed);
    BOUND.store(u64::MAX, Ordering::Relaxed);
    ARMED.store(false, Ordering::Release);
}

/// Publish the reallocation bound and the live member count — called by
/// the membership owner at join, leave, eviction and on its sweep cadence
/// (`MembershipOwner::refresh_free_grace_bound`).
///
/// The bound is recomputed there rather than on every renewal on purpose:
/// `min_acked_free_epoch` is O(members), and at 15,000 members × 1,500
/// beats/s that would be 22.5 M scans/s to learn something that can only
/// change when a renewal arrives. The cost of the cadence is that a
/// released offset's residence includes up to one renewal interval, which
/// is accounted for in [`ack_cycle`].
pub fn publish_bound(min_acked: u64, members: usize) {
    MEMBERS.store(members as u64, Ordering::Relaxed);
    let has_plane = PLANE.load().is_some();
    if members == 0 || !has_plane {
        BOUND.store(u64::MAX, Ordering::Relaxed);
        ARMED.store(false, Ordering::Release);
        return;
    }
    BOUND.store(min_acked, Ordering::Relaxed);
    ARMED.store(true, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Derivations
// ---------------------------------------------------------------------------

/// One held entry's RAM footprint: `(label, offset, size)` — three `u64`.
pub const GRACE_ENTRY_BYTES: u64 = 24;

/// The ring-cap floor, in entries. **Field-derived, not tuning**: the
/// measured saturated ingest of 12.7 GB/s
/// (`.benchmarks/2026-07-28-ingest-economy.md`) over one default
/// acknowledgement cycle (≈ 38 s with the shipped clocks) displaces
/// ≈ 120 k blocks at the 4 MiB shipped block size, so a smaller floor
/// would fence a healthy reader merely because the writer is fast.
const RING_CAP_FLOOR: u64 = 131_072;

/// How many offsets one harvest publishes. Matches the block-reclaim
/// drain's batch quantum (`SQUEEZEFS_RECLAIM_BATCH_BLOCKS` default 64) —
/// grace is that queue's downstream sibling, and matching it keeps one
/// free's worst-case publish burst identical to one reclaim batch's. Not a
/// resource cap: every free and every allocation harvests, so a deeper
/// ring simply drains over more calls.
pub const HARVEST_BATCH: usize = 64;

/// The ring cap in entries, from an R5 budget — the pure form (tie-tested).
///
/// `budget/1024 / GRACE_ENTRY_BYTES` keeps the ring below 0.1 % of the
/// memory budget, floored at [`RING_CAP_FLOOR`]. Deliberately NOT an R5
/// component: R5 components must be able to SHED, and the only shed
/// available here would be releasing unacknowledged offsets — the one thing
/// this mechanism exists to forbid. It is bounded instead, and the bound is
/// published (`free_grace_ring_cap`).
pub fn resolve_ring_cap(budget_bytes: u64) -> usize {
    (budget_bytes / 1024 / GRACE_ENTRY_BYTES).max(RING_CAP_FLOOR) as usize
}

/// The ring cap in force: `SQUEEZEFS_FREE_GRACE_MAX_OFFSETS` verbatim, else
/// the derivation over the live R5 budget.
pub fn derived_ring_cap() -> usize {
    match crate::env_knobs::opt_int_knob::<u64>("SQUEEZEFS_FREE_GRACE_MAX_OFFSETS") {
        Some(v) => v.max(1) as usize,
        None => resolve_ring_cap(crate::read_lane::effective_mem_budget()),
    }
}

/// **One honest acknowledgement cycle** — how long the plane's own numbers
/// say a healthy reader may take to answer for an offset freed now:
///
/// | Term | Why |
/// |---|---|
/// | `3 × renew_interval` | learn a label ≥ the free's (one beat), carry the acknowledgement home (one beat), and the owner's bound-publish cadence (one sweep) |
/// | `3 × staleness_bound` | qualify (the dereference must be durably checkpointed and in the record the reader's pass adopts), wait for that pass, and expire the daemon caches whose reader TTL *is* the staleness bound |
/// | `skew_max` | §6.7's clock-skew bound between the two hosts |
/// | `D_purge` | §6.7's "observe, then finish" term: in-flight serves drain |
///
/// Everything is a published number ([`crate::ro_coherence::reader_staleness_bound`]
/// and the S6 lease clocks), so the bound cannot drift from the machinery
/// in force.
pub fn ack_cycle(clocks: &LeaseClocks) -> Duration {
    let staleness = crate::ro_coherence::reader_staleness_bound();
    clocks.renew_interval * 3 + staleness * 3 + clocks.skew_max + clocks.d_purge
}

/// Resolve the routine fence bound from an explicit override (ms) and the
/// plane's clocks — the pure form.
///
/// Default = **two** acknowledgement cycles: one missed cycle is tolerated
/// before a member is called a laggard, mirroring S6's renewal discipline
/// (three attempts before the member's own deadline). An explicit value
/// below ONE cycle is **refused, never clamped**: it would evict readers
/// that are answering exactly as designed, and silently lengthening an
/// operator's number would hide that.
pub fn resolve_fence_bound_from(
    explicit_ms: Option<u64>,
    clocks: &LeaseClocks,
) -> Result<Duration> {
    let cycle = ack_cycle(clocks);
    match explicit_ms {
        None => Ok(cycle * 2),
        Some(ms) => {
            let d = Duration::from_millis(ms);
            if d < cycle {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "SQUEEZEFS_FREE_GRACE_MAX_MS={ms} is below one honest acknowledgement cycle \
                     ({cycle:?} = 3 × renewal interval + 3 × reader staleness bound + skew_max + \
                     D_purge): the freed-offset grace period would fence readers that are \
                     answering exactly as designed (spec §6.8 item 3 — 'a reader that fails to \
                     acknowledge is fenced', not one that is merely mid-cycle). Raise the value, \
                     or shorten the cycle through SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS / \
                     SQUEEZEFS_META_REVALIDATE_MS"
                )));
            }
            Ok(d)
        }
    }
}

/// The fence bound in force (reads the knob).
pub fn resolve_fence_bound(clocks: &LeaseClocks) -> Result<Duration> {
    resolve_fence_bound_from(
        crate::env_knobs::opt_int_knob::<u64>("SQUEEZEFS_FREE_GRACE_MAX_MS"),
        clocks,
    )
}

// ---------------------------------------------------------------------------
// The ring: one per allocator
// ---------------------------------------------------------------------------

struct GraceEntry {
    label: u64,
    offset: u64,
    size: u64,
}

/// One volume's freed-offset grace ring: terminally-freed offsets awaiting
/// the readers' acknowledgement, FIFO by label.
///
/// A `parking_lot::Mutex<VecDeque<..>>` rather than a lock-free queue on
/// purpose: the harvest must PEEK the front (a lock-free queue can only
/// pop, and pushing an ineligible entry back would destroy the label
/// order), and the label is read INSIDE the lock so the deque is ordered by
/// construction even when several threads free concurrently. The lock is
/// never held across anything but a push or a bounded pop run — no
/// allocation, no device I/O, no other lock — and it is never taken at all
/// on an unarmed mount (the [`armed`] load short-circuits first), which is
/// every mount that has no reader.
pub struct GraceRing {
    entries: parking_lot::Mutex<VecDeque<GraceEntry>>,
    /// Held-entry count, published for the lock-free fast path.
    len: AtomicUsize,
    /// Held device bytes (this volume's share of `free_grace_bytes`).
    bytes: AtomicU64,
    cap: usize,
}

impl Default for GraceRing {
    fn default() -> Self {
        Self::derived()
    }
}

impl GraceRing {
    /// A ring with an explicit cap (the test seam).
    pub fn new(cap: usize) -> Self {
        Self {
            entries: parking_lot::Mutex::new(VecDeque::new()),
            len: AtomicUsize::new(0),
            bytes: AtomicU64::new(0),
            cap: cap.max(1),
        }
    }

    /// A ring on the derived cap ([`derived_ring_cap`]).
    pub fn derived() -> Self {
        Self::new(derived_ring_cap())
    }

    /// Hold `offset` until the readers have acknowledged past it.
    ///
    /// `false` ⇔ the gate is not armed and the caller must publish the
    /// offset to the free list exactly as it always did — the unarmed
    /// mount's whole cost is the one relaxed load that answers this.
    pub fn defer(&self, offset: u64, size: u64) -> bool {
        if !armed() {
            return false;
        }
        let mut guard = self.entries.lock();
        // The label is read UNDER the lock: two threads freeing
        // concurrently then stamp in lock-acquisition order, so the deque
        // is label-ordered by construction and the harvest's front peek is
        // exact.
        let Some(label) = label_now() else {
            return false;
        };
        guard.push_back(GraceEntry {
            label,
            offset,
            size,
        });
        self.len.store(guard.len(), Ordering::Release);
        drop(guard);
        self.bytes.fetch_add(size, Ordering::Relaxed);
        DEFERRALS.fetch_add(1, Ordering::Relaxed);
        HELD_OFFSETS.fetch_add(1, Ordering::Relaxed);
        HELD_BYTES.fetch_add(size, Ordering::Relaxed);
        true
    }

    /// Release up to `max` offsets whose label the readers have
    /// acknowledged — the routine harvest, run at every terminal free and
    /// at the allocation funnel. The returned offsets are OWED a free-list
    /// publish by the caller (the allocator, which owns the free list).
    pub fn harvest(&self, max: usize) -> Vec<u64> {
        self.harvest_with(max, false)
    }

    /// The pressure harvest: identical, except the fence deadline is the
    /// PRESSURE bound (one acknowledgement cycle) rather than the routine
    /// one. It never releases an unacknowledged offset without evicting the
    /// member responsible — pressure buys promptness, never a broken
    /// promise (see the module docs' pressure ruling).
    pub fn harvest_pressure(&self, max: usize) -> Vec<u64> {
        self.harvest_with(max, true)
    }

    fn harvest_with(&self, max: usize, pressure: bool) -> Vec<u64> {
        if self.len.load(Ordering::Acquire) == 0 {
            return Vec::new();
        }
        let mut bound = bound();
        let oldest = {
            let guard = self.entries.lock();
            guard.front().map(|e| e.label)
        };
        let mut forced = false;
        if let Some(oldest) = oldest {
            if oldest > bound {
                let over_cap = self.len.load(Ordering::Acquire) >= self.cap;
                let deadline_ms = if pressure {
                    pressure_bound_ms()
                } else {
                    fence_bound_ms()
                };
                let now = owner_now_ms().unwrap_or(0);
                let expired = deadline_ms > 0 && now.saturating_sub(deadline_ms) >= oldest;
                if expired || over_cap {
                    let why = if expired {
                        "the grace bound expired"
                    } else {
                        "the grace ring reached its cap"
                    };
                    force_progress(oldest, why);
                    // Re-read: `force_progress` recomputes the true minimum
                    // (which may simply have been a stale cadence-published
                    // value) and republishes it after any eviction.
                    bound = self::bound();
                    forced = bound >= oldest;
                }
            }
        }
        let mut out = Vec::new();
        let mut released_bytes = 0u64;
        {
            let mut guard = self.entries.lock();
            while out.len() < max {
                match guard.front() {
                    Some(e) if e.label <= bound => {
                        let e = guard.pop_front().expect("front peeked");
                        released_bytes += e.size;
                        out.push(e.offset);
                    }
                    _ => break,
                }
            }
            self.len.store(guard.len(), Ordering::Release);
        }
        if !out.is_empty() {
            self.bytes.fetch_sub(released_bytes, Ordering::Relaxed);
            HELD_OFFSETS.fetch_sub(out.len() as u64, Ordering::Relaxed);
            HELD_BYTES.fetch_sub(released_bytes, Ordering::Relaxed);
            RELEASES.fetch_add(out.len() as u64, Ordering::Relaxed);
            if forced {
                FORCED_RELEASES.fetch_add(out.len() as u64, Ordering::Relaxed);
            }
        }
        out
    }

    /// Held entries on this volume.
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    /// `true` ⇔ nothing is held (the common case, and the fast path's
    /// answer).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Held device bytes on this volume.
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// The oldest held label (`None` = empty) — the deadline instrument and
    /// what a test asserts an acknowledgement against.
    pub fn oldest_label(&self) -> Option<u64> {
        if self.is_empty() {
            return None;
        }
        self.entries.lock().front().map(|e| e.label)
    }

    /// `true` ⇔ `offset` is held here. One relaxed load when the ring is
    /// empty; a bounded scan otherwise (diagnostics, the fsck/debt
    /// exemptions and the contract tests — never the hot path).
    pub fn holds(&self, offset: u64) -> bool {
        if self.is_empty() {
            return false;
        }
        self.entries.lock().iter().any(|e| e.offset == offset)
    }
}

/// **"A reader that fails to acknowledge is fenced, not waited on."**
///
/// Recompute the true minimum first (the published one is a cadence
/// snapshot, so a stall is often nothing but staleness), and only if the
/// bound is genuinely behind `oldest` name the laggards and EVICT them
/// through S6 — which mints their dead epoch, drops their grant bucket and
/// removes them from the minimum. Then republish.
///
/// Touches only the membership plane's own RAM state: no data-plane lock,
/// no metadata lock, no device I/O — a leaf in the lock order, called with
/// the ring lock released.
fn force_progress(oldest: u64, why: &str) {
    let Some(owner) = crate::membership::installed_owner() else {
        // The plane vanished (disarm / last member left): the bound is
        // already `u64::MAX` and everything releases.
        publish_bound(u64::MAX, 0);
        return;
    };
    owner.refresh_free_grace_bound();
    if bound() >= oldest {
        return;
    }
    let laggards = owner.members_behind_free_epoch(oldest);
    for id in &laggards {
        if owner
            .evict(
                id,
                &format!(
                    "did not acknowledge freed-offset label {oldest} within the grace bound \
                     ({why}) — spec §6.8 item 3: a reader that fails to acknowledge is FENCED, \
                     not waited on, because an unbounded wait converts a slow reader into the \
                     writer's ENOSPC"
                ),
            )
            .is_some()
        {
            LAGGARD_FENCES.fetch_add(1, Ordering::Relaxed);
            log::error!(
                "freed-offset grace period: member '{id}' FENCED — it never acknowledged label \
                 {oldest} ({why}). Its cached bindings are void by construction (its lease is \
                 gone, and it self-fences on its own stricter deadline); the offsets it was \
                 holding are now reallocatable (free_grace_laggard_fences)"
            );
        }
    }
    owner.refresh_free_grace_bound();
}

/// Count an allocation that refused `StorageFull` while the ring still held
/// offsets — the pressure ruling's instrument (never a silent stall).
pub fn note_alloc_stall(held: usize, held_bytes: u64) {
    ALLOC_STALLS.fetch_add(1, Ordering::Relaxed);
    log::error!(
        "allocation refused StorageFull with {held} offset(s) ({held_bytes} B) held in the \
         freed-offset grace period: the readers have not acknowledged past label {:?} and the \
         pressure bound ({} ms) has not expired. This is the RULING, not a bug — reallocating an \
         offset a reader may still resolve serves another file's bytes, silently on a \
         passthrough volume. The wait is bounded: past the pressure bound the laggard is fenced \
         (free_grace_alloc_stalls, free_grace_bound)",
        bound(),
        pressure_bound_ms(),
    );
}

// ---------------------------------------------------------------------------
// The reader's side: where the acknowledgement is emitted
// ---------------------------------------------------------------------------

/// One revalidation pass's inputs to the ladder — every term a published
/// number, so the promotion rule is auditable rather than implicit.
#[derive(Debug, Clone, Copy)]
pub struct AckInputs {
    /// The label learned from the owner (its own instant of the grant).
    pub label: u64,
    /// Member-clock instant at which that label was learned.
    pub learned_at_ms: u64,
    /// Member-clock instant at which this pass BEGAN (its ledger read).
    pub pass_start_ms: u64,
    /// Member-clock instant now.
    pub now_ms: u64,
    /// `true` ⇔ the pass advanced the epoch, i.e. it ran the R-6 purge.
    pub advanced: bool,
    /// `staleness_bound + skew_max`.
    pub qualify_lag_ms: u64,
    /// `staleness_bound + D_purge`.
    pub drain_lag_ms: u64,
}

/// The reader's acknowledgement ladder: it decides WHEN an echoed label
/// honestly means *"I have finished using anything freed at or before
/// this"*, which is a strictly stronger statement than *"I saw it"*.
///
/// Three conditions, in order:
///
/// 1. **An epoch step.** Only a pass that advanced ran the R-6 purge, and
///    only the purge drops cached BYTES keyed by a reused bare offset. An
///    inert poll proves the reader's metadata is current and nothing at all
///    about its block-key census, which is the half §6.3 calls
///    structurally undetectable.
/// 2. **Late enough.** The pass must have BEGUN at least
///    `staleness_bound + skew_max` after the label was learned. The label
///    post-dates the free; the writer's checkpoint ceiling then says the
///    dereference is durably checkpointed within the staleness bound, so
///    the record this pass adopts contains it. A pass that began earlier
///    can adopt a record that still names the freed block, and the reader
///    would re-resolve the binding immediately after purging it.
/// 3. **Drained.** `staleness_bound + D_purge` must then elapse: the daemon
///    layout/attr caches (whose reader TTL *is* the staleness bound —
///    §6.8 item 4) must expire, and serves already in flight when the purge
///    ran must finish. This is exactly §6.7's `D_purge` term ("observe,
///    then finish"), which S6's clock arithmetic already reserves.
///
/// A newer label never displaces a pending one until it has been
/// acknowledged, so the ladder cannot starve when the renewal cadence is
/// shorter than the drain window.
#[derive(Debug)]
pub struct ReaderAckLadder {
    qualified: AtomicU64,
    ready_at_ms: AtomicU64,
    acked: AtomicU64,
}

impl Default for ReaderAckLadder {
    fn default() -> Self {
        Self::new()
    }
}

impl ReaderAckLadder {
    /// An empty ladder (nothing qualified, nothing acknowledged).
    pub const fn new() -> Self {
        Self {
            qualified: AtomicU64::new(0),
            ready_at_ms: AtomicU64::new(0),
            acked: AtomicU64::new(0),
        }
    }

    /// Feed one completed revalidation pass. `Some(label)` ⇔ this pass
    /// promoted an acknowledgement, which the caller carries to the plane.
    pub fn note_pass(&self, i: AckInputs) -> Option<u64> {
        let pending = self.qualified.load(Ordering::Acquire);
        let acked = self.acked.load(Ordering::Acquire);
        // (1) + (2): qualify a NEW label only when the previous candidate
        // has been acknowledged — a pending candidate is always the oldest
        // (and therefore the soonest-ready) statement we can make.
        if i.advanced
            && i.label > pending
            && pending <= acked
            && i.pass_start_ms >= i.learned_at_ms.saturating_add(i.qualify_lag_ms)
        {
            self.qualified.store(i.label, Ordering::Release);
            self.ready_at_ms
                .store(i.now_ms.saturating_add(i.drain_lag_ms), Ordering::Release);
        }
        // (3): promote once the drain window has elapsed.
        let candidate = self.qualified.load(Ordering::Acquire);
        if candidate > self.acked.load(Ordering::Acquire)
            && i.now_ms >= self.ready_at_ms.load(Ordering::Acquire)
        {
            self.acked.store(candidate, Ordering::Release);
            READER_ACKS.fetch_add(1, Ordering::Relaxed);
            return Some(candidate);
        }
        None
    }

    /// The highest label this ladder has acknowledged.
    pub fn acked(&self) -> u64 {
        self.acked.load(Ordering::Acquire)
    }

    /// The label awaiting its drain window (`0` = none pending).
    pub fn pending(&self) -> u64 {
        let q = self.qualified.load(Ordering::Acquire);
        if q > self.acked() {
            q
        } else {
            0
        }
    }
}

/// The mount's ladder (one reader per process — the
/// `membership::INSTALLED` shape).
static LADDER: ReaderAckLadder = ReaderAckLadder::new();

/// **The reader's hook**: called by the S5 revalidation task after every
/// pass ([`crate::ro_coherence::spawn_reader_revalidation`]).
///
/// `Some(label)` ⇔ an acknowledgement was promoted and handed to the
/// session, where it rides the next lease renewal at zero extra cost (S6's
/// whole point: a reader writes nothing, anywhere). A mount that is not a
/// plane member does nothing at all.
pub fn reader_pass_completed(pass_start_ms: u64, advanced: bool) -> Option<u64> {
    let session = crate::membership::installed_member()?;
    let (label, learned_at_ms) = session.learned_label();
    if label == 0 {
        return None;
    }
    let staleness = crate::ro_coherence::reader_staleness_bound().as_millis() as u64;
    let out = LADDER.note_pass(AckInputs {
        label,
        learned_at_ms,
        pass_start_ms,
        now_ms: session.now_ms(),
        advanced,
        qualify_lag_ms: staleness + session.skew_max_ms(),
        drain_lag_ms: staleness + session.d_purge_ms(),
    });
    if let Some(label) = out {
        session.ack_free_epoch(label);
        log::debug!(
            "freed-offset acknowledgement: this reader has FINISHED with everything freed at or \
             before label {label} (an epoch-step purge ran after the label was learned, and the \
             drain window has elapsed) — it rides the next lease renewal (spec §6.8 item 3)"
        );
    }
    out
}

// ---------------------------------------------------------------------------
// Gauges + the stats block
// ---------------------------------------------------------------------------

/// Offsets deferred into the grace period since mount.
pub fn deferrals() -> u64 {
    DEFERRALS.load(Ordering::Relaxed)
}

/// Offsets released to the free list (acknowledged or forced).
pub fn releases() -> u64 {
    RELEASES.load(Ordering::Relaxed)
}

/// Offsets released WITHOUT an acknowledgement, i.e. past the bound or at
/// the ring cap — each one paired with an eviction. Healthy readers keep
/// this 0.
pub fn forced_releases() -> u64 {
    FORCED_RELEASES.load(Ordering::Relaxed)
}

/// Members evicted for not acknowledging (the "fenced, not waited on"
/// counter — must stay 0 on a healthy fleet).
pub fn laggard_fences() -> u64 {
    LAGGARD_FENCES.load(Ordering::Relaxed)
}

/// Allocations that refused `StorageFull` while offsets were held here.
pub fn alloc_stalls() -> u64 {
    ALLOC_STALLS.load(Ordering::Relaxed)
}

/// Offsets currently held across every volume.
pub fn held_offsets() -> u64 {
    HELD_OFFSETS.load(Ordering::Relaxed)
}

/// Device bytes currently held across every volume.
pub fn held_bytes() -> u64 {
    HELD_BYTES.load(Ordering::Relaxed)
}

/// Acknowledgements this mount has emitted as a READER.
pub fn reader_acks() -> u64 {
    READER_ACKS.load(Ordering::Relaxed)
}

/// The item-3 block of the stats inode (merged by `fuse_client`, the
/// `membership::stats_snapshot` precedent): an unarmed mount exports the
/// posture word alone rather than a block of zeroes that would read like a
/// broken plane.
pub fn stats_snapshot() -> serde_json::Value {
    if PLANE.load().is_none() {
        return serde_json::json!({ "free_grace_mode": "off" });
    }
    let bound = bound();
    serde_json::json!({
        "free_grace_mode": if armed() { "armed" } else { "idle" },
        "free_grace_bound": if bound == u64::MAX { 0 } else { bound },
        "free_grace_members": MEMBERS.load(Ordering::Relaxed),
        "free_grace_offsets": held_offsets(),
        "free_grace_bytes": held_bytes(),
        "free_grace_deferrals": deferrals(),
        "free_grace_releases": releases(),
        "free_grace_forced_releases": forced_releases(),
        "free_grace_laggard_fences": laggard_fences(),
        "free_grace_alloc_stalls": alloc_stalls(),
        "free_grace_reader_acks": reader_acks(),
        "free_grace_fence_bound_ms": fence_bound_ms(),
        "free_grace_pressure_bound_ms": pressure_bound_ms(),
        "free_grace_ring_cap": derived_ring_cap(),
    })
}

/// **Test seam** (the [`crate::data_custody::test_clear_poison`]
/// precedent): drop the plane and zero every gauge so one process can run
/// the contracts independently. Production has no reset path — the plane is
/// armed once per mount and disarmed at teardown.
pub fn reset_for_test() {
    disarm_owner_plane();
    for c in [
        &DEFERRALS,
        &RELEASES,
        &FORCED_RELEASES,
        &LAGGARD_FENCES,
        &ALLOC_STALLS,
        &HELD_OFFSETS,
        &HELD_BYTES,
        &READER_ACKS,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    LADDER.qualified.store(0, Ordering::Relaxed);
    LADDER.ready_at_ms.store(0, Ordering::Relaxed);
    LADDER.acked.store(0, Ordering::Relaxed);
}
