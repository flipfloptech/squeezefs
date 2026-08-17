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
/// was TRIMMED against live custody (the `range_custody_desired_trims`
/// ledger's predicate — required is structurally never trimmed).
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
#[derive(Clone, Copy, Debug)]
pub struct DemotionPending {
    /// The block-aligned region being demoted.
    pub region: (u64, u64),
    /// The incumbent grant's token (the renewal notice's key — the
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
        for g in self.candidates(desired.0, desired.1) {
            if Some(g.token) == target_token || !g.overlaps(window.0, window.1) {
                continue;
            }
            if g.end <= required.0 {
                window.0 = window.0.max(g.end);
            } else if g.start >= required.1 {
                window.1 = window.1.min(g.start);
            }
        }
        debug_assert!(window.0 <= required.0 && window.1 >= required.1);
        let trimmed = window != desired;

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
    /// same entry lock). The record is re-positioned to keep the
    /// `(start, token)` sort order; its token and owner survive (the
    /// extension mints nothing — the client's cached token, and every
    /// in-flight write fencing on it, stay current). `false` ⇔ no grant
    /// carries `token` (a caller applying a stale plan — impossible under
    /// the one-critical-section discipline, honest for the loom model).
    pub fn widen_grant(&mut self, token: u64, span: (u64, u64)) -> bool {
        let Some(at) = self.ranges.iter().position(|g| g.token == token) else {
            return false;
        };
        let mut g = self.ranges.remove(at);
        debug_assert!(span.0 <= g.start && span.1 >= g.end, "widen only unions");
        g.start = span.0;
        g.end = span.1;
        let at = self
            .ranges
            .partition_point(|r| (r.start, r.token) < (g.start, g.token));
        self.widest = self.widest.max(g.len());
        self.ranges.insert(at, g);
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
        if !self.demoted.is_empty()
            && self.demoted.iter().any(|&(s, e)| e > start && s < end)
        {
            return true;
        }
        self.candidates(start, end)
            .iter()
            .any(|g| g.overlaps(start, end) && (g.token != holder_token || !g.covers(start, end)))
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
    /// hull OUTSIDE the demoted regions — the §9.3 stab: each answer is
    /// `(incumbent_token, incumbent_scope, shared block-aligned region)`.
    /// Byte-overlapping grants are included (their carve is the same
    /// demotion); shares wholly inside demoted regions are licensed and
    /// excluded.
    pub fn foreign_block_sharers(
        &self,
        span: (u64, u64),
        block_size: u64,
        scope: u64,
    ) -> Vec<(u64, u64, (u64, u64))> {
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
            out.push((g.token, g.owner_nonce, (s, e)));
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
    pub fn own_adjacent_grant(&self, required: (u64, u64), scope: u64) -> Option<(u64, (u64, u64))> {
        self.ranges
            .iter()
            .filter(|g| g.owner_nonce == scope && g.mode == LockMode::Exclusive)
            .find(|g| g.end >= required.0 && g.start <= required.1)
            .map(|g| {
                (
                    g.token,
                    (g.start.min(required.0), g.end.max(required.1)),
                )
            })
    }

    /// Mark one demotion pending. `true` = a NEW record (the ledger's
    /// `range_custody_demotions` increment); an existing same-incumbent
    /// overlapping record widens instead (idempotent re-asks from a
    /// parked waiter's re-plans never double-count).
    pub fn mark_demotion_pending(&mut self, region: (u64, u64), incumbent_token: u64) -> bool {
        if let Some(p) = self.pending.iter_mut().find(|p| {
            p.incumbent_token == incumbent_token
                && p.region.1 >= region.0
                && p.region.0 <= region.1
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

    /// The UNACKED pendings naming `incumbent_token` — the renewal
    /// notice's read (composed under this same entry serialization).
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
