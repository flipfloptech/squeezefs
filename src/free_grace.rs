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
//! | "fenced, not waited on" | [`MembershipOwner::evict`](crate::membership::MembershipOwner::evict) via `force_progress` |
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
//! # The pressure-coupled release valve (rung-20 residual 6)
//!
//! The ruling above says what happens when the supply runs out; it does
//! not stop the supply running out. The field showed it does, on the
//! cadence alone: a rewrite storm's deferrals outrun the readers'
//! releases on their natural beat, `free_grace_offsets` climbs
//! monotonically and the lane's share follows it down
//! (`.benchmarks/2026-08-19-blob-aware-merge-and-fabric-venue.md` §3 —
//! 0 → 825 across one 8-rank row, never draining;
//! `.benchmarks/2026-08-18-mw-field-mpiio-prep.md` §Residuals — 21,001
//! deferrals against 17,751 releases with the machinery otherwise
//! healthy). Ring capacity was never the binding constraint, so a bigger
//! ring answers nothing: the writer must make the readers ANSWER SOONER.
//!
//! The valve is one graded signal feeding a three-rung ladder, cheapest
//! coherence cost first. **The signal** ([`runway_ms`]) is arithmetic the
//! ring can do on itself: its two end labels give the storm's own
//! deferral rate, and the smaller of its headroom and the volume's free
//! supply gives how many more offsets that rate may consume — so the
//! reading is a TIME, comparable against the very bounds the plane
//! publishes, and no threshold is a free-floating constant.
//!
//! | Rung | Act | Counted | Derivation |
//! |---|---|---|---|
//! | **(a) prod** | members the writer is waiting on are granted a SHORTER renewal cadence, and their answers already in hand are read at once instead of at the owner's sweep | `free_grace_prods` | [`ProdParams::cadence_for`] — the plane's own [`ack_cycle`] inverted against the runway, clamped into `[`[`ack_refresh_floor`]`, the routine cadence]` |
//! | **(b) tighten** | the fence deadline slides from the routine bound toward the pressure bound as the runway shortens | `free_grace_bound_tightenings` | [`effective_bound_ms_from`] — linear between the two numbers the plane already derives; the FLOOR is one honest ack cycle, so a healthy reader is never fenced by the tightening |
//! | **(c) force** | the existing arm: a forced release, always WITH the responsible member's eviction | `free_grace_forced_releases` / `free_grace_laggard_fences` | unchanged |
//!
//! Rung (a) needs no new wire field and no push channel: `Grant::renew_ms`
//! IS what [`crate::membership::MemberSession::renew_at_ms`] — and hence
//! the renewal loop's sleep — is computed from, so handing a laggard a
//! shorter cadence on the beat it is already making is the ask. It rides
//! the dedicated `sqz-lease` lane (finding 2's isolation), which is why a
//! prod is deliverable under exactly the storm that provokes it.
//!
//! **What rung (a) may NOT do** is beat faster than the reader's ladder
//! can produce a new answer, and the sharp edge there is not economy but
//! correctness: every renewal re-learns a fresher label, so a cadence
//! shorter than the ladder's qualification lag would refresh the target
//! out from under every pass and the reader would acknowledge *nothing*
//! (see [`ReaderAckLadder`], which snapshots a CANDIDATE precisely so that
//! cannot happen, and [`ack_refresh_floor`], which is the cadence floor).
//!
//! `SQUEEZEFS_FREE_GRACE_VALVE=0` disarms rungs (a) and (b) — the A/B
//! control, which restores the pre-campaign shape verbatim: the routine
//! bound, the pressure bound at the allocation cliff, and nothing else.
//!
//! # The hold-time campaign (2026-09-06, finding 15's remaining half)
//!
//! With the co-writer free path's supply leak closed, the s11 fleet row
//! HELD the supply instead: `free_grace_bound_age_ms` 8,994 with every
//! cadence at its 1 Hz floor and nothing starving
//! (`.benchmarks/2026-09-06-free-grace-hold-time.md`). The hold is now
//! DECOMPOSED per released offset (`free_grace_hold_phase_ns`:
//! `defer→checkpointed` off the KV checkpoint mark, `checkpointed→min_acked`
//! off [`publish_bound`]'s own advance, `min_acked→released`; exact-sum),
//! with the live hold (`free_grace_hold_ms`) and the per-member ack lag
//! (`free_grace_member_ack_lag_ms`) beside it. Measured at the fleet
//! cadences, 6.0 s of the 8.6 s are the ladder's two DERIVED windows
//! (`staleness + skew`, `staleness + D_purge` — each term a poll interval
//! or the checkpoint ceiling), and the cadence terms around them are what
//! two levers cut: **(b)** a promotion wakes the member's renewal loop
//! and renews as CARRIAGE ([`crate::membership::request_renewal_now`],
//! `SQUEEZEFS_FREE_GRACE_ACK_RENEWAL`), and **(d)** a binding member's
//! advancing ack marks the bound dirty for the next harvest's recompute
//! (`note_member_ack_advanced`, `SQUEEZEFS_FREE_GRACE_REFRESH_ON_ACK`)
//! — together −922 ms in-process, closure exact, zero fences. A faster
//! writer checkpoint cadence alone was measured INERT: the reader
//! qualifies on the time bound, never on observing the checkpoint. It
//! pays as the **writer→member checkpoint composite** (adjudication
//! item 4, user decision 2026-09-06): while the valve is ASKING (a prod
//! in force) the writer's checkpoint ceiling is `P/2` against the
//! reader's routine poll ([`elastic_checkpoint_ceiling_ms`],
//! [`checkpoint_ceiling_in_force_ms`] — the KV checkpoint task reads it
//! per tick), every grant CARRIES the live ceiling
//! ([`crate::membership::Grant::checkpoint_ceiling_ms`] via
//! [`advertise_checkpoint_ceiling`], a promise the writer honours for one
//! routine ceiling past the grant), and on the member it is the prod
//! floor ([`ProdParams::floor_for`]) and L2b's pass floor
//! ([`reader_pass_interval`]) — so passes and beats run at `P/2` too, at
//! the accepted cost of 2× lease-lane beats and 2× checkpoint cycles
//! while an ask is in force. `SQUEEZEFS_FREE_GRACE_CHECKPOINT_COMPOSITE=0`
//! is the shipped shape exactly; the published `reader_staleness_bound_ms`
//! never moves (the elastic cadence sits inside it).
//!
//! # The ladder re-derivation (user decision 2026-09-06)
//!
//! The two windows were then RE-DERIVED
//! (`.benchmarks/2026-09-06-free-grace-ladder-rederivation.md`; the
//! design's KD-FG-11 as amended) — still pinned against demand, but each
//! term traced to the writer's machinery or to an observed event:
//! **qualify** = the writer's checkpoint LANDING ceiling + skew
//! ([`qualify_lag_ms`]; the staleness bound's poll interval bounded
//! nothing — the pass IS the poll), **drain** = an epoch-step invalidation
//! of the reader's layout cache
//! ([`crate::ro_coherence::layout_entry_pre_step`] — no TTL to wait out)
//! plus an OBSERVED in-flight drain ([`crate::ro_coherence::ServeStamp`]
//! / [`crate::ro_coherence::serve_drained_below`] — every read serve
//! counted in the purge generation it started under; `D_purge` kept only
//! as the `free_grace_drain_overdue` tripwire). Fleet-cadence model:
//! `bound_age` 7,724 → 2,724 ms, closure exact, zero fences. Each of the
//! three levers (`SQUEEZEFS_FREE_GRACE_{QUALIFY_CEILING,DRAIN_EPOCH_STAMP,
//! DRAIN_OBSERVED}`) restores its retired term verbatim.
//!
//! # Term 2 — the fleet-only `min_acked→released` hop, and the lane behind it
//!
//! The fleet row (`.benchmarks/2026-09-06-free-grace-hold-time.md` §6) read
//! a 2,500 ms MEAN on `min_acked→released` where the model read 9 ms.
//! `released` IS the harvest's pop, and the harvest is DEMAND-driven — a
//! terminal free landing, an allocation, a co-writer's harvest RPC — so the
//! stage is the wait for the next demand event after the cover; in the
//! fleet's quiet phases (a close, a fsync wedge) the cover comes from the
//! owner's 10 s sweep and the release from the next iteration's first
//! demand. Behind it sits a hop no ledger saw: a released CO-WRITER-lane
//! block is on the AUTHORITY's list, reachable only by that co-writer's
//! next harvest RPC — its ENOSPC park slices, or the watermark tick, which
//! never fires while the co-writer holds supply above its watermark.
//! `alloc_lane_visible_phase_ns` (`released_served` on the authority's
//! clock, `served_visible` = the co-writer's round trip, exact-sum) is the
//! instrument ([`mark_lane_release`] / [`take_lane_release`] /
//! [`note_lane_visible`]); the reply carries each block's age (publish
//! schema 14). The lever `SQUEEZEFS_FREE_GRACE_LANE_PUSH` (default on):
//! **release on ack** — a binding member's advancing acknowledgement
//! harvests every ring to its uncovered front through the installed
//! [`ReleaseHook`], rate-limited by lever (d)'s law — and **the lane-supply
//! hint** — the renewal grant carries the member's lane population on the
//! authority's lists (per-lane counters, never a scan), and a co-writer
//! learning a nonzero hint while owed wakes its refill at once
//! ([`lane_supply_wake`]). Model at the fleet cadences (3 s write / 21 s
//! close): `min_acked→released` 3,408 → 0 ms, the lane hop 30.7 → 1.5 s,
//! the hold 22.6 → 14.7 s; `.benchmarks/2026-09-06-free-grace-lane-visible.md`.
//!
//! # Cost when unarmed (the shipped default)
//!
//! `SQUEEZEFS_MEMBERSHIP_BIND=off` is the default, so the common mount must
//! pay nothing: every entry point is one relaxed load of the armed word feeding
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
/// Finding 29: bounded-allocation park slices taken (see
/// [`pressure_parks`]).
static PRESSURE_PARKS: AtomicU64 = AtomicU64::new(0);
static LAGGARD_FENCES: AtomicU64 = AtomicU64::new(0);
static ALLOC_STALLS: AtomicU64 = AtomicU64::new(0);
static HELD_OFFSETS: AtomicU64 = AtomicU64::new(0);
static HELD_BYTES: AtomicU64 = AtomicU64::new(0);
static READER_ACKS: AtomicU64 = AtomicU64::new(0);
/// Sustain campaign site 0 (KD-FG-9): harvest passes that OBSERVED recycle
/// coupling — ring aging past the physics floor while the lane-reachable
/// supply sat at the trough. The counter is the PR 1 observation (always
/// live); the MARK below is what PR 3's consumers arm off.
static DEMAND_WAITS: AtomicU64 = AtomicU64::new(0);
/// The demand MARK's expiry (owner clock; TTL'd exactly like the runway
/// reading). A live mark arms rung a′ (§5.2) and the L3 widened refresh
/// gate; it NEVER feeds rung (b)'s deadline.
static DEMAND_UNTIL_MS: AtomicU64 = AtomicU64::new(0);
/// The graded coupling face (`free_grace_demand_pct`): how far past the
/// physics floor the ring's front had aged at the last site-0 observation,
/// capped at 100. Read beside the scarcity face `free_grace_pressure_pct`.
static DEMAND_AGE_PCT: AtomicU64 = AtomicU64::new(0);
/// Rung a′'s engagement: prods issued BECAUSE the demand mark was live
/// (⊆ `free_grace_prods`).
static DEMAND_PRODS: AtomicU64 = AtomicU64::new(0);
/// 1 ⇔ the cadence in `PROD_RENEW_MS` came from the demand arm (rung a′)
/// rather than the space runway — what splits `DEMAND_PRODS` from `PRODS`.
static PROD_FROM_DEMAND: AtomicU64 = AtomicU64::new(0);
/// L3's engagement: bound recomputes run on the demand/prod harvest path
/// (the valve's existing rate-limited refresh; law ≤ elapsed ÷ the floor).
static BOUND_REFRESHES: AtomicU64 = AtomicU64::new(0);
/// L2b (§5.2b): the prodded RENEWAL cadence last adopted by this member
/// (ms), and its expiry on the member clock — the revalidation loop's
/// pass-cadence ask (no wire field: the grant's `renew_ms` IS the ask).
static PASS_PROD_MS: AtomicU64 = AtomicU64::new(0);
static PASS_PROD_UNTIL_MS: AtomicU64 = AtomicU64::new(0);
/// L2b's engagement: revalidation passes run on a tightened cadence.
static PASS_PRODS: AtomicU64 = AtomicU64::new(0);
/// The pass cadence in force, ms (routine when no prod is live — the
/// `prod_renew_ms` precedent).
static PASS_INTERVAL_MS: AtomicU64 = AtomicU64::new(0);
/// The per-offset residence histogram (`free_grace_residence_ms`, §8):
/// stamped at each release with `now − (label − 1)` — the measured loop
/// latency whose p50 must agree with `bound_age` under storm.
static RESIDENCE_MS: once_cell::sync::Lazy<crate::fuse_client::LatencyHistogram> =
    once_cell::sync::Lazy::new(crate::fuse_client::LatencyHistogram::default);
/// Residence samples recorded (the histogram's own total — the contracts'
/// cheap accessor).
static RESIDENCE_SAMPLES: AtomicU64 = AtomicU64::new(0);

// -- the hold-time decomposition (2026-09-06 campaign) ----------------------
//
// Where a held offset's residence goes, per stage, READ off the machinery
// rather than inferred from the cadences: `defer→checkpointed` (the first
// KV checkpoint the authority completed after the defer — the instant the
// dereference is durably in a root a reader can adopt), `checkpointed→
// min_acked` (the first bound publish that covered the label — every
// member acknowledged past it) and `min_acked→released` (the harvest's
// visit); `total` is the residence. Exact-sum per sample when the
// checkpoint stage is placeable.

/// Owner-clock instants of completed checkpoints (any meta volume),
/// oldest first. Pruned at each push to the routine fence bound — the
/// longest an offset can be held — so the deque's population derives
/// from `fence_ms ÷ the checkpoint period` (≈ 80–160 on the shipped
/// clocks), never a constant. A leaf lock: taken for a push (≤ a few/s)
/// or a bounded lookup pass per harvest, never across anything else.
static CHECKPOINT_MARKS: parking_lot::Mutex<VecDeque<u64>> =
    parking_lot::Mutex::new(VecDeque::new());
/// Marks recorded since arm (`free_grace_checkpoint_marks`).
static CHECKPOINT_MARK_COUNT: AtomicU64 = AtomicU64::new(0);
/// `(bound, owner instant)` of every bound ADVANCE (`publish_bound` with a
/// higher minimum than the published one), oldest first — both columns
/// monotone by construction, so the covering publish of a label is one
/// partition point. Pruned like the marks.
static BOUND_ADVANCES: parking_lot::Mutex<VecDeque<(u64, u64)>> =
    parking_lot::Mutex::new(VecDeque::new());
/// The per-stage histograms (`free_grace_hold_phase_ns`).
static HOLD_PHASES: once_cell::sync::Lazy<[crate::fuse_client::LatencyHistogram; HOLD_PHASES_N]> =
    once_cell::sync::Lazy::new(|| {
        std::array::from_fn(|_| crate::fuse_client::LatencyHistogram::default())
    });
/// Releases one or more of whose stages could not be placed: no recorded
/// bound advance covers the label (`total` alone stamps), or no checkpoint
/// mark sits between the defer and the cover — a plane armed without the
/// KV hook, or a cover that preceded any recorded checkpoint (`total` and
/// `min_acked_released` stamp). The exact-sum law reads over the placed
/// population.
static HOLD_UNPLACED: AtomicU64 = AtomicU64::new(0);
/// The measured checkpoint cycle duration, EWMA in ns (fed by the hook).
static CHECKPOINT_CYCLE_EWMA_NS: AtomicU64 = AtomicU64::new(0);
/// The measured hold (`free_grace_hold_ms`): EWMA of the per-offset
/// residence at release, folded per harvest batch.
static HOLD_EWMA_MS: AtomicU64 = AtomicU64::new(0);

// -- the hold-time levers -----------------------------------------------------

/// Lever (b): renewals a promotion triggered at once instead of waiting
/// for the member's next beat (`free_grace_ack_renewals`, member-side).
static ACK_RENEWALS: AtomicU64 = AtomicU64::new(0);
/// Lever (d): `true` ⇔ a member whose recorded acknowledgement sat at or
/// below the published bound has advanced it since the last recompute —
/// the min MAY have moved. Set by the owner's renewal (one compare),
/// consumed by the next harvest.
static BOUND_DIRTY: AtomicBool = AtomicBool::new(false);
/// Lever (d): recomputes the dirty mark triggered
/// (`free_grace_bound_refreshes_on_ack`, ⊆ `free_grace_bound_refreshes`).
static BOUND_REFRESHES_ON_ACK: AtomicU64 = AtomicU64::new(0);
/// The measured cost of one O(members) minimum, EWMA in ns — the physical
/// floor of the on-ack recompute's rate limit (a recompute may not run
/// more than half the time).
static BOUND_SCAN_EWMA_NS: AtomicU64 = AtomicU64::new(0);

// -- the writer→member checkpoint composite (adjudication item 4, 2026-09-06) --
//
// Contract 31 measured a faster writer checkpoint INERT alone: the reader
// qualifies a label on a TIME bound, never on observing the checkpoint,
// and at the shipped cadences every pass already advances. It pays only
// as the COMPOSITE the user approved on 2026-09-06: while the valve is
// ASKING members to answer sooner (a prod in force — rung (a) off the
// space runway or rung a′ off the demand mark), the writer does its half
// of the ask — its checkpoint ceiling becomes P/2 against the reader's
// routine poll P (the Nyquist bound: a new root in every pass window) —
// and the live ceiling travels to every member on the grant
// (`Grant::checkpoint_ceiling_ms`), where it is the prod floor AND L2b's
// pass floor, so passes and beats run at P/2 too: the learn, the
// qualify-rounding and the carry terms of the hold halve. The accepted
// cost: 2× lease-lane beats and 2× checkpoint cycles while an ask is in
// force. `SQUEEZEFS_FREE_GRACE_CHECKPOINT_COMPOSITE=0` restores the
// shipped shape exactly (KD-FG-7).

/// Checkpoint cycles the task ran with the elastic ceiling in force
/// (`free_grace_checkpoint_elastic_cycles` — the writer-side engagement).
static CHECKPOINT_ELASTIC_CYCLES: AtomicU64 = AtomicU64::new(0);
/// **The promise pair**: the smallest elastic ceiling advertised on a
/// grant whose window is still open, and the owner instant that window
/// closes — one routine ceiling past the LAST elastic advertisement. A
/// grant advertising `c` at `t` promises "every commit before this grant
/// is checkpointed within `c` of it"; the writer therefore enforces
/// `min(the live derivation, this pair)` and relaxes no sooner than every
/// advertised window has closed, whatever the ask or the lever did since.
/// (Two words, no lock: a first-after-lapse race between two concurrent
/// grants can keep the LARGER of two derivations taken µs apart — the
/// same P/2 with at most the EWMA cycle floor's drift between them.)
static PROMISED_CEILING_MS: AtomicU64 = AtomicU64::new(0);
static PROMISED_UNTIL_MS: AtomicU64 = AtomicU64::new(0);
/// L2b's pass FLOOR deposit (the composite's member side): the writer's
/// checkpoint ceiling advertised on the grant that carried the ask in
/// `PASS_PROD_MS` (`0` = none advertised — the routine constant applies).
static PASS_FLOOR_MS: AtomicU64 = AtomicU64::new(0);

// -- the lane-visible decomposition (finding 15 term 2, 2026-09-06) ----------
//
// Where a RELEASED offset goes before a co-writer can mint it: a released
// co-writer-lane block lands on the AUTHORITY's free list (`released` — the
// harvest's pop and the free-list publish are one synchronous act, so the
// hold ledger's `min_acked→released` already ends at the publish), waits
// there until that lane's next harvest RPC takes it (`released→served`,
// authority clock), and becomes visible to the co-writer's allocator when
// the reply is adopted (`served→visible`, the round trip — the co-writer's
// clock, the serve having happened inside it). The two clocks never mix:
// the authority stamps the age it measured into the reply, the co-writer
// stamps that age beside its own RTT, and `total ≡ released_served +
// served_visible` per sample by construction.

/// `(vol_tag, block_idx)` → owner-clock ms of the free-list publish, for
/// every grace-released block of a lane THIS mount does not own (the
/// co-writers' supply on the authority's list). Inserted at the publish,
/// removed at the lane harvest that takes the block — a subset of the
/// free list's foreign-lane population, so it is bounded by it. Only ever
/// touched on an armed plane with a partition engaged.
static LANE_RELEASE_MARKS: once_cell::sync::Lazy<scc::HashMap<(u64, u64), u64>> =
    once_cell::sync::Lazy::new(scc::HashMap::new);
/// The per-stage histograms (`alloc_lane_visible_phase_ns`).
static LANE_VISIBLE_PHASES: once_cell::sync::Lazy<
    [crate::fuse_client::LatencyHistogram; LANE_VISIBLE_PHASES_N],
> = once_cell::sync::Lazy::new(|| {
    std::array::from_fn(|_| crate::fuse_client::LatencyHistogram::default())
});
/// Served lane blocks with no release mark (`alloc_lane_visible_unplaced`):
/// a block that reached the free list other than through a grace release
/// on this plane (the mount-time derivation, a release before the plane
/// armed). The exact-sum law reads over the placed population.
static LANE_VISIBLE_UNPLACED: AtomicU64 = AtomicU64::new(0);

const LANE_VISIBLE_PHASES_N: usize = 3;
/// The stage names, in loop order; the JSON keys of
/// `alloc_lane_visible_phase_ns`.
pub const LANE_VISIBLE_PHASE_NAMES: [&str; LANE_VISIBLE_PHASES_N] =
    ["released_served", "served_visible", "total"];
const LANE_VISIBLE_RELEASED_SERVED: usize = 0;
const LANE_VISIBLE_SERVED_VISIBLE: usize = 1;
const LANE_VISIBLE_TOTAL: usize = 2;

// -- the lane-push lever (`SQUEEZEFS_FREE_GRACE_LANE_PUSH`) --------------------

/// Cached knob: 0 = unread, 1 = on, 2 = off (the `ACK_RENEWAL` shape).
static LANE_PUSH: AtomicU64 = AtomicU64::new(0);
/// The authority's release hook — every allocator's grace ring harvested
/// to its uncovered front — installed by the multi-writer arm (`None` on
/// every other mount, which makes the release-on-ack arm structurally
/// inert there).
static RELEASE_HOOK: once_cell::sync::Lazy<ArcSwapOption<ReleaseHook>> =
    once_cell::sync::Lazy::new(ArcSwapOption::empty);
/// Release-on-ack runs (`free_grace_lane_push_releases`): binding acks
/// whose arrival harvested the rings instead of leaving the covered
/// offsets to the next demand event.
static LANE_PUSH_RELEASES: AtomicU64 = AtomicU64::new(0);
/// The authority's per-member lane-supply source (member id → blocks of
/// that member's lane on this mount's free lists), installed by the
/// multi-writer arm beside the hook.
static LANE_SUPPLY_SOURCE: once_cell::sync::Lazy<ArcSwapOption<LaneSupplySource>> =
    once_cell::sync::Lazy::new(ArcSwapOption::empty);
/// Renewal grants that carried a nonzero lane-supply hint
/// (`free_grace_lane_push_hints`, authority side).
static LANE_PUSH_HINTS: AtomicU64 = AtomicU64::new(0);
/// The last hint this member's renewal learned (co-writer side): blocks of
/// its lane on the authority's free list.
static LANE_SUPPLY_HINT: AtomicU64 = AtomicU64::new(0);
/// Hints that woke a parked refill (`free_grace_lane_push_wakes`,
/// co-writer side).
static LANE_PUSH_WAKES: AtomicU64 = AtomicU64::new(0);
/// The co-writer's refill wake: the ahead-refill task and the bounded
/// allocation park wait on it beside their own cadence, so a hint ends
/// the wait at once.
static LANE_SUPPLY_WAKE: squeezefs_ipc::sqz_notify::Notify =
    squeezefs_ipc::sqz_notify::Notify::new();

/// The authority-side release hook: harvest every ring to its uncovered
/// front (RAM only — ring locks, free-list inserts, the mark ledgers).
pub type ReleaseHook = Arc<dyn Fn() + Send + Sync>;
/// The authority-side lane-supply source: a member id → the blocks of that
/// member's lane on this mount's free lists (0 for a member with no lane).
pub type LaneSupplySource = Arc<dyn Fn(&str) -> u64 + Send + Sync>;

const HOLD_PHASES_N: usize = 4;
/// The stage names, in loop order; the JSON keys of
/// `free_grace_hold_phase_ns`.
pub const HOLD_PHASE_NAMES: [&str; HOLD_PHASES_N] = [
    "defer_checkpointed",
    "checkpointed_min_acked",
    "min_acked_released",
    "total",
];
const HOLD_DEFER_CHECKPOINTED: usize = 0;
const HOLD_CHECKPOINTED_MIN_ACKED: usize = 1;
const HOLD_MIN_ACKED_RELEASED: usize = 2;
const HOLD_TOTAL: usize = 3;

// -- the pressure valve's words (rung-20 residual 6) ------------------------

/// Rung (a): grants handed a tightened renewal cadence.
static PRODS: AtomicU64 = AtomicU64::new(0);
/// Rung (b): harvests that evaluated a deadline below the routine bound.
static BOUND_TIGHTENINGS: AtomicU64 = AtomicU64::new(0);
/// The live pressure reading — how long the writer's supply lasts at the
/// storm's measured deferral rate. `u64::MAX` = no reading.
static RUNWAY_MS: AtomicU64 = AtomicU64::new(u64::MAX);
/// Owner-clock instant past which [`RUNWAY_MS`] is stale (a pressured
/// writer refreshes it at every free; a quiet one lets it expire).
static RUNWAY_UNTIL_MS: AtomicU64 = AtomicU64::new(0);
/// The tightened renewal cadence in force, ms (`0` = no prod).
static PROD_RENEW_MS: AtomicU64 = AtomicU64::new(0);
/// Finding 18's engagement gauge: expiry windows in which a live ask
/// RELAXED one doubling step toward routine (while the plane still held
/// offsets) instead of lapsing to the routine cadence — each pre-fix
/// lapse was a routine-beat acknowledgement hole the min-composition
/// inherited (the f16a row's 15.5–22.6 s `bound_age` against PR 5's
/// ≤ 12 s gate, with the member ladder itself on budget).
static PROD_DECAYS: AtomicU64 = AtomicU64::new(0);
/// Owner-clock expiry of the prod (same lifetime rule as the reading).
static PROD_UNTIL_MS: AtomicU64 = AtomicU64::new(0);
/// The NEWEST label the plane is holding: a member that has acknowledged
/// past it is holding nothing and is not prodded.
///
/// Deliberately the newest and not the oldest — the label at the front is
/// the one this very harvest is about to release, so a member that just
/// answered it reads as "caught up" for the instant between its beat and
/// the writer's next free, and under a storm that instant is every beat.
/// "Is there anything held that this member has not acknowledged" is the
/// question that survives the storm.
static PROD_LABEL: AtomicU64 = AtomicU64::new(0);
/// Owner-clock instant of the last pressure-driven bound recomputation —
/// the rate limiter for rung (a)'s second half (the minimum is O(members)).
static LAST_BOUND_REFRESH_MS: AtomicU64 = AtomicU64::new(0);

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
    /// what allocation evaluates when it is about to refuse `StorageFull`,
    /// and the FLOOR rung (b) tightens toward.
    pressure_fence_ms: u64,
    /// Rung (a)'s cadence law — `None` when the plane was armed without
    /// its clocks (the deterministic test seam) or when the operator
    /// disarmed the valve.
    prod: Option<ProdParams>,
    /// `false` ⇒ rungs (a) and (b) stand down (`SQUEEZEFS_FREE_GRACE_VALVE=0`,
    /// the A/B control): the routine and pressure bounds behave exactly as
    /// they did before the valve landed.
    valve: bool,
    /// How long one pressure reading stays live without a refresh. The
    /// routine renewal cadence where it is known: past one beat with no
    /// harvest, whatever the storm was doing is over.
    reading_ttl_ms: u64,
    /// **The physics floor** (the sustain campaign's site-0 input,
    /// design-free-grace-sustain §5.4/KD-FG-9): the age past which a held
    /// offset SHOULD have released on a healthy loop — the reader's
    /// qualify (`staleness + skew`) + drain (`staleness + D_purge`)
    /// windows plus two acknowledgement-refresh beats. Resolved once at
    /// arm (production derives it from the same published numbers the
    /// ack ladder runs on; the explicit-bounds test seam approximates it
    /// with the pressure bound).
    demand_floor_ms: u64,
    /// The reader's ROUTINE poll interval `P`
    /// ([`crate::ro_coherence::reader_revalidate_interval`]), ms — the
    /// Nyquist input of the elastic checkpoint ceiling. Resolved once at
    /// arm: the derivation reads the environment.
    reader_poll_ms: u64,
    /// The writer's ROUTINE effective checkpoint ceiling
    /// ([`writer_routine_checkpoint_ceiling_ms`]), ms — what a grant
    /// advertises with no ask in force and the cap the elastic ceiling can
    /// never exceed.
    writer_routine_ceiling_ms: u64,
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
pub fn fence_bound_base_ms() -> u64 {
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
    // sweep, but never sooner than a healthy reader can answer. It is also
    // rung (b)'s floor.
    let cycle = ack_cycle(clocks);
    let pressure = (fence / 2).max(cycle).min(fence);
    // The site-0 physics floor (sustain campaign, KD-FG-9): the reader's
    // own qualify + drain windows plus two refresh beats — the age past
    // which a held offset SHOULD have released on a healthy loop. Derived
    // from the same published numbers the ack ladder runs on (≈ 8.02 s on
    // the shipped s11-venue derivation, §3.2).
    let staleness = crate::ro_coherence::reader_staleness_bound();
    let floor = (staleness + clocks.skew_max)
        + (staleness + clocks.d_purge)
        + ack_refresh_floor(clocks) * 2;
    arm_plane(
        clock,
        fence,
        pressure,
        Some(ProdParams::derive(clocks)),
        floor,
    );
    Ok(())
}

/// Arm with explicit bounds — the deterministic test seam (and the form
/// [`arm_owner_plane`] resolves into).
///
/// Rung (a) stands down here: the prodded cadence is a statement about the
/// READER's ladder, and a plane armed without the clocks that ladder runs
/// on has nothing honest to say about it. Rung (b) and the fence arm are
/// fully live.
pub fn arm_owner_plane_with(clock: LeaseClock, fence: Duration, pressure: Duration) {
    // The seam approximates the site-0 physics floor with the pressure
    // bound (one honest cycle-class number); production derives the real
    // one in `arm_owner_plane`.
    arm_plane(clock, fence, pressure, None, pressure);
}

fn arm_plane(
    clock: LeaseClock,
    fence: Duration,
    pressure: Duration,
    prod: Option<ProdParams>,
    demand_floor: Duration,
) {
    // The A/B control is read ONCE, at arm: the valve's hot path is the
    // free path, and a knob read there would be a getenv per free.
    let valve = crate::env_knobs::bool_knob("SQUEEZEFS_FREE_GRACE_VALVE", true);
    let prod = if valve { prod } else { None };
    let reading_ttl_ms = prod
        .map(|p| p.renew_ms)
        .unwrap_or_else(|| pressure.as_millis() as u64);
    PLANE.store(Some(Arc::new(Plane {
        clock,
        fence_ms: fence.as_millis() as u64,
        pressure_fence_ms: pressure.as_millis() as u64,
        prod,
        valve,
        reading_ttl_ms,
        demand_floor_ms: demand_floor.as_millis() as u64,
        reader_poll_ms: (crate::ro_coherence::reader_revalidate_interval().as_millis() as u64)
            .max(1),
        writer_routine_ceiling_ms: writer_routine_checkpoint_ceiling_ms(),
    })));
    if !valve {
        log::warn!(
            "freed-offset grace period: the pressure-coupled release valve is DISARMED \
             (SQUEEZEFS_FREE_GRACE_VALVE=0, the A/B control) — readers keep their routine \
             renewal cadence under write pressure and the fence deadline never tightens, so a \
             storm whose deferrals outrun the releases reaches ENOSPC (free_grace_alloc_stalls) \
             and then the fence, exactly as it did before rung-20 residual 6"
        );
    }
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
    let plane = PLANE.load();
    let published = if members == 0 || plane.is_none() {
        u64::MAX
    } else {
        min_acked
    };
    let previous = BOUND.swap(published, Ordering::Relaxed);
    ARMED.store(published != u64::MAX, Ordering::Release);
    // The hold ledger's covering instants: every ADVANCE, including the
    // one to `u64::MAX` (a disarm releases everything held, and those
    // releases attribute to it).
    if published > previous {
        if let Some(plane) = plane.as_ref() {
            let now = plane.clock.now_ms();
            let mut advances = BOUND_ADVANCES.lock();
            prune_marks_front(&mut advances, |&(_, at)| at, now, plane.fence_ms);
            advances.push_back((published, now));
        }
    }
}

/// Drop the entries of an owner-instant-ordered deque older than the
/// routine fence bound — the longest an offset can be held, so nothing a
/// future release could attribute to is ever dropped.
fn prune_marks_front<T>(deque: &mut VecDeque<T>, at: impl Fn(&T) -> u64, now: u64, fence_ms: u64) {
    let horizon = now.saturating_sub(fence_ms);
    while deque.front().is_some_and(|e| at(e) < horizon) {
        deque.pop_front();
    }
}

/// **The KV checkpoint hook** (hold-time campaign): called by the
/// checkpoint task once the ledger record naming the new roots has been
/// written — the instant a reader's poll can adopt a root carrying every
/// dereference committed before the cycle began. Records the owner-clock
/// mark the `defer→checkpointed` stage is read against. No plane ⇒ one
/// `ArcSwap` load and nothing else (a solo mount's checkpoint pays no
/// lock); `cycle` is the cycle's own wall duration, the measured cost the
/// coupled cadence is floored on.
pub fn note_checkpoint_completed(cycle: Duration) {
    let Some(plane) = PLANE.load_full() else {
        return;
    };
    let now = plane.clock.now_ms();
    {
        let mut marks = CHECKPOINT_MARKS.lock();
        prune_marks_front(&mut marks, |&at| at, now, plane.fence_ms);
        marks.push_back(now);
    }
    CHECKPOINT_MARK_COUNT.fetch_add(1, Ordering::Relaxed);
    // The measured cycle cost (EWMA α = 1/4, the `sample_alloc_rate`
    // shape): the physical floor of the coupled checkpoint cadence — a
    // cadence shorter than the cycle it schedules is not a cadence.
    let inst = cycle.as_nanos().min(u64::MAX as u128) as u64;
    let old = CHECKPOINT_CYCLE_EWMA_NS.load(Ordering::Relaxed);
    let ewma = if old == 0 {
        inst
    } else {
        old.saturating_sub(old.div_ceil(4)) + inst / 4
    };
    CHECKPOINT_CYCLE_EWMA_NS.store(ewma, Ordering::Relaxed);
}

/// The measured checkpoint cycle cost, ms (`free_grace_checkpoint_cycle_ms`;
/// 0 until the first mark).
pub fn checkpoint_cycle_ms() -> u64 {
    CHECKPOINT_CYCLE_EWMA_NS.load(Ordering::Relaxed) / 1_000_000
}

/// Checkpoint marks recorded on the armed plane (`free_grace_checkpoint_marks`).
pub fn checkpoint_marks() -> u64 {
    CHECKPOINT_MARK_COUNT.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// The writer→member checkpoint composite (adjudication item 4)
// ---------------------------------------------------------------------------

/// **The writer's ROUTINE effective checkpoint ceiling**, ms: the KV
/// checkpoint task decides `elapsed ≥ CHECKPOINT_MAX_AGE_MS` once per
/// flush-cadence tick, so the cadence it actually keeps is
/// `max(tick, CHECKPOINT_MAX_AGE_MS)` — the constant on every venue whose
/// flush interval is below one second (the shipped 50 ms default), the
/// tick itself on a slow-flush venue. The SAME derivation
/// `resolve_revalidate_interval_ms` performs for the reader's poll (with
/// no override), so the two cannot drift. What a grant advertises when no
/// ask is in force — an honest number on every venue, where the constant
/// alone is not.
pub fn writer_routine_checkpoint_ceiling_ms() -> u64 {
    crate::meta_backend::kv::revalidate::resolve_revalidate_interval_ms(
        crate::meta_backend::resolve_flush_interval_ms(),
        None,
    )
    .max(1)
}

/// **The elastic checkpoint ceiling — the pure form** (tie-tested in
/// `tests/derivation_sweep_tests.rs`): half the reader's routine poll
/// `P` — the Nyquist bound that puts a new root in every pass window
/// however the two cadences phase — floored at twice the checkpoint
/// cycle's own measured cost (a cycle may not run more than half the
/// time: lever (d)'s law for the bound scan, applied to the checkpoint)
/// and never slower than the writer's routine ceiling. One millisecond is
/// the physical minimum (the clocks' grain).
pub fn elastic_checkpoint_ceiling_ms(reader_poll_ms: u64, cycle_ms: u64, routine_ms: u64) -> u64 {
    (reader_poll_ms / 2)
        .max(cycle_ms.saturating_mul(2))
        .max(1)
        .min(routine_ms.max(1))
}

/// `true` ⇔ the valve is ASKING: a prod cadence is in force (rung (a) off
/// the space runway or rung a′ off the demand mark — both deposit into the
/// same word). The composite's engagement predicate: the writer does its
/// half of the ask exactly while the members are asked for theirs. The
/// L4 demand mark alone would not do — site 0's age arm needs the ring's
/// front past the physics floor, which a healthy coupled loop (the fleet-
/// cadence shape, hold ≈ 7.7 s against an 8.02 s floor) never reaches, and
/// the fleet's demand came from the ENOSPC edges this composite exists to
/// remove; the prod is the signal both arms already agree on.
fn ask_in_force(plane: &Plane) -> bool {
    plane.valve
        && PROD_RENEW_MS.load(Ordering::Relaxed) != 0
        && plane.clock.now_ms() < PROD_UNTIL_MS.load(Ordering::Relaxed)
}

/// The composite's live derivation on `plane`: `Some(ceiling)` while the
/// lever is on and an ask is in force AND the derived ceiling actually sits
/// below the routine (a cycle cost that pins it at the routine is not
/// elastic); `None` otherwise.
fn elastic_ceiling_now(plane: &Plane) -> Option<u64> {
    if !checkpoint_composite_enabled() || !ask_in_force(plane) {
        return None;
    }
    let c = elastic_checkpoint_ceiling_ms(
        plane.reader_poll_ms,
        checkpoint_cycle_ms(),
        plane.writer_routine_ceiling_ms,
    );
    (c < plane.writer_routine_ceiling_ms).then_some(c)
}

/// The promise pair's reading at `now`: the smallest ceiling advertised on
/// a grant whose window is still open.
fn promised_ceiling_ms(now: u64) -> Option<u64> {
    (now < PROMISED_UNTIL_MS.load(Ordering::Relaxed))
        .then(|| PROMISED_CEILING_MS.load(Ordering::Relaxed))
        .filter(|&c| c != 0)
}

/// **The checkpoint ceiling IN FORCE** — what the KV checkpoint task
/// compares its elapsed-since-last-cycle against and tightens its tick to
/// (`checkpoint.rs`). `None` = the routine posture: the task's shipped
/// constant and tick, untouched. `Some(c)` = the smaller of the live
/// derivation and the promise pair, so an advertised window is honoured
/// after the ask lapses (or the lever is latched off) until it closes.
/// One `ArcSwap` load and a few relaxed loads per tick; `None` on a
/// plane-less mount before anything else is read.
pub fn checkpoint_ceiling_in_force_ms() -> Option<u64> {
    let plane = PLANE.load();
    let plane = plane.as_ref()?;
    let now = plane.clock.now_ms();
    match (elastic_ceiling_now(plane), promised_ceiling_ms(now)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// **The published gauge** (`free_grace_checkpoint_ceiling_ms`, the
/// `fence_bound_ms` precedent: the unadorned name is what the machinery is
/// enforcing): the ceiling in force, else the writer's routine ceiling;
/// the constant on a plane-less mount.
pub fn checkpoint_ceiling_ms() -> u64 {
    let plane = PLANE.load();
    let Some(plane) = plane.as_ref() else {
        return crate::meta_backend::kv::checkpoint::CHECKPOINT_MAX_AGE_MS as u64;
    };
    checkpoint_ceiling_in_force_ms().unwrap_or(plane.writer_routine_ceiling_ms)
}

/// **The grant's advertisement** — called by the membership owner at the
/// ONE place a grant is minted (`MembershipOwner::grant_for`): the ceiling
/// this grant promises, and the act of recording that promise. `0` on an
/// owner with no grace plane (it has nothing honest to say about
/// checkpoints — the member falls back to its own derivation of the
/// landing ceiling). An elastic value opens (or tightens) the promise
/// window for one routine ceiling from now; a routine value promises
/// nothing beyond what the routine enforces.
///
/// What travels is the **landing** ceiling (ladder re-derivation item 1
/// composed with this composite): the decision ceiling the task enforces
/// plus the two tick-granularity terms its `tick` evaluates behind —
/// `checkpoint_landing_ceiling_ms` for the routine posture (the trigger +
/// 2 × the flush tick: 1,100 ms on the shipped 50 ms flush),
/// `checkpoint_landing_ceiling_for_elastic` for an elastic one (the task
/// tightens its tick to the decision, so `c + 2 × min(tick, c)`). The
/// promise pair below is kept in DECISION terms — it is what the task
/// compares elapsed time against.
pub fn advertise_checkpoint_ceiling() -> u64 {
    let plane = PLANE.load();
    let Some(plane) = plane.as_ref() else {
        return 0;
    };
    let flush_ms = crate::meta_backend::resolve_flush_interval_ms();
    let Some(c) = elastic_ceiling_now(plane) else {
        return crate::meta_backend::kv::checkpoint::checkpoint_landing_ceiling_ms(flush_ms);
    };
    let now = plane.clock.now_ms();
    let until = now.saturating_add(plane.writer_routine_ceiling_ms);
    let was_until = PROMISED_UNTIL_MS.fetch_max(until, Ordering::Relaxed);
    if was_until <= now {
        // No open window: this grant opens one at its own value.
        PROMISED_CEILING_MS.store(c, Ordering::Relaxed);
    } else {
        // An open window: only ever tighten it (a larger promise is
        // already honoured by the smaller one in force).
        PROMISED_CEILING_MS.fetch_min(c, Ordering::Relaxed);
    }
    crate::meta_backend::kv::checkpoint::checkpoint_landing_ceiling_for_elastic(c, flush_ms)
}

/// Count one checkpoint cycle run with the elastic ceiling in force
/// (called by the checkpoint task's tick beside the cycle).
pub fn note_elastic_checkpoint_cycle() {
    CHECKPOINT_ELASTIC_CYCLES.fetch_add(1, Ordering::Relaxed);
}

/// Checkpoint cycles run with the elastic ceiling in force
/// (`free_grace_checkpoint_elastic_cycles`; 0 without an ask, 0 under
/// `CHECKPOINT_COMPOSITE=0`).
pub fn checkpoint_elastic_cycles() -> u64 {
    CHECKPOINT_ELASTIC_CYCLES.load(Ordering::Relaxed)
}

/// The `SQUEEZEFS_FREE_GRACE_CHECKPOINT_COMPOSITE` lever's latch (the
/// `ACK_PIPELINE` pattern): the writer→member checkpoint composite. `0` =
/// the writer's shipped checkpoint cadence, the routine ceiling on every
/// grant, the constant as the member's floor — the shipped shape exactly.
static CHECKPOINT_COMPOSITE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub(crate) fn checkpoint_composite_enabled() -> bool {
    match CHECKPOINT_COMPOSITE.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = crate::env_knobs::bool_knob("SQUEEZEFS_FREE_GRACE_CHECKPOINT_COMPOSITE", true);
            CHECKPOINT_COMPOSITE.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            on
        }
    }
}

/// Test seam (the `test_set_ack_pipeline` shape).
pub fn test_set_checkpoint_composite(on: Option<bool>) {
    CHECKPOINT_COMPOSITE.store(
        match on {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        },
        Ordering::Relaxed,
    );
}

/// Test seam: `true` ⇔ a preset was in force.
pub fn test_clear_checkpoint_composite() -> bool {
    CHECKPOINT_COMPOSITE.swap(0, Ordering::Relaxed) != 0
}

/// Stamp one release into the hold-phase histograms: `label` is the
/// offset's grace label (defer instant + 1), `released_at` the owner
/// instant of the harvest. Called with the ring lock RELEASED.
fn stamp_hold_phases(label: u64, released_at: u64) {
    let defer_at = label.saturating_sub(1);
    let total = released_at.saturating_sub(defer_at);
    HOLD_PHASES[HOLD_TOTAL].record(Duration::from_millis(total));
    // The covering publish: the first advance whose bound reaches the
    // label (both columns monotone ⇒ one partition point).
    let covered_at = {
        let advances = BOUND_ADVANCES.lock();
        let i = advances.partition_point(|&(bound, _)| bound < label);
        advances.get(i).map(|&(_, at)| at)
    };
    let Some(covered_at) = covered_at else {
        HOLD_UNPLACED.fetch_add(1, Ordering::Relaxed);
        return;
    };
    HOLD_PHASES[HOLD_MIN_ACKED_RELEASED].record(Duration::from_millis(
        released_at.saturating_sub(covered_at),
    ));
    // The first checkpoint completed at or after the defer's own
    // millisecond (the dereference commit precedes the free through the
    // reclaim queue, so a cycle completing in that ms carries it).
    let checkpointed_at = {
        let marks = CHECKPOINT_MARKS.lock();
        let i = marks.partition_point(|&at| at < defer_at);
        marks.get(i).copied()
    };
    match checkpointed_at {
        Some(ck) if ck <= covered_at => {
            HOLD_PHASES[HOLD_DEFER_CHECKPOINTED].record(Duration::from_millis(ck - defer_at));
            HOLD_PHASES[HOLD_CHECKPOINTED_MIN_ACKED].record(Duration::from_millis(covered_at - ck));
        }
        _ => {
            HOLD_UNPLACED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Fold one harvest batch's mean residence into the live hold gauge
/// (`free_grace_hold_ms` — EWMA α = 1/4 over batches, the
/// `sample_alloc_rate` shape): the MEASURED loop latency the capacity law
/// multiplies the churn by. A batch is the natural sample — every offset
/// in it was covered by the same bound advance.
fn note_hold_sample(labels: &[u64], released_at: u64) {
    if labels.is_empty() {
        return;
    }
    let sum: u64 = labels
        .iter()
        .map(|&l| released_at.saturating_sub(l.saturating_sub(1)))
        .sum();
    let inst = sum / labels.len() as u64;
    let old = HOLD_EWMA_MS.load(Ordering::Relaxed);
    let ewma = if old == 0 {
        inst
    } else {
        old.saturating_sub(old.div_ceil(4)) + inst / 4
    };
    HOLD_EWMA_MS.store(ewma, Ordering::Relaxed);
}

/// **The measured hold** (`free_grace_hold_ms`): the live EWMA of the
/// per-offset residence at release — what a freed offset's lane share is
/// tied up for. 0 until the first release.
pub fn hold_ms() -> u64 {
    HOLD_EWMA_MS.load(Ordering::Relaxed)
}

/// `free_grace_hold_phase_ns`: the per-stage histograms, keyed by
/// [`HOLD_PHASE_NAMES`].
pub fn hold_phase_json() -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (i, name) in HOLD_PHASE_NAMES.iter().enumerate() {
        out.insert((*name).to_string(), HOLD_PHASES[i].to_json());
    }
    serde_json::Value::Object(out)
}

/// Releases one or more of whose stages could not be placed
/// (`free_grace_hold_unplaced`; the exact-sum law reads over the placed
/// population).
pub fn hold_unplaced() -> u64 {
    HOLD_UNPLACED.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// The lane-visible decomposition (finding 15 term 2): released → served →
// visible, and the lane-push lever
// ---------------------------------------------------------------------------

/// **The authority's release mark**: a grace-released block of a lane this
/// mount does not own reached the free list NOW. Called by the allocator's
/// harvest publish for foreign-lane blocks only (a block of this mount's
/// own lane is visible to its allocator the instant it is published, so
/// there is no hop to measure). No plane ⇒ nothing recorded.
pub fn mark_lane_release(vol_tag: u64, block_idx: u64) {
    let Some(now) = owner_now_ms() else {
        return;
    };
    let _ = LANE_RELEASE_MARKS.insert_sync((vol_tag, block_idx), now);
}

/// **The lane harvest took the block** (authority side): the age of its
/// release mark, ms on the owner clock, stamped into `released_served`
/// and returned so the reply can carry it. `None` ⇔ no mark (counted on
/// `alloc_lane_visible_unplaced`; the reply carries `u64::MAX`).
pub fn take_lane_release(vol_tag: u64, block_idx: u64) -> Option<u64> {
    let released_at = LANE_RELEASE_MARKS
        .remove_sync(&(vol_tag, block_idx))
        .map(|(_, at)| at);
    let (Some(released_at), Some(now)) = (released_at, owner_now_ms()) else {
        LANE_VISIBLE_UNPLACED.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    let age = now.saturating_sub(released_at);
    LANE_VISIBLE_PHASES[LANE_VISIBLE_RELEASED_SERVED].record(Duration::from_millis(age));
    Some(age)
}

/// The sentinel a reply carries for a served block with no release mark.
pub const LANE_RELEASE_AGE_UNPLACED: u64 = u64::MAX;

/// **The co-writer adopted the block** (co-writer side): stamp the three
/// stages of one served block — `released_served` as the authority
/// measured it (carried on the reply), `served_visible` as this mount's
/// own round trip, `total` their sum — so the family closes exactly per
/// sample. An unplaced age stamps nothing and counts on
/// `alloc_lane_visible_unplaced` here too.
pub fn note_lane_visible(released_served_ms: u64, served_visible_ms: u64) {
    if released_served_ms == LANE_RELEASE_AGE_UNPLACED {
        LANE_VISIBLE_UNPLACED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    LANE_VISIBLE_PHASES[LANE_VISIBLE_RELEASED_SERVED]
        .record(Duration::from_millis(released_served_ms));
    LANE_VISIBLE_PHASES[LANE_VISIBLE_SERVED_VISIBLE]
        .record(Duration::from_millis(served_visible_ms));
    LANE_VISIBLE_PHASES[LANE_VISIBLE_TOTAL].record(Duration::from_millis(
        released_served_ms.saturating_add(served_visible_ms),
    ));
}

/// `alloc_lane_visible_phase_ns`: the per-stage histograms, keyed by
/// [`LANE_VISIBLE_PHASE_NAMES`]. On an authority only `released_served`
/// has samples (it measures the wait on its own list); on a co-writer all
/// three, exact-sum.
pub fn lane_visible_phase_json() -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (i, name) in LANE_VISIBLE_PHASE_NAMES.iter().enumerate() {
        out.insert((*name).to_string(), LANE_VISIBLE_PHASES[i].to_json());
    }
    serde_json::Value::Object(out)
}

/// Served lane blocks the release ledger could not place
/// (`alloc_lane_visible_unplaced`).
pub fn lane_visible_unplaced() -> u64 {
    LANE_VISIBLE_UNPLACED.load(Ordering::Relaxed)
}

/// Release marks outstanding: grace-released foreign-lane blocks on this
/// authority's free lists that no lane harvest has taken yet.
pub fn lane_release_marks() -> u64 {
    LANE_RELEASE_MARKS.len() as u64
}

/// `SQUEEZEFS_FREE_GRACE_LANE_PUSH` (default on), read once and cached
/// (the free path and the renewal path may not pay a getenv).
pub fn lane_push_enabled() -> bool {
    match LANE_PUSH.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = crate::env_knobs::bool_knob("SQUEEZEFS_FREE_GRACE_LANE_PUSH", true);
            LANE_PUSH.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            on
        }
    }
}

/// Test seam (the `test_set_ack_pipeline` shape).
pub fn test_set_lane_push(on: Option<bool>) {
    LANE_PUSH.store(
        match on {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        },
        Ordering::Relaxed,
    );
}

/// Test seam: `true` ⇔ a preset was in force.
pub fn test_clear_lane_push() -> bool {
    LANE_PUSH.swap(0, Ordering::Relaxed) != 0
}

/// Install the authority's release hook (the multi-writer arm; the rigs).
pub fn install_release_hook(hook: ReleaseHook) {
    RELEASE_HOOK.store(Some(Arc::new(hook)));
}

/// Uninstall it (disarm / unmount / test teardown).
pub fn uninstall_release_hook() {
    RELEASE_HOOK.store(None);
}

/// **Release on ack** (the lever's authority half): a BINDING member's
/// acknowledgement just advanced (lever (d)'s one-compare gate), so the
/// covered offsets are released NOW — every ring harvested to its
/// uncovered front through the installed hook — instead of at the next
/// demand event (a free, an allocation, a harvest RPC), which on a fleet
/// whose writers are the ones parked may be seconds away. Rate-limited by
/// exactly lever (d)'s law (`refresh_on_ack_interval_ms`, sharing its
/// refresh instant): the hook's harvest runs the recompute, so the two
/// arms are one act at one cadence.
fn release_on_ack() {
    let Some(hook) = RELEASE_HOOK.load_full() else {
        return;
    };
    let plane = PLANE.load();
    let Some(plane) = plane.as_ref() else {
        return;
    };
    let now = plane.clock.now_ms();
    // The floor in force (under the composite the members answer twice
    // as often, so the min can change twice as often — the limit follows).
    let floor_ms = live_prod_floor_ms(plane);
    let members = MEMBERS.load(Ordering::Relaxed);
    let scan_ms = BOUND_SCAN_EWMA_NS
        .load(Ordering::Relaxed)
        .div_ceil(1_000_000);
    let interval = refresh_on_ack_interval_ms(floor_ms, members, scan_ms);
    if now.saturating_sub(LAST_BOUND_REFRESH_MS.load(Ordering::Relaxed)) < interval {
        return;
    }
    LANE_PUSH_RELEASES.fetch_add(1, Ordering::Relaxed);
    hook();
}

/// Release-on-ack runs (`free_grace_lane_push_releases`).
pub fn lane_push_releases() -> u64 {
    LANE_PUSH_RELEASES.load(Ordering::Relaxed)
}

/// Install the authority's lane-supply source (the multi-writer arm; the
/// rigs).
pub fn install_lane_supply_source(src: LaneSupplySource) {
    LANE_SUPPLY_SOURCE.store(Some(Arc::new(src)));
}

/// Uninstall it (disarm / unmount / test teardown).
pub fn uninstall_lane_supply_source() {
    LANE_SUPPLY_SOURCE.store(None);
}

/// **The renewal grant's lane-supply hint** (the lever's wire half, owner
/// side): the blocks of `member_id`'s lane sitting on this authority's
/// free lists — released, unserved, reachable by that member alone. O(1)
/// per volume through the installed source (per-lane counters maintained
/// inside the free set's insert/remove); 0 with the lever off, no source
/// (every mount that is not a multi-writer authority), or a member with
/// no lane (a reader). Never a scan in the renewal hot op (KD-FG-4).
pub fn lane_supply_for_member(member_id: &str) -> u64 {
    if !lane_push_enabled() {
        return 0;
    }
    let Some(src) = LANE_SUPPLY_SOURCE.load_full() else {
        return 0;
    };
    let n = src(member_id);
    if n > 0 {
        LANE_PUSH_HINTS.fetch_add(1, Ordering::Relaxed);
    }
    n
}

/// Grants that carried a nonzero hint (`free_grace_lane_push_hints`).
pub fn lane_push_hints() -> u64 {
    LANE_PUSH_HINTS.load(Ordering::Relaxed)
}

/// **The member learned a hint** (co-writer side, from the adopted grant):
/// the value is kept for the refill decision, and a nonzero one wakes
/// every parked refill (the ahead task, a bounded allocation park) so the
/// lane harvest runs on THIS round trip's heels rather than at its own
/// cadence. Inert with the lever off.
pub fn note_lane_supply_hint(blocks: u64) {
    if !lane_push_enabled() {
        return;
    }
    LANE_SUPPLY_HINT.store(blocks, Ordering::Relaxed);
    if blocks > 0 {
        LANE_PUSH_WAKES.fetch_add(1, Ordering::Relaxed);
        LANE_SUPPLY_WAKE.notify_waiters();
    }
}

/// The last learned hint (co-writer side; 0 = the authority's list holds
/// nothing for this lane, or no hint has arrived).
pub fn lane_supply_hint() -> u64 {
    LANE_SUPPLY_HINT.load(Ordering::Relaxed)
}

/// Hints that woke a refill (`free_grace_lane_push_wakes`).
pub fn lane_push_wakes() -> u64 {
    LANE_PUSH_WAKES.load(Ordering::Relaxed)
}

/// The co-writer's refill wake (see [`note_lane_supply_hint`]).
pub fn lane_supply_wake() -> &'static squeezefs_ipc::sqz_notify::Notify {
    &LANE_SUPPLY_WAKE
}

/// **The pushed-refill decision** (co-writer side, pure): harvest now ⇔
/// the lever is on, the authority's last hint says the lane has supply on
/// its list, and this allocator is OWED blocks (its shipped frees came back
/// `Freed`). The watermark plays no part — a quiet lane's rate-derived
/// watermark decays to 0, which is exactly when the routine ahead tick
/// goes dark while the supply sits on the authority.
pub fn lane_push_wants_harvest(hint_blocks: u64, owed_blocks: u64) -> bool {
    lane_push_enabled() && hint_blocks > 0 && owed_blocks > 0
}

/// **The pushed-refill decision, per VOLUME** (the two-volume law,
/// `.benchmarks/2026-09-07-cowriter-lane-aware-placement.md`): the hint is
/// SUMMED over the authority's volumes, so it cannot name the one holding
/// the supply — and a peer's rewrites of this lane's blocks put supply on
/// a list no owed ledger here knows about. Harvest ⇔
/// [`lane_push_wants_harvest`] (owed), OR the hint is nonzero and THIS
/// volume's lane-reachable stock is 0: a dry volume asking is one RPC that
/// either refills it or proves the supply is its sibling's (which the
/// lane-aware placement carries meanwhile). A stocked volume owed nothing
/// never asks (no wasted RTT).
pub fn lane_push_wants_harvest_on_volume(
    hint_blocks: u64,
    owed_blocks: u64,
    reachable_blocks: u64,
) -> bool {
    lane_push_wants_harvest(hint_blocks, owed_blocks)
        || (lane_push_enabled() && hint_blocks > 0 && reachable_blocks == 0)
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
/// memory budget, floored at `RING_CAP_FLOOR`. Deliberately NOT an R5
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
// The pressure valve's derivations (rung-20 residual 6)
// ---------------------------------------------------------------------------

/// **The graded pressure signal**: how long the writer's supply lasts at
/// the storm's own measured deferral rate, in ms. `None` = no reading.
///
/// Every term comes from state the ring already holds, so the signal costs
/// two peeks of a deque the harvest locks anyway:
///
/// * the **rate** is `held ÷ (newest label − oldest label)` — the labels
///   are owner-clock milliseconds and the deque is label-ordered by
///   construction, so its two ends ARE the measurement window. Fewer than
///   two entries carry no rate at all, and reading one as a cliff would
///   fence a reader for a single free;
/// * the **supply** is the smaller of the ring's headroom (`cap − held`,
///   the RAM bound) and the volume's free blocks (the space bound — the
///   one the field's 32 GiB lane hit; on a lane-partitioned volume this is
///   the LANE's share, because `virgin_bytes` already divides by the
///   partition width).
///
/// A burst that lands inside one millisecond reads as a zero runway rather
/// than a division by zero, which is honest: at that rate the supply is
/// already gone.
pub fn runway_ms(held: u64, span_ms: u64, cap: usize, free_supply_blocks: u64) -> Option<u64> {
    if held < 2 {
        return None;
    }
    let headroom = (cap as u64).saturating_sub(held);
    let supply = headroom.min(free_supply_blocks);
    // `span/held` is the mean inter-arrival time of the measurement
    // window; a sub-millisecond window rounds to 0 and the answer with it.
    Some(supply.saturating_mul(span_ms) / held)
}

/// **Rung (b)**: the fence deadline in force for a given runway — the
/// routine bound when there is nothing to say, the pressure bound at the
/// cliff, and a straight line between (so there is no threshold cliff to
/// oscillate across).
///
/// `pressure_ms` is the FLOOR by construction (`arm_owner_plane` derives
/// it as at least one honest [`ack_cycle`]), which is what keeps rung (b)
/// from ever fencing a reader that is answering as designed.
pub fn effective_bound_ms_from(runway_ms: Option<u64>, fence_ms: u64, pressure_ms: u64) -> u64 {
    let Some(runway) = runway_ms else {
        return fence_ms;
    };
    if fence_ms <= pressure_ms {
        return fence_ms;
    }
    let runway = runway.min(fence_ms);
    // u128 so a long runway against a long bound cannot overflow the
    // product on the way to a value that is at most `fence_ms`.
    let slack = (u128::from(fence_ms - pressure_ms) * u128::from(runway)) / u128::from(fence_ms);
    pressure_ms.saturating_add(slack as u64)
}

/// The shortest interval at which a member's acknowledgement can carry
/// something NEW: its revalidation pass cadence (nothing about a reader's
/// answer changes between two passes) floored at the clock-skew/RTT bound
/// (nothing about the wire's answer changes faster than that).
///
/// This is rung (a)'s cadence floor, and it is a correctness floor as much
/// as an economy one — see [`ReaderAckLadder`].
pub fn ack_refresh_floor(clocks: &LeaseClocks) -> Duration {
    crate::ro_coherence::reader_revalidate_interval().max(clocks.skew_max)
}

/// **Rung (a)'s law**: the plane's own [`ack_cycle`] inverted against the
/// measured runway, resolved ONCE at arm because the free path may not pay
/// a knob read or a poller construction per free.
#[derive(Debug, Clone, Copy)]
pub struct ProdParams {
    /// The routine renewal cadence — the prod's ceiling (a "prod" that
    /// slowed a member down would not be one).
    renew_ms: u64,
    /// The terms of the cycle a faster beat CANNOT shrink: the reader's
    /// three staleness bounds, the skew and `D_purge`.
    reader_lag_ms: u64,
    /// [`ack_refresh_floor`] — the ROUTINE floor.
    floor_ms: u64,
    /// The reader's routine pass interval `P` and the clock-skew bound —
    /// the live floor's two inputs beside the writer's advertised ceiling
    /// ([`Self::floor_for`]).
    pass_ms: u64,
    skew_ms: u64,
}

impl ProdParams {
    /// Resolve from the plane's clocks (one poller construction, at arm).
    pub fn derive(clocks: &LeaseClocks) -> Self {
        let renew_ms = clocks.renew_interval.as_millis() as u64;
        let cycle_ms = ack_cycle(clocks).as_millis() as u64;
        Self {
            renew_ms,
            reader_lag_ms: cycle_ms.saturating_sub(renew_ms.saturating_mul(3)),
            floor_ms: ack_refresh_floor(clocks).as_millis() as u64,
            pass_ms: crate::ro_coherence::reader_revalidate_interval().as_millis() as u64,
            skew_ms: clocks.skew_max.as_millis() as u64,
        }
    }

    /// **The LIVE acknowledgement-refresh floor** for a writer whose
    /// checkpoint ceiling is `checkpoint_ceiling_ms` (the composite): a
    /// member's pass runs at `clamp(ask, ceiling, P)`, so its answer can
    /// change every `min(P, ceiling)` — floored at the clock-skew bound
    /// like the routine floor. At the writer's routine ceiling this IS
    /// [`ack_refresh_floor`] (the two derive from the same cadence unless
    /// an explicit `SQUEEZEFS_META_REVALIDATE_MS` polls SLOWER than the
    /// writer checkpoints); the call sites consult it only while an
    /// elastic ceiling is in force, so the routine posture is `floor_ms`
    /// verbatim either way.
    pub fn floor_for(&self, checkpoint_ceiling_ms: u64) -> u64 {
        self.pass_ms.min(checkpoint_ceiling_ms).max(self.skew_ms)
    }

    /// The cadence to grant a member the writer is waiting on, or `None`
    /// when the routine one already fits inside the runway. `floor_ms` is
    /// the floor in force ([`Self::floor_for`]; the routine one is
    /// [`ack_refresh_floor`]).
    ///
    /// `ack_cycle = 3 × beat + reader_lag`, so the beat that lets a whole
    /// acknowledgement cycle complete before the supply runs out is
    /// `(runway − reader_lag) / 3`. Below the floor the answer cannot
    /// arrive any sooner however hard the writer asks — that is when rung
    /// (b) takes over.
    pub fn cadence_for(&self, runway_ms: u64, floor_ms: u64) -> Option<u64> {
        let want = runway_ms.saturating_sub(self.reader_lag_ms) / 3;
        let cadence = want.clamp(floor_ms.min(self.renew_ms), self.renew_ms);
        (cadence < self.renew_ms).then_some(cadence)
    }
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

impl Drop for GraceRing {
    /// Reconcile the PROCESS gauges for anything still held: a volume
    /// retired mid-flight (`volume remove-data`, an offline tool's
    /// short-lived allocator) must not leave `free_grace_offsets` claiming
    /// space that no allocator owns any more. The offsets themselves need
    /// nothing — the volume they belonged to is gone.
    fn drop(&mut self) {
        let held = self.len.swap(0, Ordering::AcqRel) as u64;
        let bytes = self.bytes.swap(0, Ordering::AcqRel);
        if held != 0 {
            HELD_OFFSETS.fetch_sub(held, Ordering::Relaxed);
            HELD_BYTES.fetch_sub(bytes, Ordering::Relaxed);
            log::warn!(
                "freed-offset grace ring dropped with {held} offset(s) ({bytes} B) still held: \
                 the volume was retired before its readers acknowledged. The offsets are gone \
                 with the volume, so nothing is stranded — but the readers' cached bindings to \
                 them are only void because the volume is (spec §6.8 item 3)"
            );
        }
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
        self.harvest_with(max, false, u64::MAX, u64::MAX)
    }

    /// The routine harvest with the volume's free supply attached — the
    /// allocator's form, and the one that can read SPACE pressure (the
    /// field's binding constraint: the ring was nowhere near its cap when
    /// the 32 GiB lane ran out). `u64::MAX` = space is not a constraint
    /// here (an unbounded/offline allocator).
    /// `lane_reachable_blocks` is the SUSTAIN campaign's site-0 input
    /// (KD-FG-10): the supply of the caller's own residue class — what
    /// actually troughs on a recycle-bound stream, where the passed-global
    /// number accumulates foreign-lane releases and never does. PR 1
    /// consumes it as a counted observation only; PR 3 re-bases the runway
    /// on it under the `DEMAND` lever.
    pub fn harvest_with_supply(
        &self,
        max: usize,
        free_supply_blocks: u64,
        lane_reachable_blocks: u64,
    ) -> Vec<u64> {
        self.harvest_with(max, false, free_supply_blocks, lane_reachable_blocks)
    }

    /// The pressure harvest: identical, except the fence deadline is the
    /// PRESSURE bound (one acknowledgement cycle) rather than the routine
    /// one. It never releases an unacknowledged offset without evicting the
    /// member responsible — pressure buys promptness, never a broken
    /// promise (see the module docs' pressure ruling).
    pub fn harvest_pressure(&self, max: usize) -> Vec<u64> {
        // Allocation is about to refuse: the supply IS gone, whatever the
        // ring's own arithmetic would have estimated.
        self.harvest_with(max, true, 0, 0)
    }

    fn harvest_with(
        &self,
        max: usize,
        pressure: bool,
        free_supply_blocks: u64,
        lane_reachable_blocks: u64,
    ) -> Vec<u64> {
        if self.len.load(Ordering::Acquire) == 0 {
            return Vec::new();
        }
        let (oldest, newest, runway) = {
            let guard = self.entries.lock();
            let ends = guard.front().zip(guard.back());
            let runway = ends.and_then(|(f, b)| {
                runway_ms(
                    guard.len() as u64,
                    b.label.saturating_sub(f.label),
                    self.cap,
                    free_supply_blocks,
                )
            });
            (
                guard.front().map(|e| e.label),
                guard.back().map(|e| e.label).unwrap_or(0),
                runway,
            )
        };
        // Site 0 — the standing recycle-coupling detector (sustain
        // campaign, KD-FG-9): the ring is non-empty AND the oldest held
        // offset has aged past the PHYSICS floor (a healthy loop would
        // have released it) AND the caller's LANE-REACHABLE supply sits
        // at the trough (releases consumed within a beat of publish).
        // Refusal-edge-independent by design — the motivating row reached
        // no ENOSPC, no empty lane harvest, no stall, and still decayed.
        // Evaluated BEFORE `note_pressure` so the mark it deposits is
        // consumed by THIS pass's rung a′ / L3 arms (same-pass coupling);
        // two compares on values this pass already holds.
        if let (Some(oldest), Some(plane)) = (oldest, PLANE.load_full()) {
            let now = plane.clock.now_ms();
            let coupled = (now.saturating_sub(plane.demand_floor_ms) > oldest
                && lane_reachable_blocks <= HARVEST_BATCH as u64)
                // The refusal edges (§5.4 sites 1–3): every one funnels
                // through the PRESSURE harvest — allocation about to
                // refuse / an empty lane harvest's last pass — and a
                // non-empty ring there IS the coupling, whatever the age.
                || pressure;
            if coupled {
                DEMAND_WAITS.fetch_add(1, Ordering::Relaxed);
                // PR 3's consumer: the MARK (TTL'd like the runway
                // reading) + the graded coupling face. The mark never
                // feeds rung (b) — see `note_pressure`.
                if demand_enabled() {
                    DEMAND_UNTIL_MS
                        .store(now.saturating_add(plane.reading_ttl_ms), Ordering::Relaxed);
                    let age = now.saturating_sub(oldest);
                    let floor = plane.demand_floor_ms.max(1);
                    DEMAND_AGE_PCT.store(
                        (age.saturating_sub(floor)).saturating_mul(100) / floor,
                        Ordering::Relaxed,
                    );
                }
            }
        }
        // The cliff reads as a zero runway (see `harvest_pressure`), which
        // is exactly the pressure bound through the graded form — the
        // pre-valve behaviour of this arm, reproduced rather than special-cased.
        let runway = if pressure { Some(0) } else { runway };
        note_pressure(runway, newest);
        // Lever (d): a binding member's ack arrived since the last
        // recompute — recompute now (rate-limited) rather than at the next
        // floor beat. The bound is read AFTER both recompute arms (this one
        // and `note_pressure`'s cadence arm), so a recompute this pass
        // releases in THIS pass, not the next harvest's.
        refresh_bound_on_dirty();
        let mut bound = self::bound();
        let mut forced = false;
        if let Some(oldest) = oldest {
            if oldest > bound {
                let over_cap = self.len.load(Ordering::Acquire) >= self.cap;
                let deadline_ms = tightened_bound_ms(runway);
                let now = owner_now_ms().unwrap_or(0);
                let expired = deadline_ms > 0 && now.saturating_sub(deadline_ms) >= oldest;
                if expired || over_cap {
                    // Name the deadline that actually fired: an operator
                    // reading a fence at 40 s against a routine bound of
                    // 76 s must be told rung (b) moved it, and why.
                    let base = fence_bound_base_ms();
                    let why = if !expired {
                        "the grace ring reached its cap".to_string()
                    } else if deadline_ms < base {
                        format!(
                            "the grace bound expired at {deadline_ms} ms — TIGHTENED from \
                             {base} ms by the pressure valve, because the writer's supply \
                             would not have lasted the routine bound"
                        )
                    } else {
                        format!("the grace bound expired ({deadline_ms} ms)")
                    };
                    force_progress(oldest, &why);
                    // Re-read: `force_progress` recomputes the true minimum
                    // (which may simply have been a stale cadence-published
                    // value) and republishes it after any eviction.
                    bound = self::bound();
                    forced = bound >= oldest;
                }
            }
        }
        let mut out = Vec::new();
        let mut labels = Vec::new();
        let mut released_bytes = 0u64;
        let release_now = owner_now_ms();
        {
            let mut guard = self.entries.lock();
            while out.len() < max {
                match guard.front() {
                    Some(e) if e.label <= bound => {
                        let e = guard.pop_front().expect("front peeked");
                        released_bytes += e.size;
                        out.push(e.offset);
                        labels.push(e.label);
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
            // `free_grace_residence_ms` (sustain campaign §8) — the
            // per-offset loop latency, `now − (label − 1)` — and its
            // per-stage decomposition (hold-time campaign), stamped with
            // the ring lock released: the stage lookups take the mark
            // deques, leaf locks of their own.
            if let Some(now) = release_now {
                for &label in &labels {
                    RESIDENCE_MS.record(Duration::from_millis(
                        now.saturating_sub(label.saturating_sub(1)),
                    ));
                    RESIDENCE_SAMPLES.fetch_add(1, Ordering::Relaxed);
                    stamp_hold_phases(label, now);
                }
                note_hold_sample(&labels, now);
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

/// **The acknowledgement-refresh floor IN FORCE** — rung (a)'s cadence
/// floor and the rate-limit floor of lever (d) and the lane push: the
/// routine [`ack_refresh_floor`] (resolved at arm into `ProdParams`), or,
/// while the composite's elastic checkpoint ceiling is in force, the
/// halved floor a member's pass can now follow ([`ProdParams::floor_for`]).
/// The explicit-bounds seam (no `ProdParams`) reads its reading TTL as
/// before. Never 0.
fn live_prod_floor_ms(plane: &Plane) -> u64 {
    let floor = match (plane.prod.as_ref(), checkpoint_ceiling_in_force_ms()) {
        (Some(p), Some(ceiling)) => p.floor_for(ceiling),
        (Some(p), None) => p.floor_ms,
        (None, _) => plane.reading_ttl_ms,
    };
    floor.max(1)
}

/// **Rung (b)** — the fence deadline this harvest evaluates, and the
/// counting of the act.
///
/// The counter is incremented HERE rather than in [`effective_bound_ms`]
/// (the gauge) on purpose: a tightening is a decision a harvest made, not
/// a number an operator read.
fn tightened_bound_ms(runway: Option<u64>) -> u64 {
    let plane = PLANE.load();
    let Some(plane) = plane.as_ref() else {
        return 0;
    };
    if !plane.valve {
        // The A/B control: the routine bound, and the pressure bound only
        // at the allocation cliff — the pre-valve arm, verbatim.
        return match runway {
            Some(0) => plane.pressure_fence_ms,
            _ => plane.fence_ms,
        };
    }
    let eff = effective_bound_ms_from(runway, plane.fence_ms, plane.pressure_fence_ms);
    if eff < plane.fence_ms {
        BOUND_TIGHTENINGS.fetch_add(1, Ordering::Relaxed);
    }
    eff
}

/// **Rung (a)** — publish the pressure reading and, when the runway is
/// short enough that the routine beat cannot fit an acknowledgement cycle
/// inside it, ask the members the writer is waiting on to come back
/// sooner.
///
/// Two acts, both cheap and both bounded:
///
/// 1. the tightened cadence is deposited for [`take_prod_cadence`], which
///    the owner's renewal path hands to a member that is actually behind;
/// 2. the bound is recomputed ONCE PER PRODDED CADENCE — the interval at
///    which a member's answer can change — so a storm does not pay the
///    O(members) minimum per free to learn what the owner's sweep would
///    have told it anyway.
///
/// The reading is deliberately last-writer-wins across volumes rather than
/// a maintained minimum: a pressured volume harvests orders of magnitude
/// more often than a quiet one, so the busy answer dominates by frequency,
/// and a stale one expires on its own within one routine beat.
fn note_pressure(runway: Option<u64>, newest_label: u64) {
    let plane = PLANE.load();
    let Some(plane) = plane.as_ref() else {
        return;
    };
    if !plane.valve {
        return;
    }
    let Some(runway) = runway else {
        // This ring has no rate to report. Never relax a live reading from
        // a busier volume on the strength of a quiet one — let it expire.
        return;
    };
    let now = plane.clock.now_ms();
    RUNWAY_MS.store(runway, Ordering::Relaxed);
    RUNWAY_UNTIL_MS.store(now.saturating_add(plane.reading_ttl_ms), Ordering::Relaxed);
    // Rung a′ (§5.2, the demand arm): a live demand mark asks the FLOOR
    // cadence of every member behind the held labels — the same gate, the
    // same delivery, the same lane as rung (a). The space arm's cadence,
    // where one computed, is only ever tightened by it (the floor is the
    // shortest interval a member's answer can change in). The mark NEVER
    // feeds rung (b): the fence deadline below tightens off the space
    // runway alone — a demand-prodded healthy reader is asked to answer
    // sooner, never fenced sooner (constraint 2, by construction).
    // The floor in force: the routine `ack_refresh_floor`, or — while the
    // composite's elastic ceiling is in force — the halved one a member's
    // pass can now follow (`ProdParams::floor_for`). Read once per
    // harvest; the first ask under a fresh storm is computed at the
    // routine floor and puts the ask in force, the next harvest reads the
    // halved one.
    let floor_ms = live_prod_floor_ms(plane);
    let space_cadence = plane
        .prod
        .as_ref()
        .and_then(|p| p.cadence_for(runway, floor_ms));
    let demand = demand_enabled() && demand_live();
    // `cadence_for` clamps into [floor, renew], so under demand the
    // effective cadence is exactly the floor — the strongest honest ask.
    let cadence = match (space_cadence, demand) {
        (Some(c), true) => Some(c.min(floor_ms)),
        (Some(c), false) => Some(c),
        (None, true) => Some(floor_ms),
        (None, false) => None,
    };
    let Some(cadence) = cadence else {
        return;
    };
    PROD_FROM_DEMAND.store(
        u64::from(demand && space_cadence.is_none()),
        Ordering::Relaxed,
    );
    PROD_RENEW_MS.store(cadence, Ordering::Relaxed);
    PROD_LABEL.fetch_max(newest_label, Ordering::Relaxed);
    PROD_UNTIL_MS.store(now.saturating_add(plane.reading_ttl_ms), Ordering::Relaxed);
    // L3 (§5.3): the valve's existing rate-limited refresh, its gate
    // widened from "a prod cadence was computed" to "…or the demand mark
    // is live" — the rate limit stays the cadence in force (under demand
    // that IS the floor), so the law `refreshes ≤ elapsed ÷ floor` holds
    // verbatim. The sweep stays as the idle-fleet backstop.
    let last = LAST_BOUND_REFRESH_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= cadence {
        recompute_bound(now);
    }
}

/// One rate-limited O(members) recompute of the published bound (L3's
/// cadence arm and lever (d)'s dirty arm share it): stamps the refresh
/// instant, counts the act, and folds the scan's own wall cost into the
/// EWMA the dirty arm's floor reads.
fn recompute_bound(now: u64) {
    LAST_BOUND_REFRESH_MS.store(now, Ordering::Relaxed);
    BOUND_REFRESHES.fetch_add(1, Ordering::Relaxed);
    BOUND_DIRTY.store(false, Ordering::Relaxed);
    if let Some(owner) = crate::membership::installed_owner() {
        let t0 = std::time::Instant::now();
        owner.refresh_free_grace_bound();
        let inst = t0.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let old = BOUND_SCAN_EWMA_NS.load(Ordering::Relaxed);
        let ewma = if old == 0 {
            inst
        } else {
            old.saturating_sub(old.div_ceil(4)) + inst / 4
        };
        BOUND_SCAN_EWMA_NS.store(ewma, Ordering::Relaxed);
    }
}

/// **Lever (d), the owner's side of the ack**: called by
/// `MembershipOwner::renew` when a member's recorded acknowledgement
/// ADVANCED, with the value it advanced from. Only a member whose old
/// value sat at or below the published bound can have been the minimum
/// (or tied for it), so only those mark the bound dirty — one compare in
/// the plane's hot op, never the O(members) scan (KD-FG-4's law stands:
/// the scan runs on the harvest path, rate-limited).
pub(crate) fn note_member_ack_advanced(previous_acked: u64) {
    if !armed() || !refresh_on_ack_enabled() {
        return;
    }
    if previous_acked <= bound() {
        BOUND_DIRTY.store(true, Ordering::Relaxed);
        // The lane-push lever's authority half: the release follows the
        // ack, not the next demand event (finding 15 term 2).
        if lane_push_enabled() {
            release_on_ack();
        }
    }
}

/// Lever (d)'s rate limit — the pure form (tie-tested in
/// `tests/derivation_sweep_tests.rs`): the minimum over `members` can
/// change at most `members` times per acknowledgement-refresh floor (each
/// member's answer changes at most once per floor), so recomputes are
/// admitted no closer than `floor ÷ members`; and a recompute costing
/// `scan_ms` may not run more than half the time, so never closer than
/// `2 × scan_ms`. No constant: at 8 members and a µs-class scan the floor
/// term governs (125 ms on the shipped 1 s floor); at 15 k members the
/// scan term does.
pub fn refresh_on_ack_interval_ms(floor_ms: u64, members: u64, scan_ms: u64) -> u64 {
    (floor_ms / members.max(1)).max(scan_ms.saturating_mul(2))
}

/// **Lever (d), the harvest's side**: recompute the bound when the dirty
/// mark is set, rate-limited to the floor the min can honestly change at
/// — `ack_refresh_floor ÷ members` (each member's answer changes at most
/// once per floor) — and never faster than twice the scan's own measured
/// cost. A mark the limit defers stays set for the next harvest.
fn refresh_bound_on_dirty() {
    if !BOUND_DIRTY.load(Ordering::Relaxed) {
        return;
    }
    let plane = PLANE.load();
    let Some(plane) = plane.as_ref() else {
        return;
    };
    let now = plane.clock.now_ms();
    // The floor in force (under the composite the members answer twice
    // as often, so the min can change twice as often — the limit follows).
    let floor_ms = live_prod_floor_ms(plane);
    let members = MEMBERS.load(Ordering::Relaxed);
    let scan_ms = BOUND_SCAN_EWMA_NS
        .load(Ordering::Relaxed)
        .div_ceil(1_000_000);
    let interval = refresh_on_ack_interval_ms(floor_ms, members, scan_ms);
    if now.saturating_sub(LAST_BOUND_REFRESH_MS.load(Ordering::Relaxed)) < interval {
        return;
    }
    BOUND_REFRESHES_ON_ACK.fetch_add(1, Ordering::Relaxed);
    recompute_bound(now);
}

/// The tightened renewal cadence to grant `acked_free_epoch`'s member, and
/// the act of counting the ask (rung (a)'s engagement instrument) —
/// called at the ONE place a cadence is handed out, the owner's renewal.
///
/// `None` when no prod is in force, when this member has already
/// acknowledged past everything the plane holds (prodding a member that
/// is not holding the free list would buy nothing and cost it beats), or
/// when an expired ask has finished its decay.
///
/// **Finding 18 — an expired ask DECAYS, it never snaps**
/// (`.benchmarks/2026-08-25-s11-freeloop-stall.md` §Finding 18): under a
/// storm the runway reading sawtooths — every release crest lets the
/// reading lapse — and the pre-fix expiry arm handed the very next
/// renewal beat the ROUTINE cadence. One routine grant is one
/// routine-beat acknowledgement hole, and the owner's min-composition
/// inherits the widest member's hole (the f16a row: `bound_age`
/// 15.5–22.6 s against PR 5's ≤ 12 s gate with the member ladder itself
/// on budget). So while the plane still HOLDS offsets, an expired ask
/// relaxes ONE DOUBLING STEP per reading window (the write-pipeline
/// probe governor's bleed-to-routine pattern — derived, never a knob),
/// re-tightens fully at the next pressure reading (`note_pressure`
/// overwrites unconditionally), and retires at routine. A ring that has
/// drained retires the ask immediately — no ask on a plane holding
/// nothing, the shipped economy law unchanged.
pub fn take_prod_cadence(acked_free_epoch: u64) -> Option<u64> {
    let mut cadence = PROD_RENEW_MS.load(Ordering::Relaxed);
    if cadence == 0 {
        return None;
    }
    let now = owner_now_ms()?;
    let until = PROD_UNTIL_MS.load(Ordering::Relaxed);
    if now >= until {
        if HELD_OFFSETS.load(Ordering::Relaxed) == 0 {
            // Drained: retire at once. A racing fresh reading re-arms
            // right after — one lost window at worst, never a wrong ask.
            PROD_RENEW_MS.store(0, Ordering::Relaxed);
            return None;
        }
        let plane = PLANE.load();
        let routine = plane
            .as_ref()
            .and_then(|p| p.prod.as_ref().map(|p| p.renew_ms))?;
        let ttl = plane.as_ref().map(|p| p.reading_ttl_ms).unwrap_or(0).max(1);
        // One relax step per window: the CAS on the expiry word elects
        // exactly one decayer; losers re-read whatever won (the decayed
        // value, or a fresh reading's tighter one).
        if PROD_UNTIL_MS
            .compare_exchange(
                until,
                now.saturating_add(ttl),
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            let next = cadence.saturating_mul(2);
            if next >= routine {
                // The ladder reached routine: hand back to the routine
                // machinery (a permanent elevated ask on a quiet plane
                // is the inverted economy).
                PROD_RENEW_MS.store(0, Ordering::Relaxed);
                return None;
            }
            PROD_RENEW_MS.store(next, Ordering::Relaxed);
            PROD_DECAYS.fetch_add(1, Ordering::Relaxed);
            cadence = next;
        } else {
            cadence = PROD_RENEW_MS.load(Ordering::Relaxed);
            if cadence == 0 || now >= PROD_UNTIL_MS.load(Ordering::Relaxed) {
                return None;
            }
        }
    }
    if acked_free_epoch >= PROD_LABEL.load(Ordering::Relaxed) {
        return None;
    }
    PRODS.fetch_add(1, Ordering::Relaxed);
    // Rung a′'s share of the ledger (⊆ prods): the cadence in force came
    // from the demand arm, not the space runway.
    if PROD_FROM_DEMAND.load(Ordering::Relaxed) == 1 {
        DEMAND_PRODS.fetch_add(1, Ordering::Relaxed);
    }
    Some(cadence)
}

/// The live pressure reading in ms (`None` = none, or stale).
fn live_runway_ms() -> Option<u64> {
    let runway = RUNWAY_MS.load(Ordering::Relaxed);
    if runway == u64::MAX {
        return None;
    }
    let now = owner_now_ms()?;
    (now < RUNWAY_UNTIL_MS.load(Ordering::Relaxed)).then_some(runway)
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
///
/// The COUNTER is the authority and is unconditional; the log line is
/// rate-limited to the first stall and every 1,024th after it, because a
/// genuinely full store retries at write-path frequency and a per-refusal
/// error line would bury the one that explains the condition.
pub fn note_alloc_stall(held: usize, held_bytes: u64) {
    let n = ALLOC_STALLS.fetch_add(1, Ordering::Relaxed);
    if n % 1024 != 0 {
        return;
    }
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
    /// The qualify window — [`qualify_lag_ms`]: the writer's checkpoint
    /// landing ceiling + `skew_max` (item 1; `staleness_bound + skew_max`
    /// under the lever's `0`).
    pub qualify_lag_ms: u64,
    /// The drain window's TIMER — [`drain_lag_ms`]: the terms of
    /// `staleness_bound + D_purge` that are still waited out by the clock
    /// (0 with items 2 and 3 both on; `S + D_purge` with both off).
    pub drain_lag_ms: u64,
    /// The acknowledgement-refresh floor (`ack_refresh_floor` — the pass
    /// cadence's own floor): the PIPELINE DEPTH derivation's input
    /// (sustain campaign §5.1 — depth is derived, never a knob).
    pub refresh_floor_ms: u64,
    /// Item 3 — the OBSERVED drain: the reader's purge generation after
    /// this pass's steps ([`crate::ro_coherence::reader_step_generation`]).
    /// A candidate this pass qualifies records it and promotes only once
    /// every serve stamped below it has completed
    /// ([`crate::ro_coherence::serve_drained_below`]). `0` = observation
    /// not in force (the lever off, or the ledger unarmed) — the timer
    /// alone decides.
    pub drain_gen: u64,
    /// `D_purge`, ms — with the observed drain in force it is only the
    /// fail-stop budget the tripwire measures the drain against
    /// (`free_grace_drain_overdue`); it never shortens the wait.
    pub drain_budget_ms: u64,
}

/// One in-flight acknowledgement candidate (sustain campaign §5.1): its
/// own learned-at snapshot (the anti-starvation law, per candidate) and
/// its own qualify/drain state — no label's gates ever move.
#[derive(Debug, Clone, Copy)]
struct AckCandidate {
    label: u64,
    learned_at_ms: u64,
    /// `now` of the pass that qualified it (`0` = unqualified).
    qualified_pass_now_ms: u64,
    /// `qualified_pass_now + drain_lag` (`0` = unset).
    ready_at_ms: u64,
    /// Item 3: the purge generation the qualifying pass left; the
    /// candidate's drain is observed below it (`0` = timer only).
    drain_gen: u64,
    /// The overdue tripwire fired for this candidate (once).
    overdue_noted: bool,
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
///    `checkpoint_ceiling + skew_max` after the label was learned
///    ([`qualify_lag_ms`]; ladder re-derivation item 1, 2026-09-06 —
///    `.benchmarks/2026-09-06-free-grace-ladder-rederivation.md`). The
///    argument, in full: the dereference commit precedes the free (the
///    reclaim queue sits between), and the free's label is the owner's
///    instant at the free, so the dereference was ACKED before the label.
///    The writer's checkpoint machinery lands a commit in the ledger within
///    its LANDING ceiling — the cadence trigger plus the two tick-
///    granularity terms the trigger is evaluated behind
///    (`checkpoint::checkpoint_landing_ceiling_ms`) — so by owner instant
///    `label + ceiling` a ledger record containing the dereference exists,
///    and a pass whose ledger READ begins after that adopts it (or a
///    newer one). The member measures from the instant it LEARNED the
///    label (its send anchor): the label was minted no later than one
///    trip after that anchor, and `skew_max ≥ the observed RTT` covers the
///    trip plus the clocks' rate drift — so a member-clock pass start of
///    `learned + ceiling + skew` is an owner-clock instant ≥ `label +
///    ceiling`. A learn instant LATER than the label's (a slow beat) only
///    pushes the pass later: conservative, never unsafe.
///
///    **Why the poll interval is not in this window** (the term the
///    2026-08 derivation carried as `staleness_bound = P + ceiling`):
///    `P` bounds how stale a reader may be BETWEEN polls — the S5
///    staleness statement. For qualification the pass itself IS the poll:
///    its read is the observation the window exists to place after the
///    landing, so `P` bounded nothing and was counted on top of the
///    ceiling. What DOES remain unstated is device time (the cycle's own
///    writes) — the same residue the published S5 bound carries; the
///    writer-advertised ceiling of adjudication item 4 is where a
///    measured cycle term belongs. A pass that began earlier can adopt a
///    record that still names the freed block, and the reader would
///    re-resolve the binding immediately after purging it.
/// 3. **Drained.** Two things must be true after the qualifying pass's
///    purge before the label is honest: (a) no daemon cache may still
///    serve a PRE-STEP binding `b → K`, and (b) every serve that resolved
///    a binding before the step must have finished (its device read may
///    otherwise land after the writer has reused `K`).
///
///    (a) was a TIMER — `staleness_bound` (`S`), the reader TTL of the
///    daemon layout/attr caches (§6.8 item 4) — and is now an
///    **epoch-step invalidation** (ladder re-derivation item 2): every
///    layout-cache entry is stamped with the reader's purge generation
///    read BEFORE its backend read
///    ([`crate::ro_coherence::reader_step_generation`]), the step's last
///    act bumps the generation (after that volume's root adoption, drop
///    pass and R-6 purge), and every binding-serving read — the handler's
///    `fetch_metadata` gate, the post-fetch binding recheck, the il sync
///    probe, the direct-drive probe, the background fill tasks' map
///    snapshots — refuses an entry stamped below the current generation
///    ([`crate::ro_coherence::layout_entry_pre_step`]). A miss costs one
///    re-resolve per cached ino per step (a step is at most one per
///    volume per pass); the gate itself is one relaxed load and a compare,
///    and adds no lock and no copy to the hot path. The attr/dentry caches
///    keep their TTL: they map names to inos (never reused) and inos to
///    sizes, never a block to an offset, and every binding they lead to
///    is resolved through the gated layout cache. So `S` left the drain
///    (`S + D_purge` verbatim under `SQUEEZEFS_FREE_GRACE_DRAIN_EPOCH_STAMP=0`).
///
///    (b) was §6.7's `D_purge` term — "observe, then finish", the
///    lease-clock fail-stop reserve S6 sizes as two revalidation cadences
///    — reused as a serve drain, and is now **observed** (ladder
///    re-derivation item 3): every read serve — the FUSE handler, the
///    R-2 fast probe, the il sync probe, the direct-drive DMA until its
///    CQE, the R2 prefetch and read-lane fill tasks, `copy_file_range`'s
///    source read — stamps the purge generation it started under and
///    counts itself in that generation's slot
///    ([`crate::ro_coherence::ServeStamp`]); a candidate qualified after
///    the step that left generation `G` promotes once every slot below
///    `G` reads zero ([`crate::ro_coherence::serve_drained_below`]),
///    re-checked on the next pass or on the completion wake of the last
///    such serve. That is strictly safer than any timer — a timer bounds
///    nothing a fabric timeout can stretch, the count is the serves
///    themselves — and it completes in the serve residence (milliseconds)
///    instead of two poll intervals. `D_purge` survives as the tripwire
///    only: an observed drain outliving it counts `free_grace_drain_overdue`
///    (and `invariant_tripwires`) and KEEPS waiting. The ordering proof
///    (same-word pairs; the Dekker pair between the stamp and the step;
///    slot aliasing poisoned, never unsafe) is `ro_coherence`'s serve
///    ledger block. [`drain_lag_ms`] is what the clock still waits out: 0
///    with both items on, `D_purge` under `SQUEEZEFS_FREE_GRACE_DRAIN_OBSERVED=0`.
///
/// A newer label never displaces a pending one until it has been
/// acknowledged, so the ladder cannot starve when the renewal cadence is
/// shorter than the drain window.
///
/// **The candidate, and why it is a word of its own** (rung-20 residual 6):
/// condition (2) is measured against the instant the label was LEARNED,
/// and every renewal re-learns a fresher label. Qualifying against
/// whatever the newest renewal carries therefore starves the ladder
/// outright whenever the renewal cadence is shorter than
/// `staleness + skew_max` — each beat moves the target further away than
/// the passes can walk, no pass ever qualifies, and the reader
/// acknowledges NOTHING while the writer's ring climbs for ever. That is
/// reachable with no valve at all (a writer at
/// `SQUEEZEFS_META_FLUSH_INTERVAL_MS=5000` derives a 6 s qualification lag
/// plus a 5 s pass cadence against a 10 s beat), and rung (a) would make
/// it reachable by design. So the ladder SNAPSHOTS the label it is working
/// on: fresher labels wait their turn, and a faster beat only ever makes
/// the snapshot fresher.
#[derive(Debug, Default)]
pub struct ReaderAckLadder {
    /// The in-flight candidates, learn-ordered (sustain campaign §5.1 —
    /// the bounded pipeline; the shipped single-candidate ladder is the
    /// depth-1 special case the A/B lever restores). `note_pass` runs once
    /// per second on the revalidation task — nowhere near a hot path.
    queue: parking_lot::Mutex<std::collections::VecDeque<AckCandidate>>,
    /// The published word the renewal reader loads.
    acked: AtomicU64,
    /// Gauge: promote-instant `now − learned_at(acked)` — the reader's own
    /// contribution to the loop latency (`free_grace_acked_lag_ms`, §8).
    acked_lag_ms: AtomicU64,
    /// Gauge: candidates in flight after the last pass
    /// (`free_grace_ack_pipeline_depth`, §8).
    depth: AtomicU64,
}

impl ReaderAckLadder {
    /// An empty ladder (nothing qualified, nothing acknowledged).
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one completed revalidation pass. `Some(label)` ⇔ this pass
    /// promoted an acknowledgement, which the caller carries to the plane.
    ///
    /// Every candidate keeps the shipped ladder's three gates VERBATIM —
    /// pipelining changes only how many labels ride concurrently (§5.1's
    /// correctness argument): (1) an epoch-step purge, (2) a pass that
    /// began ≥ `learned_at + qualify_lag`, (3) `drain_lag` elapsed since
    /// that pass. Labels adopt in learn order, so promotion is monotone.
    pub fn note_pass(&self, i: AckInputs) -> Option<u64> {
        // Depth: derived, never a knob — enough slots to keep adopting for
        // one full qualify+drain window at the pass cadence, plus slack
        // (= 9 on the shipped s11-venue derivation). The lever's depth-1
        // arm is the pre-campaign ladder verbatim.
        let cap = if ack_pipeline_enabled() {
            (((i.qualify_lag_ms.saturating_add(i.drain_lag_ms)).div_ceil(i.refresh_floor_ms.max(1))
                as usize)
                .saturating_add(2))
            .clamp(2, 16)
        } else {
            1
        };
        let acked = self.acked.load(Ordering::Acquire);
        let mut q = self.queue.lock();
        // (0) Adopt: a label fresher than the newest candidate joins with
        // its OWN learned-at snapshot (the anti-starvation law per
        // candidate — a faster beat only ever makes the NEWEST candidate
        // fresher, never moves an adopted one's target).
        let newest = q.back().map(|c| c.label).unwrap_or(acked);
        if i.label > newest && i.label > acked {
            if q.len() >= cap && cap > 1 {
                // The high-water rule: drop the oldest UNQUALIFIED
                // candidate in favour of the newest label (acking a
                // fresher label subsumes the older). Qualified candidates
                // are never dropped — anything that reached qualification
                // always promotes, which is the no-wedge half. At depth 1
                // (the lever) nothing is ever displaced: the shipped
                // snapshot law.
                if let Some(pos) = q.iter().position(|c| c.qualified_pass_now_ms == 0) {
                    q.remove(pos);
                }
            }
            if q.len() < cap {
                q.push_back(AckCandidate {
                    label: i.label,
                    learned_at_ms: i.learned_at_ms,
                    qualified_pass_now_ms: 0,
                    ready_at_ms: 0,
                    drain_gen: 0,
                    overdue_noted: false,
                });
            }
        }
        // (1) + (2): an advancing pass qualifies EVERY candidate whose
        // learn instant is old enough — each on its own gate, verbatim.
        if i.advanced {
            for c in q
                .iter_mut()
                .filter(|c| c.qualified_pass_now_ms == 0)
                .filter(|c| i.pass_start_ms >= c.learned_at_ms.saturating_add(i.qualify_lag_ms))
            {
                c.qualified_pass_now_ms = i.now_ms;
                c.ready_at_ms = i.now_ms.saturating_add(i.drain_lag_ms);
                c.drain_gen = i.drain_gen;
            }
        }
        // (3): promote the deepest qualified-and-drained prefix (learn
        // order = label order, and an advancing pass qualifies front-first,
        // so the prefix is the whole promotable set). ONE ack is emitted —
        // the max — because acking it subsumes everything beneath.
        //
        // Item 3 — the OBSERVED half: the wait word is published BEFORE
        // the slots are read (the completion side's Dekker half), then a
        // candidate is drained iff every serve stamped below its
        // generation has completed. The `D_purge` budget is a tripwire on
        // an observed drain: overdue is counted once per candidate and
        // reported, and the candidate keeps waiting — the timer never
        // shortens an observed wait.
        let wait_gen = q
            .iter()
            .filter(|c| c.qualified_pass_now_ms != 0)
            .map(|c| c.drain_gen)
            .max()
            .unwrap_or(0);
        crate::ro_coherence::set_drain_wait_gen(wait_gen);
        let mut promoted: Option<AckCandidate> = None;
        while let Some(front) = q.front_mut() {
            if front.qualified_pass_now_ms == 0 || i.now_ms < front.ready_at_ms {
                break;
            }
            if front.drain_gen != 0 && !crate::ro_coherence::serve_drained_below(front.drain_gen) {
                if !front.overdue_noted
                    && i.now_ms.saturating_sub(front.qualified_pass_now_ms) >= i.drain_budget_ms
                {
                    front.overdue_noted = true;
                    DRAIN_OVERDUE.fetch_add(1, Ordering::Relaxed);
                    crate::note_invariant_tripwire(
                        "free_grace_drain_overdue",
                        &format!(
                            "a read serve that started before purge generation {} is still in \
                             flight {} ms after the qualifying pass — past the D_purge fail-stop \
                             budget of {} ms; the acknowledgement of label {} keeps waiting on it \
                             (a serve this long is a wedged I/O, never load)",
                            front.drain_gen,
                            i.now_ms.saturating_sub(front.qualified_pass_now_ms),
                            i.drain_budget_ms,
                            front.label
                        ),
                    );
                }
                break;
            }
            if front.drain_gen != 0 {
                DRAIN_OBSERVED.fetch_add(1, Ordering::Relaxed);
            }
            promoted = q.pop_front();
        }
        if promoted.is_some() && q.iter().all(|c| c.qualified_pass_now_ms == 0) {
            crate::ro_coherence::set_drain_wait_gen(0);
        }
        self.depth.store(q.len() as u64, Ordering::Relaxed);
        drop(q);
        if let Some(c) = promoted {
            self.acked.store(c.label, Ordering::Release);
            self.acked_lag_ms
                .store(i.now_ms.saturating_sub(c.learned_at_ms), Ordering::Relaxed);
            READER_ACKS.fetch_add(1, Ordering::Relaxed);
            return Some(c.label);
        }
        None
    }

    /// The highest label this ladder has acknowledged.
    pub fn acked(&self) -> u64 {
        self.acked.load(Ordering::Acquire)
    }

    /// The highest label awaiting its drain window (`0` = none pending).
    pub fn pending(&self) -> u64 {
        self.queue
            .lock()
            .iter()
            .rev()
            .find(|c| c.qualified_pass_now_ms != 0)
            .map(|c| c.label)
            .unwrap_or(0)
    }

    /// Candidates in flight (`free_grace_ack_pipeline_depth` — ≤ the
    /// derived cap; ≤ 1 under the depth-1 lever).
    pub fn depth(&self) -> u64 {
        self.depth.load(Ordering::Relaxed)
    }

    /// Promote-instant `now − learned_at(acked)` — the reader's own
    /// contribution to the loop latency (`free_grace_acked_lag_ms`).
    pub fn acked_lag_ms(&self) -> u64 {
        self.acked_lag_ms.load(Ordering::Relaxed)
    }

    /// Test seam: drop every in-flight candidate and zero the words.
    fn reset(&self) {
        self.queue.lock().clear();
        self.acked.store(0, Ordering::Relaxed);
        self.acked_lag_ms.store(0, Ordering::Relaxed);
        self.depth.store(0, Ordering::Relaxed);
    }
}

/// The `SQUEEZEFS_FREE_GRACE_ACK_PIPELINE` lever's latch: 0 = unread,
/// 1 = on, 2 = off. Latched on first use (the knob registry refuses bad
/// values at startup; ENG-10), overridable by the test seam.
static ACK_PIPELINE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn ack_pipeline_enabled() -> bool {
    match ACK_PIPELINE.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = crate::env_knobs::bool_knob("SQUEEZEFS_FREE_GRACE_ACK_PIPELINE", true);
            ACK_PIPELINE.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            on
        }
    }
}

/// Test seam (the contracts drive both lever arms in one process):
/// `Some(on)` presets the latch; `None` returns it to the knob.
pub fn test_set_ack_pipeline(on: Option<bool>) {
    ACK_PIPELINE.store(
        match on {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        },
        Ordering::Relaxed,
    );
}

/// Test seam: `true` ⇔ a preset was in force; the latch returns to the
/// knob either way.
pub fn test_clear_ack_pipeline() -> bool {
    ACK_PIPELINE.swap(0, Ordering::Relaxed) != 0
}

/// The `SQUEEZEFS_FREE_GRACE_DEMAND` lever's latch (the `ACK_PIPELINE`
/// pattern): the demand arm — site 0's consumer, rung a′, the L3 widened
/// refresh, AND the KD-FG-10 runway supply re-base. `0` restores the
/// pre-campaign valve verbatim (the observation counter stays live —
/// the instrument is not the mechanism).
static DEMAND_LEVER: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub(crate) fn demand_enabled() -> bool {
    match DEMAND_LEVER.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = crate::env_knobs::bool_knob("SQUEEZEFS_FREE_GRACE_DEMAND", true);
            DEMAND_LEVER.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            on
        }
    }
}

/// Test seam (the `test_set_ack_pipeline` shape).
pub fn test_set_demand(on: Option<bool>) {
    DEMAND_LEVER.store(
        match on {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        },
        Ordering::Relaxed,
    );
}

/// Test seam: `true` ⇔ a preset was in force.
pub fn test_clear_demand() -> bool {
    DEMAND_LEVER.swap(0, Ordering::Relaxed) != 0
}

/// The `SQUEEZEFS_FREE_GRACE_PASS_ELASTIC` lever's latch (L2b, §5.2b —
/// OQ 3's user decision): a prodded member also tightens its revalidation
/// pass cadence. `0` = the routine pass cadence always (the pre-campaign
/// S5 behavior verbatim).
static PASS_ELASTIC: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn pass_elastic_enabled() -> bool {
    match PASS_ELASTIC.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = crate::env_knobs::bool_knob("SQUEEZEFS_FREE_GRACE_PASS_ELASTIC", true);
            PASS_ELASTIC.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            on
        }
    }
}

/// Test seam (the `test_set_ack_pipeline` shape).
pub fn test_set_pass_elastic(on: Option<bool>) {
    PASS_ELASTIC.store(
        match on {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        },
        Ordering::Relaxed,
    );
}

/// Test seam: `true` ⇔ a preset was in force.
pub fn test_clear_pass_elastic() -> bool {
    PASS_ELASTIC.swap(0, Ordering::Relaxed) != 0
}

/// `true` ⇔ the demand mark is live (owner clock; TTL'd like the runway).
fn demand_live() -> bool {
    let Some(now) = owner_now_ms() else {
        return false;
    };
    now < DEMAND_UNTIL_MS.load(Ordering::Relaxed)
}

/// **L2b, the member side** (§5.2b): a renewal grant adopted with a
/// tightened `renew_ms` IS the pass-cadence ask — deposit it, TTL'd like
/// the prod (two beats of the prodded cadence, so a quiet window restores
/// the routine cadence within one reading TTL) — together with the
/// writer's checkpoint ceiling the SAME grant advertised
/// (`Grant::checkpoint_ceiling_ms`; `0` = none, the routine constant
/// applies): the ask's floor (the composite, adjudication item 4).
pub fn note_prodded_renewal(renew_ms: u64, checkpoint_ceiling_ms: u64, member_now_ms: u64) {
    PASS_PROD_MS.store(renew_ms, Ordering::Relaxed);
    PASS_FLOOR_MS.store(checkpoint_ceiling_ms, Ordering::Relaxed);
    PASS_PROD_UNTIL_MS.store(
        member_now_ms.saturating_add(renew_ms.saturating_mul(2)),
        Ordering::Relaxed,
    );
}

/// **L2b, the pass-cadence resolver** — called by the revalidation loop
/// once per iteration: `clamp(prodded renew_ms, the writer's advertised
/// checkpoint ceiling, routine)` while the deposit is live and the lever
/// is on; the routine cadence otherwise. The floor is physics, not tuning
/// (a pass faster than the writer's checkpoint cadence finds nothing new);
/// its INPUT is live since the composite (user decision 2026-09-06): a
/// grant advertising no ceiling — or the composite lever off — floors at
/// `CHECKPOINT_MAX_AGE_MS`, the shipped law verbatim. On a venue whose
/// routine interval already sits AT the routine floor this is
/// structurally inert until the writer's ceiling drops below it.
pub fn reader_pass_interval(routine: Duration, member_now_ms: u64) -> Duration {
    let routine_ms = routine.as_millis() as u64;
    let out = 'tightened: {
        if !pass_elastic_enabled() {
            break 'tightened routine_ms;
        }
        if member_now_ms >= PASS_PROD_UNTIL_MS.load(Ordering::Relaxed) {
            break 'tightened routine_ms;
        }
        let asked = PASS_PROD_MS.load(Ordering::Relaxed);
        if asked == 0 {
            break 'tightened routine_ms;
        }
        let constant = crate::meta_backend::kv::checkpoint::CHECKPOINT_MAX_AGE_MS as u64;
        let advertised = PASS_FLOOR_MS.load(Ordering::Relaxed);
        let floor = if checkpoint_composite_enabled() && advertised != 0 {
            advertised
        } else {
            constant
        };
        let tightened = asked.max(floor).min(routine_ms);
        if tightened < routine_ms {
            PASS_PRODS.fetch_add(1, Ordering::Relaxed);
        }
        tightened
    };
    PASS_INTERVAL_MS.store(out, Ordering::Relaxed);
    Duration::from_millis(out)
}

/// **The reader-side acknowledgement-refresh floor** — the pipeline-depth
/// derivation's input ([`AckInputs::refresh_floor_ms`]): the member's pass
/// cadence floored at the clock skew, `ProdParams::floor_for`'s law read
/// with the ceiling this member learned. With the composite lever off the
/// ceiling term is absent and this is the shipped `max(P, skew)` verbatim
/// (a slow-flush venue's `P` above the constant must not shrink its depth
/// input on the A/B control); with it on, an advertised P/2 halves the
/// floor — labels arrive at the halved beat, and a depth derived from the
/// routine floor would saturate and displace unqualified candidates.
pub fn reader_refresh_floor_ms(pass_ms: u64, checkpoint_ceiling_ms: u64, skew_ms: u64) -> u64 {
    let ceiling = if checkpoint_composite_enabled() {
        checkpoint_ceiling_ms
    } else {
        u64::MAX
    };
    pass_ms.min(ceiling).max(skew_ms)
}

/// The mount's ladder (one reader per process — the
/// `membership::INSTALLED` shape).
static LADDER: once_cell::sync::Lazy<ReaderAckLadder> =
    once_cell::sync::Lazy::new(ReaderAckLadder::default);

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
    let qualify = qualify_lag_ms(
        qualify_ceiling_enabled(),
        session.checkpoint_ceiling_ms(),
        staleness,
        session.skew_max_ms(),
    );
    QUALIFY_LAG_MS.store(qualify, Ordering::Relaxed);
    // Item 3: the drain is observed only where serves are being counted —
    // the lever on AND the cadence task armed the ledger.
    let observed =
        crate::ro_coherence::drain_observed_enabled() && crate::ro_coherence::serve_ledger_armed();
    let drain = drain_lag_ms(
        crate::ro_coherence::drain_epoch_stamp_enabled(),
        observed,
        staleness,
        session.d_purge_ms(),
    );
    DRAIN_LAG_MS.store(drain, Ordering::Relaxed);
    let out = LADDER.note_pass(AckInputs {
        label,
        learned_at_ms,
        pass_start_ms,
        now_ms: session.now_ms(),
        advanced,
        qualify_lag_ms: qualify,
        drain_lag_ms: drain,
        // The generation AFTER this pass's steps: every serve stamped
        // below it started before the last of them.
        drain_gen: if observed {
            crate::ro_coherence::reader_step_generation()
        } else {
            0
        },
        drain_budget_ms: session.d_purge_ms(),
        // The pass cadence's own floor (`ack_refresh_floor`'s arithmetic,
        // reader-side — `ProdParams::floor_for`'s law with the ceiling
        // this member learned): the pipeline-depth derivation's input.
        // Under the composite labels arrive at the halved beat, so the
        // depth must derive from the halved floor or the queue saturates
        // and displaces unqualified candidates (measured: the hold GREW
        // 800 ms with the routine floor here).
        refresh_floor_ms: reader_refresh_floor_ms(
            crate::ro_coherence::reader_revalidate_interval().as_millis() as u64,
            session.checkpoint_ceiling_ms(),
            session.skew_max_ms(),
        ),
    });
    if let Some(label) = out {
        session.ack_free_epoch(label);
        // Lever (b): the acknowledgement carries itself home NOW — the
        // renewal loop is woken instead of sleeping out the rest of its
        // beat (the T5 carry term becomes one round trip). The renewal
        // re-anchors the beat, so the lease lane sees at most one extra
        // renewal per promotion, and a promotion happens at most once per
        // pass. Renewing early is always safe under §6.7 (T_self bounds
        // NOT renewing).
        if ack_renewal_enabled() {
            crate::membership::request_renewal_now();
            ACK_RENEWALS.fetch_add(1, Ordering::Relaxed);
        }
        log::debug!(
            "freed-offset acknowledgement: this reader has FINISHED with everything freed at or \
             before label {label} (an epoch-step purge ran after the label was learned, and the \
             drain window has elapsed) — it rides the next lease renewal (spec §6.8 item 3)"
        );
    }
    out
}

/// **Gate 2's window — the qualify lag** (ladder re-derivation item 1,
/// 2026-09-06). The pure rule both [`reader_pass_completed`] and the
/// closed-loop model compute: with the `SQUEEZEFS_FREE_GRACE_QUALIFY_CEILING`
/// lever on, the WRITER's checkpoint landing ceiling plus the skew bound;
/// off, the retired `staleness + skew` verbatim (the A/B control).
///
/// Why the poll interval is not in it: the dereference commit precedes the
/// free (the reclaim queue sits between), so it is in the ledger within the
/// writer's landing ceiling of the free's label; a pass BEGINNING that far
/// after the label was learned reads a record that contains it. The
/// staleness bound's `P` term is "how stale can a reader be between polls"
/// — but for qualification the pass IS the poll, so `P` bounded nothing
/// here. `skew_max` (≥ the observed RTT) covers the label being minted up
/// to one trip AFTER the member-clock instant it is measured from.
pub fn qualify_lag_ms(
    ceiling_lever_on: bool,
    checkpoint_ceiling_ms: u64,
    staleness_bound_ms: u64,
    skew_max_ms: u64,
) -> u64 {
    if ceiling_lever_on {
        checkpoint_ceiling_ms.saturating_add(skew_max_ms)
    } else {
        staleness_bound_ms.saturating_add(skew_max_ms)
    }
}

/// The qualify lag in force on this reader, ms (`free_grace_qualify_lag_ms`
/// — published so the operator page cannot drift from the derivation the
/// ladder enforces; 0 until the first pass).
static QUALIFY_LAG_MS: AtomicU64 = AtomicU64::new(0);

/// `free_grace_qualify_lag_ms`.
pub fn qualify_lag_in_force_ms() -> u64 {
    QUALIFY_LAG_MS.load(Ordering::Relaxed)
}

/// **Gate 3's TIMER — the drain lag** (ladder re-derivation items 2 and
/// 3): the terms of the retired `staleness_bound + D_purge` still waited
/// out by the clock. With `SQUEEZEFS_FREE_GRACE_DRAIN_EPOCH_STAMP` on the
/// `S` term is gone (the daemon's layout cache can no longer serve a
/// pre-step binding after the epoch step —
/// [`crate::ro_coherence::layout_entry_pre_step`]); with
/// `SQUEEZEFS_FREE_GRACE_DRAIN_OBSERVED` on (and the ledger armed) the
/// `D_purge` term is gone too — the drain is OBSERVED
/// ([`crate::ro_coherence::serve_drained_below`]) and `D_purge` is only
/// the tripwire's budget. Both off: `S + D_purge` verbatim.
pub fn drain_lag_ms(
    epoch_stamp_lever_on: bool,
    observed_drain_on: bool,
    staleness_bound_ms: u64,
    d_purge_ms: u64,
) -> u64 {
    let s = if epoch_stamp_lever_on {
        0
    } else {
        staleness_bound_ms
    };
    let d = if observed_drain_on { 0 } else { d_purge_ms };
    s.saturating_add(d)
}

/// The drain lag in force on this reader, ms (`free_grace_drain_lag_ms`).
static DRAIN_LAG_MS: AtomicU64 = AtomicU64::new(0);

/// `free_grace_drain_lag_ms`.
pub fn drain_lag_in_force_ms() -> u64 {
    DRAIN_LAG_MS.load(Ordering::Relaxed)
}

/// Item 3's engagement: promotions whose drain was decided by OBSERVATION
/// — the pre-step in-flight count reaching zero (`free_grace_drain_observed`;
/// ≈ `free_grace_reader_acks` on an armed reader, 0 under the lever's `0`).
static DRAIN_OBSERVED: AtomicU64 = AtomicU64::new(0);

/// Item 3's tripwire: candidates whose observed drain outlived the
/// `D_purge` fail-stop budget (`free_grace_drain_overdue`, must-stay-0 —
/// a serve that long is a wedged I/O; the wait is NOT shortened).
static DRAIN_OVERDUE: AtomicU64 = AtomicU64::new(0);

/// `free_grace_drain_observed`.
pub fn drain_observed() -> u64 {
    DRAIN_OBSERVED.load(Ordering::Relaxed)
}

/// `free_grace_drain_overdue`.
pub fn drain_overdue() -> u64 {
    DRAIN_OVERDUE.load(Ordering::Relaxed)
}

/// The `SQUEEZEFS_FREE_GRACE_QUALIFY_CEILING` lever's latch (the
/// `ACK_PIPELINE` pattern): item 1, qualify = the writer's checkpoint
/// ceiling + skew. `0` = `staleness + skew` (the pre-change window).
static QUALIFY_CEILING: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn qualify_ceiling_enabled() -> bool {
    match QUALIFY_CEILING.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = crate::env_knobs::bool_knob("SQUEEZEFS_FREE_GRACE_QUALIFY_CEILING", true);
            QUALIFY_CEILING.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            on
        }
    }
}

/// Test seam (the `test_set_ack_pipeline` shape).
pub fn test_set_qualify_ceiling(on: Option<bool>) {
    QUALIFY_CEILING.store(
        match on {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        },
        Ordering::Relaxed,
    );
}

/// Test seam: `true` ⇔ a preset was in force.
pub fn test_clear_qualify_ceiling() -> bool {
    QUALIFY_CEILING.swap(0, Ordering::Relaxed) != 0
}

/// The `SQUEEZEFS_FREE_GRACE_ACK_RENEWAL` lever's latch (the
/// `ACK_PIPELINE` pattern): lever (b), a promotion wakes the renewal loop.
static ACK_RENEWAL: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn ack_renewal_enabled() -> bool {
    match ACK_RENEWAL.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = crate::env_knobs::bool_knob("SQUEEZEFS_FREE_GRACE_ACK_RENEWAL", true);
            ACK_RENEWAL.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            on
        }
    }
}

/// Test seam (the `test_set_ack_pipeline` shape).
pub fn test_set_ack_renewal(on: Option<bool>) {
    ACK_RENEWAL.store(
        match on {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        },
        Ordering::Relaxed,
    );
}

/// Test seam: `true` ⇔ a preset was in force.
pub fn test_clear_ack_renewal() -> bool {
    ACK_RENEWAL.swap(0, Ordering::Relaxed) != 0
}

/// Lever (b)'s engagement (`free_grace_ack_renewals`, member-side).
pub fn ack_renewals() -> u64 {
    ACK_RENEWALS.load(Ordering::Relaxed)
}

/// The `SQUEEZEFS_FREE_GRACE_REFRESH_ON_ACK` lever's latch: lever (d), a
/// binding member's advancing acknowledgement marks the bound dirty and
/// the next harvest recomputes it.
static REFRESH_ON_ACK: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn refresh_on_ack_enabled() -> bool {
    match REFRESH_ON_ACK.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = crate::env_knobs::bool_knob("SQUEEZEFS_FREE_GRACE_REFRESH_ON_ACK", true);
            REFRESH_ON_ACK.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            on
        }
    }
}

/// Test seam (the `test_set_ack_pipeline` shape).
pub fn test_set_refresh_on_ack(on: Option<bool>) {
    REFRESH_ON_ACK.store(
        match on {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        },
        Ordering::Relaxed,
    );
}

/// Test seam: `true` ⇔ a preset was in force.
pub fn test_clear_refresh_on_ack() -> bool {
    REFRESH_ON_ACK.swap(0, Ordering::Relaxed) != 0
}

/// Lever (d)'s engagement (`free_grace_bound_refreshes_on_ack`, ⊆
/// `free_grace_bound_refreshes`).
pub fn bound_refreshes_on_ack() -> u64 {
    BOUND_REFRESHES_ON_ACK.load(Ordering::Relaxed)
}

/// The measured O(members) scan cost, ms (`free_grace_bound_scan_ms`; 0
/// until the first recompute).
pub fn bound_scan_ms() -> u64 {
    BOUND_SCAN_EWMA_NS.load(Ordering::Relaxed) / 1_000_000
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
/// Finding 29: bounded-allocation park engagements — each is one slice a
/// write-path allocation waited on grace-held space instead of taking
/// the first refusal as a terminal verdict (the pre-fix shape: fsync EIO
/// with the fence gauges at 0 and reclaimable space in the ring).
pub fn pressure_parks() -> u64 {
    PRESSURE_PARKS.load(Ordering::Relaxed)
}

pub(crate) fn note_pressure_park() {
    PRESSURE_PARKS.fetch_add(1, Ordering::Relaxed);
}

/// The park slice (finding 29): how long one bounded-allocation retry
/// sleeps before re-running the pressure harvest. Derived from the
/// pressure bound (the deadline the park is waiting out), clamped to
/// [10 ms, 250 ms] — responsive at small bounds, never a busy-spin at
/// large ones. Not a knob.
pub fn pressure_park_slice_ms() -> u64 {
    let b = pressure_bound_ms();
    if b == 0 {
        return 50;
    }
    (b / 16).clamp(10, 250)
}

/// The park's WALL backstop (finding 29): a frozen owner clock cannot
/// fence, so the park ends by wall time — twice the ROUTINE fence bound
/// (the most patient DURATION the plane publishes,
/// `free_grace_fence_bound_base_ms`), floored at one second. Past it the
/// refusal stands, loud: the plane is broken, not slow.
///
/// A duration, never [`bound`]: that is the reallocation LABEL — an
/// owner-clock instant whose "nothing owed" sentinel is `u64::MAX` on
/// every mount that is not an owner with members. Read as a wait it made
/// the park unbounded on every co-writer and reader (the 2026-09-05 s11
/// fleet wedge, `.benchmarks/2026-09-06-cowriter-enospc-wedge.md`).
pub fn pressure_park_wall_ms() -> u64 {
    fence_bound_base_ms().saturating_mul(2).max(1_000)
}

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

/// **Rung (a)**: grants that carried a tightened renewal cadence to a
/// member the writer was waiting on. 0 on a quiet writer; growth under a
/// storm is the ladder working. Growth with `free_grace_bound` flat means
/// the ask is being delivered and not answered — expect rung (c) next.
pub fn prods() -> u64 {
    PRODS.load(Ordering::Relaxed)
}

/// **Rung (b)**: harvests that evaluated a deadline below the routine
/// bound. 0 on a quiet writer; the ratio against
/// `free_grace_forced_releases` is the point of the whole valve —
/// tightenings are supposed to be many and forced releases none.
pub fn bound_tightenings() -> u64 {
    BOUND_TIGHTENINGS.load(Ordering::Relaxed)
}

/// Sustain-campaign site 0 (KD-FG-9): harvest passes that observed recycle
/// coupling — the ring aging past the physics floor while the caller's
/// lane-reachable supply sat at the trough. 0 on solo/unarmed mounts;
/// growth is the *coupled* statement the s11 row's gauges lacked.
pub fn demand_waits() -> u64 {
    DEMAND_WAITS.load(Ordering::Relaxed)
}

/// The graded coupling face (`free_grace_demand_pct`, §5.4) — read beside
/// the scarcity face `free_grace_pressure_pct`: the s11 failure shape
/// (decay at scarcity 0) must read ≈ 100 HERE. 0 when the mark expired or
/// the `DEMAND` lever is off.
pub fn demand_pct() -> u64 {
    if !demand_live() {
        return 0;
    }
    DEMAND_AGE_PCT.load(Ordering::Relaxed).min(100)
}

/// Rung a′'s engagement (`free_grace_demand_prods`, ⊆ `free_grace_prods`):
/// growth with `bound` advancing is the arm working; growth with `bound`
/// flat = expect rung (c), the same reading as rung (a).
pub fn demand_prods() -> u64 {
    DEMAND_PRODS.load(Ordering::Relaxed)
}

/// L3's engagement (`free_grace_bound_refreshes`): bound recomputes run on
/// the demand/prod harvest path. Law: ≤ elapsed ÷ the floor rate limit —
/// a breach is a bug.
pub fn bound_refreshes() -> u64 {
    BOUND_REFRESHES.load(Ordering::Relaxed)
}

/// L2b's engagement (`free_grace_pass_prods`): revalidation passes run on
/// a tightened cadence. 0 on quiet fleets, under `PASS_ELASTIC=0`, and —
/// structurally — on venues whose routine interval already sits at the
/// checkpoint floor (the s11 venue).
pub fn pass_prods() -> u64 {
    PASS_PRODS.load(Ordering::Relaxed)
}

/// The revalidation pass cadence in force, ms (`free_grace_pass_interval_ms`
/// — routine when no prod is live; 0 until the first resolve).
pub fn pass_interval_ms() -> u64 {
    PASS_INTERVAL_MS.load(Ordering::Relaxed)
}

/// **The loop-latency instrument** (`free_grace_bound_age_ms`, §8):
/// owner-clock `now − BOUND` while armed and holding — how far behind the
/// clock the published reallocation bound is running. 0 with no plane, 0
/// when nothing is held (the ring drained), 0 when nothing is owed
/// (`BOUND == u64::MAX`). Post-campaign target on the s11 venue: ≤ 12 s
/// sustained under storm (was ≈ 28.6 s measured).
pub fn bound_age_ms() -> u64 {
    if HELD_OFFSETS.load(Ordering::Relaxed) == 0 {
        return 0;
    }
    let bound = bound();
    let Some(now) = owner_now_ms() else {
        return 0;
    };
    if bound == u64::MAX {
        return 0;
    }
    now.saturating_sub(bound)
}

/// Residence samples recorded (the `free_grace_residence_ms` histogram's
/// total — one per released offset).
pub fn residence_samples() -> u64 {
    RESIDENCE_SAMPLES.load(Ordering::Relaxed)
}

/// The graded pressure reading, 0 (quiet) … 100 (the supply is gone at the
/// measured deferral rate) — `100 − runway ÷ the routine bound`, so it is
/// the same comparison rung (b) makes, expressed for a human.
pub fn pressure_pct() -> u64 {
    let fence = fence_bound_base_ms();
    match (live_runway_ms(), fence) {
        (Some(runway), f) if f > 0 => 100 - (runway.min(f) * 100 / f),
        _ => 0,
    }
}

/// The fence deadline IN FORCE (the `write_pipeline_depth_target` /
/// `..._base` precedent: the unadorned name is what the machinery is
/// enforcing, `fence_bound_base_ms` is the un-tightened derivation).
pub fn effective_bound_ms() -> u64 {
    let plane = PLANE.load();
    let Some(plane) = plane.as_ref() else {
        return 0;
    };
    if !plane.valve {
        return plane.fence_ms;
    }
    effective_bound_ms_from(live_runway_ms(), plane.fence_ms, plane.pressure_fence_ms)
}

/// Finding 18's engagement gauge: decay steps taken by an expired ask
/// (see `PROD_DECAYS`).
pub fn prod_decays() -> u64 {
    PROD_DECAYS.load(Ordering::Relaxed)
}

/// The tightened renewal cadence being handed to laggards right now, ms
/// (`0` = none in force).
pub fn prod_renew_ms() -> u64 {
    let cadence = PROD_RENEW_MS.load(Ordering::Relaxed);
    match owner_now_ms() {
        Some(now) if now < PROD_UNTIL_MS.load(Ordering::Relaxed) => cadence,
        _ => 0,
    }
}

/// The item-3 block of the stats inode (merged by `fuse_client`, the
/// `membership::stats_snapshot` precedent): an unarmed mount exports the
/// posture word alone rather than a block of zeroes that would read like a
/// broken plane.
pub fn stats_snapshot() -> serde_json::Value {
    if PLANE.load().is_none() {
        // A READER holds no plane and no ring — its item-3 face is what it
        // has acknowledged. Exported under its own posture word so an
        // operator can tell "this mount is answering for the writer's free
        // list" from "no plane here at all", which are opposite
        // conditions that would otherwise both read `off`.
        if let Some(session) = crate::membership::installed_member() {
            let (label, _) = session.learned_label();
            return serde_json::json!({
                "free_grace_mode": "reader",
                "free_grace_reader_acks": reader_acks(),
                "free_grace_acked_label": session.acked_free_epoch(),
                "free_grace_learned_label": label,
                "free_grace_reader_pending_label": LADDER.pending(),
                // PR 2 (L1): the pipeline's reader gauges — candidates in
                // flight (≤ the derived cap; ≤ 1 under the depth-1 lever)
                // and the reader's own promote lag.
                "free_grace_ack_pipeline_depth": LADDER.depth(),
                "free_grace_acked_lag_ms": LADDER.acked_lag_ms(),
                // L2b (PR 3): the elastic pass cadence's engagement and
                // the cadence in force.
                "free_grace_pass_prods": pass_prods(),
                "free_grace_pass_interval_ms": pass_interval_ms(),
                // Lever (b): promotions that carried themselves home.
                "free_grace_ack_renewals": ack_renewals(),
                // The ladder re-derivation (2026-09-06): the qualify window
                // in force — the writer's checkpoint ceiling + skew, or the
                // retired staleness + skew under the lever's `0`.
                "free_grace_qualify_lag_ms": qualify_lag_in_force_ms(),
                "free_grace_drain_lag_ms": drain_lag_in_force_ms(),
                // Item 2's cache-gate faces: the purge generation and the
                // pre-step entries the gate refused (each one re-resolve).
                "reader_layout_step_gen": crate::ro_coherence::reader_layout_step_gen(),
                "reader_layout_step_misses": crate::ro_coherence::reader_layout_step_misses(),
                // Item 3's observed-drain faces: promotions decided by
                // observation, the D_purge tripwire (must stay 0), the
                // completion wakes, the serves in flight right now, the
                // Dekker recheck's engagement and the slot-overrun tripwire
                // (must stay 0).
                "free_grace_drain_observed": drain_observed(),
                "free_grace_drain_overdue": drain_overdue(),
                "free_grace_drain_wakes": crate::ro_coherence::drain_wakes(),
                "reader_serves_inflight": crate::ro_coherence::serves_inflight(),
                "reader_serve_step_races": crate::ro_coherence::serve_step_races(),
                "reader_serve_slot_overruns": crate::ro_coherence::serve_slot_overruns(),
                // The composite: the writer's LANDING ceiling this member
                // learned with its label (the qualify term's and the pass
                // floor's input) — named apart from the writer's own
                // `free_grace_checkpoint_ceiling_ms`, which is the DECISION
                // ceiling its task enforces (the landing one is that plus
                // two ticks).
                "free_grace_advertised_ceiling_ms": session.checkpoint_ceiling_ms(),
            });
        }
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
        // The bound IN FORCE, and the un-tightened derivation beside it:
        // a tightening only the machinery knows about is a promise the
        // operator cannot read (rung-20 residual 6).
        "free_grace_fence_bound_ms": effective_bound_ms(),
        "free_grace_fence_bound_base_ms": fence_bound_base_ms(),
        "free_grace_pressure_bound_ms": pressure_bound_ms(),
        "free_grace_ring_cap": derived_ring_cap(),
        "free_grace_pressure_pct": pressure_pct(),
        "free_grace_prods": prods(),
        "free_grace_pressure_parks": pressure_parks(),
        "free_grace_prod_decays": prod_decays(),
        "free_grace_bound_tightenings": bound_tightenings(),
        "free_grace_prod_renew_ms": prod_renew_ms(),
        // The sustain campaign's attribution instruments (PR 1, §8).
        "free_grace_bound_age_ms": bound_age_ms(),
        "free_grace_demand_waits": demand_waits(),
        "free_grace_residence_ms": RESIDENCE_MS.to_json(),
        // The demand arm (PR 3, §5.2–§5.4): the coupling face beside the
        // scarcity face, rung a′'s share of the prod ledger, and L3's
        // refresh engagement.
        "free_grace_demand_pct": demand_pct(),
        "free_grace_demand_prods": demand_prods(),
        "free_grace_bound_refreshes": bound_refreshes(),
        // The hold-time campaign's instruments: the per-stage
        // decomposition of the residence, the checkpoint marks it is read
        // against (with the measured cycle cost), the live hold, and the
        // per-member acknowledgement lag (the min-composition's culprit
        // finder; the census itself names peers, so it rides the key-
        // census gate like `dlm_custody_grant_census`).
        "free_grace_hold_phase_ns": hold_phase_json(),
        "free_grace_hold_unplaced": hold_unplaced(),
        "free_grace_hold_ms": hold_ms(),
        "free_grace_checkpoint_marks": checkpoint_marks(),
        "free_grace_checkpoint_cycle_ms": checkpoint_cycle_ms(),
        // The composite (adjudication item 4): the checkpoint ceiling in
        // force (the routine writer ceiling with no ask; P/2 under one)
        // and the cycles run under the elastic one.
        "free_grace_checkpoint_ceiling_ms": checkpoint_ceiling_ms(),
        "free_grace_checkpoint_elastic_cycles": checkpoint_elastic_cycles(),
        "free_grace_member_ack_lag_ms": member_ack_lag_json(),
        "free_grace_member_ack_lag_census": member_ack_lag_census_json(),
        // Lever (d)'s engagement and the scan cost its rate limit floors on.
        "free_grace_bound_refreshes_on_ack": bound_refreshes_on_ack(),
        "free_grace_bound_scan_ms": bound_scan_ms(),
        // The lane-push lever's authority half (finding 15 term 2):
        // binding acks that harvested the rings on arrival, and grants
        // that carried a lane-supply hint.
        "free_grace_lane_push_releases": lane_push_releases(),
        "free_grace_lane_push_hints": lane_push_hints(),
    })
}

/// The lane-visible block of the stats inode — merged by `fuse_client`
/// under the `alloc_lane_*` family's engagement gate (a partition or the
/// grace plane engaged), because its samples exist on BOTH postures: the
/// authority stamps `released_served`, the co-writer all three stages plus
/// the wakes the hint produced.
pub fn lane_visible_stats() -> serde_json::Value {
    serde_json::json!({
        "alloc_lane_visible_phase_ns": lane_visible_phase_json(),
        "alloc_lane_visible_unplaced": lane_visible_unplaced(),
        "alloc_lane_release_marks": lane_release_marks(),
        "free_grace_lane_push_wakes": lane_push_wakes(),
        "free_grace_lane_supply_hint": lane_supply_hint(),
    })
}

/// `free_grace_member_ack_lag_ms`: max / mean / min of the owner's
/// per-member acknowledgement lag ([`crate::membership::MembershipOwner::member_ack_lags`])
/// plus the member count; all 0 with no owner or no members.
fn member_ack_lag_json() -> serde_json::Value {
    let lags: Vec<u64> = crate::membership::installed_owner()
        .map(|o| {
            o.member_ack_lags()
                .into_iter()
                .map(|(_, lag)| lag)
                .collect()
        })
        .unwrap_or_default();
    let n = lags.len() as u64;
    serde_json::json!({
        "max": lags.iter().copied().max().unwrap_or(0),
        "mean": lags.iter().sum::<u64>().checked_div(n).unwrap_or(0),
        "min": lags.iter().copied().min().unwrap_or(0),
        "members": n,
    })
}

/// The per-member census (`[{id, lag_ms}]`), opt-in behind
/// `SQUEEZEFS_STATS_KEY_CENSUS=1` (VAL-7a — it names peers); `null`
/// otherwise, so tooling can tell "gated" from "empty".
fn member_ack_lag_census_json() -> serde_json::Value {
    if !crate::fuse_client::stats_key_census_enabled() {
        return serde_json::Value::Null;
    }
    let rows: Vec<serde_json::Value> = crate::membership::installed_owner()
        .map(|o| {
            o.member_ack_lags()
                .into_iter()
                .map(|(id, lag)| serde_json::json!({ "id": id, "lag_ms": lag }))
                .collect()
        })
        .unwrap_or_default();
    serde_json::Value::Array(rows)
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
        &PRODS,
        &BOUND_TIGHTENINGS,
        &RUNWAY_UNTIL_MS,
        &PROD_RENEW_MS,
        &PROD_DECAYS,
        &PROD_UNTIL_MS,
        &PROD_LABEL,
        &LAST_BOUND_REFRESH_MS,
        &DEMAND_WAITS,
        &RESIDENCE_SAMPLES,
        &DEMAND_UNTIL_MS,
        &DEMAND_AGE_PCT,
        &DEMAND_PRODS,
        &PROD_FROM_DEMAND,
        &BOUND_REFRESHES,
        &PASS_PROD_MS,
        &PASS_PROD_UNTIL_MS,
        &PASS_PRODS,
        &PASS_INTERVAL_MS,
        &CHECKPOINT_MARK_COUNT,
        &CHECKPOINT_CYCLE_EWMA_NS,
        &HOLD_UNPLACED,
        &HOLD_EWMA_MS,
        &ACK_RENEWALS,
        &BOUND_REFRESHES_ON_ACK,
        &BOUND_SCAN_EWMA_NS,
        &LANE_VISIBLE_UNPLACED,
        &LANE_PUSH_RELEASES,
        &LANE_PUSH_HINTS,
        &LANE_SUPPLY_HINT,
        &LANE_PUSH_WAKES,
        &QUALIFY_LAG_MS,
        &DRAIN_LAG_MS,
        &DRAIN_OBSERVED,
        &DRAIN_OVERDUE,
        &CHECKPOINT_ELASTIC_CYCLES,
        &PROMISED_CEILING_MS,
        &PROMISED_UNTIL_MS,
        &PASS_FLOOR_MS,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    BOUND_DIRTY.store(false, Ordering::Relaxed);
    test_set_ack_renewal(None);
    test_set_refresh_on_ack(None);
    test_set_qualify_ceiling(None);
    crate::ro_coherence::reset_for_test();
    test_set_demand(None);
    test_set_pass_elastic(None);
    test_set_lane_push(None);
    test_set_checkpoint_composite(None);
    uninstall_release_hook();
    uninstall_lane_supply_source();
    RESIDENCE_MS.reset();
    for h in HOLD_PHASES.iter() {
        h.reset();
    }
    for h in LANE_VISIBLE_PHASES.iter() {
        h.reset();
    }
    LANE_RELEASE_MARKS.clear_sync();
    CHECKPOINT_MARKS.lock().clear();
    BOUND_ADVANCES.lock().clear();
    RUNWAY_MS.store(u64::MAX, Ordering::Relaxed);
    LADDER.reset();
    test_set_ack_pipeline(None);
}
