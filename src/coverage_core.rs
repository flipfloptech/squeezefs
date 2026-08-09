//! The **written-coverage union** — ONE coverage law for RAM and device
//! accumulation (device-overlay design KD-OV-2, PR B1).
//!
//! Extracted verbatim from `ActiveBlockBuf::record_write`'s union
//! (RW3b / FIND-L1-A: block completeness is the ACCUMULATED
//! written-coverage union — overlap-safe, order-blind, kernel-split
//! out-of-order segments are the normal case). The RW3b campaign paid
//! for this union once; a second implementation would re-earn its bugs,
//! so `active_block.rs` (RAM accumulation) and `overlay_core.rs`
//! (device accumulation) both consume THIS module — drift between the
//! two coverage laws is unrepresentable by construction.
//!
//! Semantics (the `record_write` contract, unchanged):
//! * a **primary run** `[primary.0, primary.1)` — the only state an
//!   in-order stream ever touches (no allocation);
//! * **out-of-order extras** — additional runs, sorted by start,
//!   pairwise disjoint and non-abutting, each disjoint and non-abutting
//!   from the primary;
//! * the **completion transition** fires exactly once, when the union
//!   reaches `[0, len)` — never because a segment's end coincides with
//!   the block end, and never twice (a re-write of a completed union
//!   reports no transition).
//!
//! Dependency-free (no atomics, no crate types) so `loom-models/` and
//! the overlay core can `#[path]`-include or `crate::`-reference it
//! from any workspace.

/// The verdict of one [`CoverageUnion::record`] merge.
#[derive(Debug, Clone, Copy, Default)]
pub struct RecordVerdict {
    /// This merge completed the union to the whole `[0, len)` range —
    /// the write-through-class trigger. Fires exactly once.
    pub completed: bool,
    /// The range was disjoint from every existing run and was recorded
    /// as an out-of-order extra (the `active_block_ooo_runs` input).
    pub out_of_order: bool,
    /// The completion fired while out-of-order runs were still
    /// uncoalesced — structurally impossible (a completing primary
    /// absorbs every reachable extra); the caller's tripwire input.
    pub completed_with_extras: bool,
}

/// Overlap-safe, order-blind written-coverage union over `[0, len)`.
///
/// `len` is deliberately a per-call argument (not stored): both
/// consumers already own the authoritative block size, and the union's
/// arithmetic is length-independent until the completion check.
#[derive(Debug, Clone)]
pub struct CoverageUnion {
    /// Primary written run `[primary.0, primary.1)`; `(0, 0)` = empty.
    primary: (u32, u32),
    /// Out-of-order overflow runs — sorted by start, pairwise disjoint
    /// and non-abutting, disjoint and non-abutting from the primary.
    extra: Vec<(u32, u32)>,
}

impl Default for CoverageUnion {
    fn default() -> Self {
        Self::new()
    }
}

impl CoverageUnion {
    /// An empty union.
    pub const fn new() -> Self {
        Self {
            primary: (0, 0),
            extra: Vec::new(),
        }
    }

    /// Merge a write of `[start, end)` into the union and report the
    /// transition verdict. Callers serialize mutations (the block-lock
    /// law); `start <= end <= len` is the caller's contract.
    pub fn record(&mut self, start: u32, end: u32, len: u32) -> RecordVerdict {
        debug_assert!(
            start <= end && end <= len,
            "write range out of block bounds"
        );
        let mut v = RecordVerdict::default();
        if start == end {
            return v;
        }
        if self.is_full(len) {
            // Already-complete: no transition to report (a re-write of a
            // completed union must not double-fire the write-through).
            return v;
        }
        let (s, e) = (start, end);
        let (p0, p1) = self.primary;
        if p0 == p1 {
            // First touch.
            self.primary = (s, e);
        } else if s <= p1 && e >= p0 {
            // Overlaps or abuts the primary run: extend it, then absorb
            // any extras the grown primary now reaches (a bridging write
            // can connect runs on both sides).
            self.primary = (p0.min(s), p1.max(e));
            self.coalesce_extras_into_primary();
        } else {
            // Disjoint from the primary: an out-of-order run. Insert into
            // the sorted extras, coalescing with overlapping/abutting
            // neighbours (extras stay disjoint from the primary by
            // construction: a run reaching the primary is caught above).
            self.insert_extra_run(s, e);
            v.out_of_order = true;
        }
        if self.is_full(len) {
            v.completed = true;
            v.completed_with_extras = !self.extra.is_empty();
        }
        v
    }

    /// The union spans the whole `[0, len)` range. Extras are disjoint
    /// from the primary, so a full primary implies no extras.
    pub fn is_full(&self, len: u32) -> bool {
        self.primary == (0, len)
    }

    /// The primary run (the single-run fast-path view).
    pub fn primary(&self) -> (u32, u32) {
        self.primary
    }

    /// Total live run count (primary + extras) — the extent-count face.
    pub fn run_count(&self) -> usize {
        self.extra.len() + usize::from(self.primary.0 != self.primary.1)
    }

    /// Claim the whole `[0, len)` range (a stage/upload exit that made
    /// the complement whole — `zero_complete` / seed-fill). Consumes any
    /// pending completion transition: a later [`Self::record`] reports
    /// `completed == false`.
    pub fn set_full(&mut self, len: u32) {
        self.primary = (0, len);
        self.extra.clear();
    }

    /// Whether `[start, end)` lies entirely within one written run.
    /// Runs are pairwise non-abutting, so single-run containment equals
    /// union containment.
    pub fn contains(&self, start: u32, end: u32) -> bool {
        let inside = |run: (u32, u32)| run.0 <= start && end <= run.1;
        inside(self.primary) || self.extra.iter().any(|&run| inside(run))
    }

    /// All written runs in ascending offset order (primary merged into
    /// the sorted extras view). Runs are pairwise disjoint and
    /// non-abutting.
    pub fn runs_sorted(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        let p = self.primary;
        let idx = self.extra.partition_point(|&(s, _)| s < p.0);
        let (before, after) = self.extra.split_at(idx);
        before
            .iter()
            .copied()
            .chain((p.0 != p.1).then_some(p))
            .chain(after.iter().copied())
    }

    /// The unwritten gaps of `[0, len)` in ascending order.
    pub fn gaps(&self, len: u32) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        let mut cursor = 0u32;
        for (s, e) in self.runs_sorted() {
            if s > cursor {
                out.push((cursor, s));
            }
            cursor = e;
        }
        if cursor < len {
            out.push((cursor, len));
        }
        out
    }

    fn coalesce_extras_into_primary(&mut self) {
        let (mut p0, mut p1) = self.primary;
        self.extra.retain(|&(s, e)| {
            if s <= p1 && e >= p0 {
                p0 = p0.min(s);
                p1 = p1.max(e);
                false
            } else {
                true
            }
        });
        // One retain pass suffices: extras are pairwise non-abutting, so
        // a grown primary can absorb each at most once, and absorbing one
        // cannot make a previously-disjoint one reachable (any run
        // between them would have been coalesced with it already).
        self.primary = (p0, p1);
    }

    fn insert_extra_run(&mut self, s: u32, e: u32) {
        let idx = self.extra.partition_point(|&(_, re)| re < s);
        let mut end_idx = idx;
        let (mut ns, mut ne) = (s, e);
        while end_idx < self.extra.len() && self.extra[end_idx].0 <= ne {
            ns = ns.min(self.extra[end_idx].0);
            ne = ne.max(self.extra[end_idx].1);
            end_idx += 1;
        }
        self.extra.splice(idx..end_idx, [(ns, ne)]);
    }
}
