//! DLM **S11**'s byte-range custody core — the `FileCustody` interval
//! algebra plus the rung-15 **required/desired admit with admit-time
//! coalescing** (spec §6.9's S11 row; KD-MW-7 and §9.2/§9.4 of
//! `docs/design-full-multi-writer.md`; PR-plan row 15).
//!
//! Extracted dependency-free so `loom-models/` can `#[path]`-include it
//! and model-check the exact shipped transitions (the `grant_table_core`
//! pattern — KD-MW-10's fourth named core). What lives here is the
//! PROTOCOL the models exercise (`range_custody_models` in
//! `loom-models/src/lib.rs`):
//!
//! * **concurrent disjoint admits never serialize and never double-grant
//!   an overlapping span** — the conflict/plan/admit sequence under one
//!   entry lock;
//! * **release-wake vs acquire race admits exactly one overlapping
//!   waiter** — retire-then-signal against register-then-probe;
//! * **whole-file acquire vs in-flight range admit: one wins, never
//!   both**;
//! * **the file-generator publication order** — the stripe floor is
//!   raised BEFORE a grant becomes visible, so a reader can never
//!   observe (or fall back past) a token the floor does not cover
//!   (contract 5's no-regression law).
//!
//! The lock table (`scc` map), the waiter stripes, the mint, the R5
//! byte-budget ceiling and the geometry cap POLICY stay in `src/dlm.rs`
//! (the charter puts the caps there); this module is the mechanism:
//! given one file's custody and one ask, decide `Covered` / `Extend` /
//! `New` / `Held`, and mutate the interval list. The main build never
//! sets `cfg(loom)`.

#[cfg(loom)]
pub(crate) mod sync {
    pub use loom::sync::atomic::{AtomicU64, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod sync {
    pub use std::sync::atomic::{AtomicU64, Ordering};
}

use sync::{AtomicU64, Ordering};

/// Lock mode (spec §6.7 "Lock modes": *"Four modes plus capability bits
/// (NL / CR / CW / EX with LOOKUP, UPDATE, PERM, LAYOUT, XATTR, DATA
/// bits) covers every shape in this filesystem … CW should ship disabled
/// until a verb issues it, per the no-dead-code rule"*).
///
/// **Shipped subset, and why it is a subset.** The no-dead-code law
/// admits a public surface the stage above needs; it does not admit modes
/// with no issuer, no observable semantic and no test:
///
/// | Mode | State | Issuer |
/// |------|-------|--------|
/// | **EX** — exclusive | shipped, default | every production acquire (`acquire_lock`) |
/// | **CW** — concurrent write | **shipped DISABLED** (`test_arm_cw_mode`) | none yet — S9/S11 issue it; §6.7 requires the mode to exist and to be unreachable until then |
/// | *CR* — concurrent read | absent | nothing takes a READ lease (§6.2 census: readers take no leases at all). It lands with **S5** read-only coherent mounts, which is the verb that gives it meaning |
/// | *NL* — null | absent | NL exists to park a resource handle across a *conversion*; this manager has no conversion verb and no client-side handle to park, so an NL variant would be unconstructible-and-unobservable. It lands with **S9**'s conversion protocol |
/// | capability bits | absent | LOOKUP/UPDATE/PERM/XATTR partition *metadata* locks, and metadata ops take no cluster lease today (§6.2). They land with **S8** function-shipped metadata, whose verbs are the issuers |
///
/// The compatibility matrix over the shipped modes:
///
/// ```text
///        EX   CW
///   EX    N    N
///   CW    N    Y
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LockMode {
    /// EX — exclusive custody of the span: compatible with nothing.
    Exclusive,
    /// CW — concurrent write: two CW holders may cover the same span and
    /// coordinate at a finer grain themselves (the classic DLM semantic).
    /// **Ships disabled** — `LocalLockManager::acquire_lock_mode`
    /// refuses it until `test_arm_cw_mode` arms it.
    ConcurrentWrite,
}

impl LockMode {
    /// The §6.7 compatibility matrix: `true` ⇔ two grants in these modes
    /// may cover overlapping bytes at the same time.
    pub const fn compatible_with(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::ConcurrentWrite, Self::ConcurrentWrite)
        )
    }
}

/// One live grant in a file's custody table: the span it protects, its
/// owner, its mode, and the fencing token minted at grant. The token is
/// globally unique (S1's single mint), which makes it the grant's exact
/// removal identity — two grants of the same span by the same client
/// (possible only under compatible modes) can never be confused.
#[derive(Clone, Copy, Debug)]
pub struct Grant {
    pub start: u64,
    pub end: u64,
    pub owner_nonce: u64,
    pub token: u64,
    pub mode: LockMode,
    /// §9.3a (residual item 7's fix): the UNION of every REQUIRED span
    /// admitted or extended into this grant — the never-trimmed floors
    /// the holder actually asked for, as opposed to the desired-minted
    /// STRETCH the span may carry beyond them. The authority knows these
    /// exactly (every admit/extend names its required; a Covered serve
    /// unions too), so `[required-hull end, end)` is the tail the
    /// tail-shrink arm may reclaim without fabricating a demotion.
    /// Equals `(start, end)` wherever no stretch exists (whole-file
    /// grants, adopted client records, plain moded admits) — which is
    /// what keeps every pre-§9.3a classification byte-identical there.
    pub required: (u64, u64),
}

impl Grant {
    #[inline]
    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.end > start && self.start < end
    }

    #[inline]
    fn covers(&self, start: u64, end: u64) -> bool {
        self.start <= start && self.end >= end
    }

    #[inline]
    fn len(&self) -> u64 {
        self.end - self.start
    }
}

/// Round `span` OUTWARD to `block_size` alignment — start floors, end
/// ceils (the §9.2 rounding doctrine: *"desired rounds UP to 4 MiB block
/// alignment (rounding doctrine: allocations round up)"*). `block_size`
/// of 0 (a defensive caller) returns the span unchanged.
pub fn block_align_out(span: (u64, u64), block_size: u64) -> (u64, u64) {
    if block_size == 0 {
        return span;
    }
    let start = (span.0 / block_size) * block_size;
    let end = span.1.div_ceil(block_size).saturating_mul(block_size);
    (start, end.max(span.1))
}

/// The §9.2 geometry-derived per-file span cap:
/// `max(16, ceil(size / block_size))`.
///
/// The cap is the file's **own geometry** — a cap below the block count
/// would refuse legitimate stripe-per-block custody, *including the
/// block-cyclic MPI decomposition, whose non-adjacent round-robin spans
/// never coalesce and legitimately approach one span per block* (the
/// Issue-19 class: a constant refusing the workload S11 exists for). The
/// floor 16 is transient pre-coalesce headroom for sub-16-block files —
/// enough live asks in flight for the admit-time merge to converge them.
/// There is deliberately no ceiling beyond geometry: the real ceiling is
/// the `dlm_grant_table_bytes` R5 byte budget, enforced in `src/dlm.rs`.
pub fn range_span_cap(size: u64, block_size: u64) -> u64 {
    if block_size == 0 {
        return 16;
    }
    size.div_ceil(block_size).max(16)
}

/// One `FileCustody::plan_range` decision — computed and applied under
/// the SAME entry lock, so the plan can never go stale before its apply.
#[derive(Debug, PartialEq, Eq)]
pub enum RangePlan {
    /// A live same-scope grant already covers `required` (an idempotent
    /// re-ask / cache refill): answer its token and span, mutate nothing.
    Covered { token: u64, span: (u64, u64) },
    /// **The admit-time merge** (§9.2 coalescing): a same-scope grant
    /// overlaps or abuts the clipped window — widen THAT grant to `span`
    /// (its union with the window), keep its token, mint nothing. This is
    /// what makes the adversarial tiny-ranges shape converge to O(1)
    /// spans instead of filling the table.
    Extend { token: u64, span: (u64, u64) },
    /// Admit a NEW grant over `span` (the conflict-free desired-subset —
    /// never smaller than required). The caller enforces the geometry cap
    /// and the byte budget, then mints.
    New { span: (u64, u64) },
    /// Foreign (or incompatible) custody overlaps REQUIRED: the ask must
    /// wait or refuse — §9.2's law forbids trimming required to fit.
    HeldForeign,
    /// `required` overlaps two or more of the SAME scope's grants (the
    /// bridge ask). Merging would absorb a second live grant record —
    /// whose release handle is outstanding — so the shape refuses loud.
    /// A client whose cache answers covering probes never builds it.
    BridgeRefused,
}

/// [`FileCustody::plan_range`]'s answer: the plan plus whether desired
/// was TRIMMED against **foreign** custody (the
/// `range_custody_desired_trims` ledger's predicate — required is
/// structurally never trimmed). Same-scope window clips are deliberately
/// NOT trims (finding 16's gauge half): the client's abutting union
/// names its own span, plan step 3 clips it back and step 4 extends that
/// same grant to the union — the admit-time merge's own mechanics, zero
/// contention. Counting them made the fabrication ledger unreadable
/// (row 2's 8,610 "trims" mixed benign self-clips with the real
/// desire-vs-peer collisions the gauge exists to name).
#[derive(Debug)]
pub struct RangeDecision {
    pub plan: RangePlan,
    pub trimmed: bool,
}

/// One §9.3 **demotion-pending** record (DLM S11 rung 17, KD-MW-8): a
/// second holder's acquire would SHARE A BLOCK with `incumbent_token`'s
/// live grant, so the ask parks and the incumbent owes an ACK (carried
/// on its renewal reply) before any grant can issue over `region`. The
/// record is RAM state on the arbiter's `FileCustody` entry — it dies
/// with the authority (MW-13's law: a demotion in flight when the
/// authority dies restarts from zero on the successor).
/// One [`FileCustody::foreign_block_sharers`] answer: a foreign grant
/// whose block hull shares blocks with the probed span, plus the §9.3a
/// classification input (`region.0 >= required_hull_end` ⇔ the share lies
/// wholly in the grant's desired-minted tail — the shrink arm; anything
/// else is honest custody — the demotion barrier).
#[derive(Clone, Copy, Debug)]
pub struct BlockSharer {
    pub token: u64,
    /// The shared block-aligned region.
    pub region: (u64, u64),
    /// The sharer grant's REQUIRED-union block hull end.
    pub required_hull_end: u64,
    /// The sharer grant's holder identity (finding 31 forensics: the
    /// mark log names both parties' scopes, so a dead-era orphan or a
    /// cross-rank collision is attributable from one log line).
    pub owner_nonce: u64,
}

/// One §9.3a **shrink-pending** record (residual board item 7's fix): a
/// second holder's REQUIRED ask overlaps `incumbent_token`'s live grant
/// ONLY in its desired-minted stretch tail (no byte of the ask's block
/// hull touches the grant's required-union hull), so instead of the
/// demotion barrier the grant is marked for a TAIL SHRINK back to
/// `floor`: the asker parks on the same wait machinery, the notice rides
/// the incumbent's next custody-channel reply (acquire/release/renewal —
/// widened from renewal-only by finding 16 half (a)), and the incumbent
/// answers its own written high-water inside the contested tail. RAM
/// state, like [`DemotionPending`] — it dies with the authority.
#[derive(Clone, Copy, Debug)]
pub struct ShrinkPending {
    /// The addressee grant's token (the reply-carried notice's key).
    pub incumbent_token: u64,
    /// The block-hulled boundary that frees every parked ask: the grant's
    /// tail shrinks back to it iff the incumbent's written high-water is
    /// at or below it. Multiple askers merge to the MIN floor.
    pub floor: u64,
    /// The block geometry the barrier classified under — the same hulls
    /// the ack must resolve with.
    pub block: u64,
}

/// `FileCustody::ack_tail_shrink`'s resolution — what the incumbent's
/// written high-water made of the pending shrink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShrinkResolution {
    /// No live shrink pending named the token (a stale or duplicate ack).
    None,
    /// The contested tail was UNWRITTEN: the grant span shrank to `floor`
    /// and the parked ask proceeds to EXCLUSIVE custody — no demotion, no
    /// shared clauses, transient.
    Shrunk { floor: u64 },
    /// The incumbent HAD written into the contested tail: the grant
    /// shrank only to the watermark's block hull (`new_end`) and the ask
    /// still overlaps honest custody — the EXISTING demotion barrier now
    /// arbitrates (the asker's re-plan marks it). Sticky demotion is
    /// thereby reserved for TRUE sharing.
    Demoted { new_end: u64 },
}

#[derive(Clone, Copy, Debug)]
pub struct DemotionPending {
    /// The block-aligned region being demoted.
    pub region: (u64, u64),
    /// The incumbent grant's token (the reply-carried notice's key — the
    /// incumbent acks by naming it; its retire sweeps the pending
    /// through the fence column).
    pub incumbent_token: u64,
    /// `true` once the incumbent acked (quiesced + re-routed): grants
    /// over `region` may now issue — coexistence licensed, the region is
    /// authority-assembled.
    pub acked: bool,
}

/// A file's custody: the whole-file slot plus the live byte-range grants,
/// as a **sorted interval list** (S11 — spec §6.9).
///
/// **Why a sorted `Vec` and not a tree.** Acquisition is per
/// *open-for-write episode*, not per op (`get_or_acquire_lease` caches in
/// `active_leases` — §6.2), so this structure is not on the op hot path;
/// what it must be is cheap at the population it actually sees and free
/// for the whole-file path that ships today. The population bound is
/// **live grants on ONE file** = concurrently held leases on it (entries
/// are retired at release, never accumulated per span ever locked) —
/// one per writing application/rank, so tens, not millions (the §9.2 cap
/// and byte budget in `src/dlm.rs` bound the adversarial worst case). At
/// that size a contiguous 40-byte-element array beats every pointer
/// structure on constants, and it costs ZERO when empty (`Vec::new` does
/// not allocate), which is what keeps the shipped whole-file path
/// unchanged (KD-MW-12).
///
/// Costs:
///
/// | Operation | Cost |
/// |---|---|
/// | whole-file EX acquire / release (the shipped path) | O(1) — two `is_empty` probes |
/// | range acquire (conflict probe / plan) | O(log n + c), `c` = grants in the stab window |
/// | range acquire (admit) | O(n) memmove |
/// | range acquire (extend — the admit-time merge) | O(n) locate + memmove (the merge rides the admit's own cost) |
/// | range release | O(log n) locate + O(n) memmove (+ O(n) only when the widest grant leaves; widened grants fall back to an O(n) scan — see [`Self::retire`]) |
/// | fencing read (`read_identity`) | O(1) — `max_token` |
/// | `span_range_shared` (the W1 clause) | O(log n + c) |
///
/// The stab window: any grant overlapping `[s,e)` has `start < e` and
/// `start + len > s`, and `len ≤ widest`, so the candidates are exactly
/// the grants with `start ∈ (s − widest, e)` — two `partition_point`s.
/// `widest` is a monotone over-approximation on admit/extend (max) and
/// exact on release (recomputed only when the widest grant is the one
/// leaving), so it can never shrink below a live grant's length and can
/// never miss a conflict.
pub struct FileCustody {
    /// Live `range: None` grants — whole-inode custody. A list, not an
    /// `Option`: under a compatible mode (CW) two whole-file grants may
    /// coexist, and silently overwriting one would strand its release.
    /// EX — every shipped acquire — keeps this at most one deep, and the
    /// fast path only ever asks `is_empty()`.
    wholes: Vec<Grant>,
    /// Live byte-range grants, sorted by `(start, token)`.
    ranges: Vec<Grant>,
    /// `max(end − start)` over `ranges` — the stab-window bound.
    widest: u64,
    /// The newest fencing token granted while this entry has been live:
    /// the object's exact generation read (see `dlm::read_identity`).
    max_token: u64,
    /// Rung 17 (§9.3): the **demoted regions** — block-aligned spans
    /// whose data-plane custody transferred to the AUTHORITY (every
    /// holder's writes there ship as extents; `span_is_range_shared`
    /// answers TRUE for any overlap). Empty on every shipped mount —
    /// one `is_empty` probe on the hot paths (KD-MW-12).
    demoted: Vec<(u64, u64)>,
    /// Rung 17 (§9.3): the live demotion-pending records (the
    /// grant-issuance barrier's state). Empty everywhere shipped.
    pending: Vec<DemotionPending>,
    /// §9.3a: the live shrink-pending records (one per incumbent token —
    /// merged to the MIN floor). Empty everywhere shipped.
    shrink_pending: Vec<ShrinkPending>,
}

impl FileCustody {
    /// A fresh, empty custody (the vacant-entry + plan-then-admit shape).
    pub fn empty() -> Self {
        Self {
            wholes: Vec::new(),
            ranges: Vec::new(),
            widest: 0,
            max_token: 0,
            demoted: Vec::new(),
            pending: Vec::new(),
            shrink_pending: Vec::new(),
        }
    }

    /// A fresh entry holding exactly `grant`.
    pub fn opened(span: Option<(u64, u64)>, grant: Grant) -> Self {
        let mut custody = Self::empty();
        custody.admit(span, grant);
        custody
    }

    /// Live byte-range grant records (the `live_range_records` gauge and
    /// the geometry-cap input).
    pub fn ranges_len(&self) -> usize {
        self.ranges.len()
    }

    /// The newest token granted while this entry has been live.
    pub fn max_token(&self) -> u64 {
        self.max_token
    }

    /// The grants that could overlap `[start, end)` — the stab window.
    fn candidates(&self, start: u64, end: u64) -> &[Grant] {
        if self.ranges.is_empty() {
            return &[];
        }
        let lo = self
            .ranges
            .partition_point(|g| g.start.saturating_add(self.widest) <= start);
        let hi = self.ranges.partition_point(|g| g.start < end);
        &self.ranges[lo..hi.max(lo)]
    }

    /// Would a `span`/`mode` request conflict with what is live?
    pub fn conflicts(&self, span: Option<(u64, u64)>, mode: LockMode) -> bool {
        // Whole-inode custody covers every span, so it is checked first
        // whatever the request is. EX (the shipped mode) makes this
        // `!is_empty()`.
        if self.wholes.iter().any(|w| !w.mode.compatible_with(mode)) {
            return true;
        }
        match span {
            // A whole-file request covers every span on the file, so any
            // incompatible live range conflicts. EX — every shipped
            // acquire — short-circuits to `is_empty()`: O(1), the
            // pre-S11 cost.
            None => match mode {
                LockMode::Exclusive => !self.ranges.is_empty(),
                _ => self.ranges.iter().any(|g| !g.mode.compatible_with(mode)),
            },
            Some((start, end)) => self
                .candidates(start, end)
                .iter()
                .any(|g| g.overlaps(start, end) && !g.mode.compatible_with(mode)),
        }
    }

    /// Record a granted lock. Callers must have cleared [`Self::conflicts`]
    /// (or hold a [`RangePlan::New`] from the same critical section).
    pub fn admit(&mut self, span: Option<(u64, u64)>, grant: Grant) {
        self.max_token = self.max_token.max(grant.token);
        match span {
            None => self.wholes.push(grant),
            Some(_) => {
                let at = self
                    .ranges
                    .partition_point(|g| (g.start, g.token) < (grant.start, grant.token));
                self.widest = self.widest.max(grant.len());
                self.ranges.insert(at, grant);
            }
        }
    }

    /// **The rung-15 required/desired plan** (§9.2, EX-only by KD-MW-9's
    /// v1 issuance law): decide how `[required)`/`[desired)` from
    /// `scope` lands on this custody. Callers hold the entry lock across
    /// plan AND apply — the decision can never go stale.
    ///
    /// `scope` is the MERGE identity — grants sharing it belong to one
    /// holder (one client lease on the wire; one manager nonce locally)
    /// and may coalesce; everything else is a wall. The clipped window is
    /// the largest desired-subset that conflicts with nothing:
    ///
    /// 1. any foreign/incompatible custody overlapping REQUIRED ⇒
    ///    [`RangePlan::HeldForeign`] (never a trim of required);
    /// 2. a same-scope grant covering required ⇒ [`RangePlan::Covered`];
    ///    two+ same-scope grants overlapping required ⇒ the bridge
    ///    refusal;
    /// 3. desired is CLIPPED against every other grant outside required
    ///    (foreign and same-scope alike — a merge never absorbs a second
    ///    record);
    /// 4. a same-scope grant overlapping/abutting the clipped window ⇒
    ///    [`RangePlan::Extend`] to the union (the admit-time merge — the
    ///    left neighbor wins when both abut, the right one having already
    ///    clipped the window so nothing is absorbed);
    /// 5. else ⇒ [`RangePlan::New`] over the window.
    pub fn plan_range(
        &self,
        required: (u64, u64),
        desired: (u64, u64),
        scope: u64,
    ) -> RangeDecision {
        debug_assert!(required.0 < required.1);
        debug_assert!(desired.0 <= required.0 && desired.1 >= required.1);
        let mode = LockMode::Exclusive;

        // 1. Whole-inode custody: a same-scope whole covers everything
        // (the idempotent re-ask under one's own whole-file lease); any
        // other whole conflicts with an EX range.
        if let Some(w) = self.wholes.iter().find(|w| w.owner_nonce == scope) {
            return RangeDecision {
                plan: RangePlan::Covered {
                    token: w.token,
                    span: (w.start, w.end),
                },
                trimmed: false,
            };
        }
        if !self.wholes.is_empty() {
            return RangeDecision {
                plan: RangePlan::HeldForeign,
                trimmed: false,
            };
        }

        // 2. The required window's own custody.
        let mut required_target: Option<&Grant> = None;
        let mut required_scope_overlaps = 0usize;
        for g in self.candidates(required.0, required.1) {
            if !g.overlaps(required.0, required.1) {
                continue;
            }
            if g.owner_nonce != scope || g.mode != mode {
                // Foreign, or a different-mode own grant (a CW self-grant
                // is not mergeable custody for an EX ask): required cannot
                // be granted whole right now.
                //
                // Deliberately NOT `!compatible_with`: the §6.7 matrix
                // governs two DIFFERENT holders, and EX‖EX is incompatible
                // by that matrix — classifying the holder's OWN grant
                // through it made every frontier-crossing chunk (a
                // required window partially overlapping the held span,
                // which every non-block-aligned writeback chunk produces)
                // read as FOREIGN custody: a lone streaming co-writer
                // waited out its whole budget ON ITSELF and died EIO (the
                // s11-range leg's second live finding, 2026-08-17 —
                // repro-ported as
                // `own_grant_partial_overlap_extends_and_covered_reask_serves`).
                return RangeDecision {
                    plan: RangePlan::HeldForeign,
                    trimmed: false,
                };
            }
            required_scope_overlaps += 1;
            if g.covers(required.0, required.1) {
                return RangeDecision {
                    plan: RangePlan::Covered {
                        token: g.token,
                        span: (g.start, g.end),
                    },
                    trimmed: false,
                };
            }
            required_target = Some(g);
        }
        if required_scope_overlaps > 1 {
            // Finding 22: a required covered by the UNION of the holder's
            // own same-mode grants is COVERED custody — every byte is
            // already this holder's, so the answer serves (naming the
            // first overlapping grant; the caller's per-grant watermark
            // marks ride the cache probe's segment law) and mutates
            // NOTHING: both records and their release handles stand. The
            // trim teacher's clamp makes abutting same-holder pairs the
            // steady state at the learned ceiling, and a straddling
            // kernel write's required bridges them — pre-fix the refusal
            // fed the retry ladder an identically-refused span for ever
            // (285 k refusal lines on the attempt-3 row). A required that
            // ESCAPES the union keeps the loud refusal below (merging
            // live records is still never done).
            let mut cursor = required.0;
            let mut first: Option<&Grant> = None;
            for g in self.candidates(required.0, required.1) {
                if g.owner_nonce != scope || g.mode != mode || !g.overlaps(required.0, required.1) {
                    continue;
                }
                if g.start > cursor {
                    break; // a gap inside required — not covered
                }
                if first.is_none() {
                    first = Some(g);
                }
                cursor = cursor.max(g.end);
                if cursor >= required.1 {
                    let g = first.expect("at least one overlapping grant");
                    return RangeDecision {
                        plan: RangePlan::Covered {
                            token: g.token,
                            span: (g.start, g.end),
                        },
                        trimmed: false,
                    };
                }
            }
            return RangeDecision {
                plan: RangePlan::BridgeRefused,
                trimmed: false,
            };
        }

        // 3. Clip desired to its conflict-free window: every grant other
        // than the merge target is a wall (same-scope walls too — the
        // merge never absorbs a second live record). The stab window over
        // DESIRED sees every candidate.
        let target_token = required_target.map(|g| g.token);
        let mut window = desired;
        // The trim gauge counts FOREIGN clips only (finding 16): a
        // same-scope wall is the merge's own bookkeeping, never
        // contention evidence.
        let mut foreign_trimmed = false;
        for g in self.candidates(desired.0, desired.1) {
            if Some(g.token) == target_token || !g.overlaps(window.0, window.1) {
                continue;
            }
            let before = window;
            if g.end <= required.0 {
                window.0 = window.0.max(g.end);
            } else if g.start >= required.1 {
                window.1 = window.1.min(g.start);
            }
            if window != before && g.owner_nonce != scope {
                foreign_trimmed = true;
            }
        }
        debug_assert!(window.0 <= required.0 && window.1 >= required.1);
        let trimmed = foreign_trimmed;

        // 4. The merge target: the grant overlapping required, or a
        // same-scope grant overlapping/abutting the clipped window (its
        // clip left it exactly adjacent). Left neighbor preferred; the
        // right wall already clipped the window, so the union absorbs
        // nothing.
        let target = required_target.or_else(|| {
            self.ranges
                .iter()
                .filter(|g| g.owner_nonce == scope && g.mode == mode)
                .find(|g| g.end == window.0 || g.start == window.1)
        });
        if let Some(g) = target {
            return RangeDecision {
                plan: RangePlan::Extend {
                    token: g.token,
                    span: (g.start.min(window.0), g.end.max(window.1)),
                },
                trimmed,
            };
        }
        RangeDecision {
            plan: RangePlan::New { span: window },
            trimmed,
        }
    }

    /// Apply a [`RangePlan::Extend`]: widen the grant carrying `token` to
    /// `span` (its union with the clipped window — computed under this
    /// same entry lock), unioning `required` into the grant's
    /// required-union watermark (§9.3a — the extension's own never-trim
    /// floor is honest custody the tail-shrink arm must not reclaim). The
    /// record is re-positioned to keep the `(start, token)` sort order;
    /// its token and owner survive (the extension mints nothing — the
    /// client's cached token, and every in-flight write fencing on it,
    /// stay current). `false` ⇔ no grant carries `token` (a caller
    /// applying a stale plan — impossible under the one-critical-section
    /// discipline, honest for the loom model).
    pub fn widen_grant(&mut self, token: u64, span: (u64, u64), required: (u64, u64)) -> bool {
        let Some(at) = self.ranges.iter().position(|g| g.token == token) else {
            return false;
        };
        let mut g = self.ranges.remove(at);
        debug_assert!(span.0 <= g.start && span.1 >= g.end, "widen only unions");
        g.start = span.0;
        g.end = span.1;
        g.required.0 = g.required.0.min(required.0);
        g.required.1 = g.required.1.max(required.1);
        let at = self
            .ranges
            .partition_point(|r| (r.start, r.token) < (g.start, g.token));
        self.widest = self.widest.max(g.len());
        self.ranges.insert(at, g);
        true
    }

    /// §9.3a: union `required` into the grant's required-union watermark
    /// WITHOUT widening its span — the COVERED serve's mutation. A wire
    /// re-ask served Covered proves the holder is actively claiming those
    /// bytes, and recording it is what closes the ack race: a tail byte
    /// Covered-served after the shrink notice travelled raises the
    /// required union past the floor, so the late-arriving ack resolves
    /// [`ShrinkResolution::Demoted`] instead of releasing a byte the
    /// holder may be DMAing (the client-local face of the same race is
    /// closed by the cache-shrink-before-watermark-read order).
    pub fn note_required(&mut self, token: u64, required: (u64, u64)) {
        if let Some(g) = self.ranges.iter_mut().find(|g| g.token == token) {
            g.required.0 = g.required.0.min(required.0);
            g.required.1 = g.required.1.max(required.1);
        }
    }

    /// §9.3a: shrink the grant carrying `token` to end at `new_end` (never
    /// widening; the span order key `(start, token)` is untouched). The
    /// CLIENT half of a shrink — its adopted record narrows to match the
    /// authority's resolution; the required union narrows with the span
    /// (an adopted record's required IS its span). `false` ⇔ no grant
    /// carries `token`.
    pub fn shrink_grant_tail(&mut self, token: u64, new_end: u64) -> bool {
        let Some(at) = self.ranges.iter().position(|g| g.token == token) else {
            return false;
        };
        let g = &mut self.ranges[at];
        if new_end >= g.end || new_end <= g.start {
            // Never widen; never shrink to empty (a full release is the
            // retire path's job, with its own wake and accounting).
            return new_end >= g.end;
        }
        let old_len = g.end - g.start;
        g.end = new_end;
        g.required.1 = g.required.1.min(new_end);
        g.required.0 = g.required.0.min(g.required.1);
        if old_len == self.widest {
            self.widest = self.ranges.iter().map(Grant::len).max().unwrap_or_default();
        }
        true
    }

    /// Retire one grant, nonce- and token-conditional (the pre-S11
    /// `remove_if_sync(|held| held.owner_nonce == nonce)` discipline,
    /// strengthened by the globally unique token). `true` ⇔ something was
    /// retired, which is what licenses the waiter wake.
    ///
    /// The keyed probe uses the lease's ACQUISITION span; a grant widened
    /// by the admit-time merge may have moved (its start can decrease),
    /// so a keyed miss falls back to an O(n) scan on the globally-unique
    /// token — correctness over constants at a population the §9.2
    /// bounds keep small.
    pub fn retire(&mut self, span: Option<(u64, u64)>, token: u64, owner_nonce: u64) -> bool {
        let mine = |g: &Grant| g.token == token && g.owner_nonce == owner_nonce;
        match span {
            None => match self.wholes.iter().position(mine) {
                Some(at) => {
                    self.wholes.remove(at);
                    true
                }
                None => false,
            },
            Some((start, _)) => {
                // The list is sorted by `(start, token)` and that pair is
                // exactly this grant's insertion key, so the search lands
                // on it, on a stranger — or nowhere, when the merge moved
                // it (the fallback scan below).
                let at = self
                    .ranges
                    .partition_point(|g| (g.start, g.token) < (start, token));
                let at = match self.ranges.get(at) {
                    Some(g) if mine(g) => Some(at),
                    _ => self.ranges.iter().position(mine),
                };
                match at {
                    Some(at) => {
                        let len = self.ranges[at].len();
                        self.ranges.remove(at);
                        if self.ranges.is_empty() {
                            self.widest = 0;
                        } else if len == self.widest {
                            self.widest =
                                self.ranges.iter().map(Grant::len).max().unwrap_or_default();
                        }
                        true
                    }
                    None => false,
                }
            }
        }
    }

    /// Is a grant carrying `token` live on this file?
    ///
    /// A token is globally unique per mint, so an affirmative answer names
    /// exactly ONE grant — which is what lets DLM S9's adoption recognise
    /// "this is the grant I was just issued, seen from the grantee's side".
    pub fn holds_token(&self, token: u64) -> bool {
        self.wholes
            .iter()
            .chain(self.ranges.iter())
            .any(|g| g.token == token)
    }

    /// Does this client still hold the grant a lease names?
    pub fn holds(&self, token: u64, owner_nonce: u64) -> bool {
        self.wholes
            .iter()
            .chain(self.ranges.iter())
            .any(|g| g.token == token && g.owner_nonce == owner_nonce)
    }

    pub fn is_vacant(&self) -> bool {
        self.wholes.is_empty() && self.ranges.is_empty()
    }

    /// Raise the object's readable generation without taking custody
    /// (the `test_bump_fencing_generation` seam).
    pub fn bump_generation(&mut self, token: u64) {
        self.max_token = self.max_token.max(token);
    }

    /// W1 clause 7's question: is `[start, end)` under byte-range custody
    /// that `holder_token`'s writer does not solely own?
    ///
    /// A live WHOLE-FILE grant is whole-inode custody by definition and is
    /// therefore never range-shared — which is why the clause is inert on
    /// the shipped write path (it takes exactly that lease).
    pub fn span_is_range_shared(&self, start: u64, end: u64, holder_token: u64) -> bool {
        // Rung 17: a DEMOTED region is authority-assembled for EVERY
        // holder — a sole covering grant is still extent-ship-only there
        // (KD-MW-8's custody transfer). One `is_empty` probe shipped.
        if !self.demoted.is_empty() && self.demoted.iter().any(|&(s, e)| e > start && s < end) {
            return true;
        }
        self.candidates(start, end)
            .iter()
            .any(|g| g.overlaps(start, end) && (g.token != holder_token || !g.covers(start, end)))
    }

    /// Finding 26 (`.benchmarks/2026-08-25-s11-freeloop-stall.md`) — the
    /// ARBITER's form of the sharing question: do grants from TWO OR
    /// MORE distinct HOLDERS (`owner_nonce`, the merge-scope identity
    /// grants coalesce under — grant TOKENS are per-mint unique, so one
    /// holder's abutting pair would read as two) overlap `[start, end)`?
    ///
    /// The authority folding shipped-assembly extents is the single
    /// overlapping holder's PROXY — the fold publishes that holder's own
    /// bytes, serialized against its shipped publishes by the per-ino
    /// serve stripe — and a DEMOTED region is the arbiter's own vehicle
    /// (every holder's writes there ship TO this fold, KD-MW-8), so
    /// neither is "shared" seen from the arbiter; attempt 8's engagement
    /// failure was [`Self::span_is_range_shared`] declining that fold on
    /// every pass. TWO distinct holders stay shared: the fold cannot be
    /// both writers' proxy at once, and the sharing-safe CoW/demotion
    /// vehicle keeps that shape. Holder-side semantics are untouched.
    pub fn span_is_range_shared_beyond_one_holder(&self, start: u64, end: u64) -> bool {
        let mut holder: Option<u64> = None;
        for g in self
            .wholes
            .iter()
            .chain(self.candidates(start, end).iter())
            .filter(|g| g.overlaps(start, end))
        {
            match holder {
                None => holder = Some(g.owner_nonce),
                Some(n) if n != g.owner_nonce => return true,
                _ => {}
            }
        }
        false
    }

    // -----------------------------------------------------------------
    // Rung 17 (§9.3) — the demotion barrier's core state transitions.
    // Callers hold the entry lock (the plan/admit discipline).
    // -----------------------------------------------------------------

    /// Any whole-file custody live? (The demotion story is range-vs-range
    /// by design — a whole-file holder keeps the plain conflict law.)
    pub fn has_wholes(&self) -> bool {
        !self.wholes.is_empty()
    }

    /// The live demoted regions (the grant reply carries them so an
    /// adopting client marks its local table).
    pub fn demoted_regions(&self) -> Vec<(u64, u64)> {
        self.demoted.clone()
    }

    /// Mark `region` demoted (merge-inserted; overlapping/abutting
    /// regions union).
    pub fn adopt_demoted(&mut self, region: (u64, u64)) {
        debug_assert!(region.0 < region.1);
        let (mut s, mut e) = region;
        self.demoted.retain(|&(rs, re)| {
            if re >= s && rs <= e {
                s = s.min(rs);
                e = e.max(re);
                false
            } else {
                true
            }
        });
        let at = self.demoted.partition_point(|&(rs, _)| rs < s);
        self.demoted.insert(at, (s, e));
    }

    /// Is `[start, end)` wholly inside the demoted regions?
    pub fn span_within_demoted(&self, start: u64, end: u64) -> bool {
        // Regions are disjoint and sorted; a span within must sit inside
        // ONE region (unions coalesce abutting regions).
        self.demoted
            .iter()
            .any(|&(rs, re)| rs <= start && re >= end)
    }

    /// Foreign range grants whose **block hull** overlaps `span`'s block
    /// hull OUTSIDE the demoted regions — the §9.3 stab. Byte-overlapping
    /// grants are included (their carve is the same demotion); shares
    /// wholly inside demoted regions are licensed and excluded. Each
    /// answer carries the sharer's REQUIRED-union hull end (§9.3a): a
    /// share lying wholly at-or-beyond it is a share with the grant's
    /// desired-minted TAIL only, which the shrink arm reclaims instead of
    /// demoting.
    pub fn foreign_block_sharers(
        &self,
        span: (u64, u64),
        block_size: u64,
        scope: u64,
    ) -> Vec<BlockSharer> {
        if block_size == 0 || self.ranges.is_empty() {
            return Vec::new();
        }
        let hull = block_align_out(span, block_size);
        let mut out = Vec::new();
        for g in self.candidates(hull.0, hull.1) {
            if g.owner_nonce == scope || !g.overlaps(hull.0, hull.1) {
                continue;
            }
            let g_hull = block_align_out((g.start, g.end), block_size);
            let s = hull.0.max(g_hull.0);
            let e = hull.1.min(g_hull.1);
            if s >= e || self.span_within_demoted(s, e) {
                continue;
            }
            out.push(BlockSharer {
                token: g.token,
                region: (s, e),
                required_hull_end: block_align_out(g.required, block_size).1,
                owner_nonce: g.owner_nonce,
            });
        }
        out
    }

    /// Are ALL of `required`'s conflicting overlaps LICENSED — i.e. every
    /// foreign (or own-incompatible-mode) byte overlap lies wholly inside
    /// a demoted region? The coexistence admit's safety check: within a
    /// demoted region nobody DMAs (every holder ships extents and the
    /// AUTHORITY is the single publisher), so EX byte overlap is
    /// tolerable there and only there.
    pub fn required_overlaps_licensed(&self, required: (u64, u64), scope: u64) -> bool {
        for g in self.candidates(required.0, required.1) {
            if !g.overlaps(required.0, required.1) {
                continue;
            }
            if g.owner_nonce == scope && g.mode == LockMode::Exclusive {
                continue; // the own-EX arm is plan_range's (Covered/Extend)
            }
            let s = required.0.max(g.start);
            let e = required.1.min(g.end);
            if !self.span_within_demoted(s, e) {
                return false;
            }
        }
        true
    }

    /// The holder's own EX grant overlapping or abutting `required`, as
    /// `(token, union span)` — the licensed-coexistence admit's merge
    /// target (within a demoted region a stream must still converge to
    /// O(1) records, or the §9.2 geometry cap refuses the workload the
    /// demotion exists to serve).
    pub fn own_adjacent_grant(
        &self,
        required: (u64, u64),
        scope: u64,
    ) -> Option<(u64, (u64, u64))> {
        self.ranges
            .iter()
            .filter(|g| g.owner_nonce == scope && g.mode == LockMode::Exclusive)
            .find(|g| g.end >= required.0 && g.start <= required.1)
            .map(|g| (g.token, (g.start.min(required.0), g.end.max(required.1))))
    }

    /// Mark one demotion pending. `true` = a NEW record (the ledger's
    /// `range_custody_demotions` increment); an existing same-incumbent
    /// overlapping record widens instead (idempotent re-asks from a
    /// parked waiter's re-plans never double-count).
    pub fn mark_demotion_pending(&mut self, region: (u64, u64), incumbent_token: u64) -> bool {
        if let Some(p) = self.pending.iter_mut().find(|p| {
            p.incumbent_token == incumbent_token && p.region.1 >= region.0 && p.region.0 <= region.1
        }) {
            p.region.0 = p.region.0.min(region.0);
            p.region.1 = p.region.1.max(region.1);
            return false;
        }
        self.pending.push(DemotionPending {
            region,
            incumbent_token,
            acked: false,
        });
        true
    }

    /// The UNACKED pendings naming `incumbent_token` — the reply-carried
    /// notice's read, every custody-channel carrier (composed under this
    /// same entry serialization).
    pub fn demotion_notices_for_token(&self, incumbent_token: u64) -> Vec<(u64, u64)> {
        self.pending
            .iter()
            .filter(|p| !p.acked && p.incumbent_token == incumbent_token)
            .map(|p| p.region)
            .collect()
    }

    /// The incumbent's ACK: mark every pending naming `(token, region)`
    /// acked and adopt the region as DEMOTED (authority-assembled).
    /// `true` = at least one pending newly acked.
    pub fn ack_demotion(&mut self, incumbent_token: u64, region: (u64, u64)) -> bool {
        let mut acked = false;
        for p in self.pending.iter_mut() {
            if !p.acked
                && p.incumbent_token == incumbent_token
                && p.region.1 >= region.0
                && p.region.0 <= region.1
            {
                p.acked = true;
                acked = true;
            }
        }
        if acked {
            self.adopt_demoted(region);
        }
        acked
    }

    /// Sweep the pendings whose INCUMBENT grant just retired
    /// (release / revocation / lease expiry): the barrier resolves
    /// through the **fence column** — the incumbent's custody over the
    /// block is provably retired without an ack. Returns the number of
    /// UN-ACKED pendings resolved (the `demotion_fence_resolves`
    /// increment; acked ones were already counted as acks).
    pub fn sweep_pendings_of(&mut self, incumbent_token: u64) -> usize {
        let before = self
            .pending
            .iter()
            .filter(|p| !p.acked && p.incumbent_token == incumbent_token)
            .count();
        self.pending
            .retain(|p| p.incumbent_token != incumbent_token);
        before
    }

    // -----------------------------------------------------------------
    // §9.3a — the tail-shrink arm's core state transitions (residual
    // board item 7's fix). Callers hold the entry lock, exactly like the
    // demotion transitions above.
    // -----------------------------------------------------------------

    /// Mark one tail shrink pending. `true` = a NEW record (the ledger's
    /// `range_custody_tail_shrinks` increment); an existing same-incumbent
    /// record deepens to the MIN floor instead (idempotent re-asks from a
    /// parked waiter's re-plans never double-count, and a second asker
    /// with a deeper boundary merges — one shrink frees them all).
    pub fn mark_shrink_pending(&mut self, incumbent_token: u64, floor: u64, block: u64) -> bool {
        debug_assert!(block > 0);
        if let Some(p) = self
            .shrink_pending
            .iter_mut()
            .find(|p| p.incumbent_token == incumbent_token)
        {
            p.floor = p.floor.min(floor);
            return false;
        }
        self.shrink_pending.push(ShrinkPending {
            incumbent_token,
            floor,
            block,
        });
        true
    }

    /// The pending shrink floor naming `incumbent_token` — the
    /// reply-carried notice's read, every custody-channel carrier
    /// (composed under this same entry serialization, the demotion
    /// notice's in-flight-renewal race pin verbatim).
    pub fn shrink_notice_for_token(&self, incumbent_token: u64) -> Option<u64> {
        self.shrink_pending
            .iter()
            .find(|p| p.incumbent_token == incumbent_token)
            .map(|p| p.floor)
    }

    /// The incumbent's shrink ACK: resolve the pending against its written
    /// high-water `watermark` (the max byte end it served write custody
    /// for inside the grant; `u64::MAX` = "unknown — treat my whole span
    /// as potentially written", the shed-cache degradation).
    ///
    /// Returns the resolution plus the number of UN-ACKED demotion
    /// pendings of the same incumbent that became provably moot because
    /// the shrink retired the incumbent's custody past their region —
    /// resolved through the demotion ledger's FENCE column (custody
    /// provably retired without an ack, the `sweep_pendings_of` law).
    pub fn ack_tail_shrink(
        &mut self,
        incumbent_token: u64,
        watermark: u64,
    ) -> (ShrinkResolution, usize) {
        let Some(at) = self
            .shrink_pending
            .iter()
            .position(|p| p.incumbent_token == incumbent_token)
        else {
            return (ShrinkResolution::None, 0);
        };
        let pending = self.shrink_pending.remove(at);
        let Some(g_at) = self.ranges.iter().position(|g| g.token == incumbent_token) else {
            // The grant died between mark and ack — its retire already
            // swept the barrier's waiters through the fence column; the
            // late ack resolves to nothing. (Unreachable through the
            // shipped retire, which sweeps shrink pendings too; honest
            // for a direct core caller.)
            return (ShrinkResolution::None, 0);
        };
        let (old_len, resolution, new_end) = {
            let g = &mut self.ranges[g_at];
            let req_hull_end = block_align_out(g.required, pending.block).1;
            let w_hull_end = if watermark == 0 {
                0
            } else {
                watermark
                    .div_ceil(pending.block)
                    .saturating_mul(pending.block)
            };
            let honest_end = req_hull_end.max(w_hull_end).min(g.end);
            let old_len = g.end - g.start;
            if honest_end <= pending.floor {
                // The contested tail is unwritten: release it whole.
                let floor = pending.floor.min(g.end).max(g.start);
                g.end = floor;
                g.required.1 = g.required.1.min(floor);
                (old_len, ShrinkResolution::Shrunk { floor }, floor)
            } else {
                // The incumbent truly wrote into the tail: keep the
                // written hull — it is honest custody now (union the
                // watermark into required so every later classification
                // reads it as such) — and let the EXISTING demotion
                // barrier arbitrate the still-contested blocks.
                g.end = honest_end;
                g.required.1 = g.required.1.max(watermark.min(honest_end));
                (
                    old_len,
                    ShrinkResolution::Demoted {
                        new_end: honest_end,
                    },
                    honest_end,
                )
            }
        };
        if old_len == self.widest {
            self.widest = self.ranges.iter().map(Grant::len).max().unwrap_or_default();
        }
        // Demotion pendings of this incumbent wholly beyond the shrunk
        // end are moot — their region's custody is provably retired
        // (the fence column); partial overlaps clamp to the new end so a
        // later ack cannot adopt-demote blocks the incumbent no longer
        // holds.
        let mut fence_resolved = 0usize;
        self.pending.retain_mut(|p| {
            if p.incumbent_token != incumbent_token {
                return true;
            }
            if p.region.0 >= new_end {
                if !p.acked {
                    fence_resolved += 1;
                }
                return false;
            }
            p.region.1 = p.region.1.min(new_end.max(p.region.0));
            true
        });
        (resolution, fence_resolved)
    }

    /// Sweep the shrink pendings whose INCUMBENT grant just retired
    /// (release / revocation / lease expiry): the whole grant died, so
    /// the shrink resolves trivially through the **fence column** —
    /// custody over the contested tail is provably retired without an
    /// ack. Returns the number resolved (the
    /// `range_custody_tail_shrink_fence_resolves` increment).
    pub fn sweep_shrink_pendings_of(&mut self, incumbent_token: u64) -> usize {
        let before = self
            .shrink_pending
            .iter()
            .filter(|p| p.incumbent_token == incumbent_token)
            .count();
        self.shrink_pending
            .retain(|p| p.incumbent_token != incumbent_token);
        before
    }
}

/// **The S1 publication order, as one function** (the loom model's fourth
/// obligation): raise the stripe floor to `token` BEFORE the admit runs,
/// so no reader — live-entry or floor-fallback — can ever observe a
/// granted token the floor does not cover, across any interleaving of
/// admits, releases and reads. Weakening (admitting first) lets a racing
/// release drop the entry and a reader fall back to a floor below the
/// grant: a generation regression, the stale-reject arm's death.
#[inline]
pub fn publish_floor_then_admit<R>(floor: &AtomicU64, token: u64, admit: impl FnOnce() -> R) -> R {
    floor.fetch_max(token, Ordering::AcqRel);
    admit()
}
