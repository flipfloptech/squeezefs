//! Stride-run slot sets — the dynamic-meta-routing at-rest encoding
//! (docs/design-dynamic-meta-routing.md §5.2/§5.3).
//!
//! A [`SlotSet`] is a set of u16 routing-slot ids represented as ordered
//! **arithmetic-progression runs** `(start, stride, count)` instead of a
//! dense per-slot list, so a volume hosting 32768 of the derived width's
//! 65536 slots costs ONE 8-byte run in its 4096-B ledger-slot membership
//! stamp — the encoding that makes `W = 2^16` free at rest. The fresh
//! identity distribution (`slot s → member s mod V`) is exactly one run
//! per member, and stride-doubling growth (`V → 2V` takes every second
//! slot of each donor's run) keeps it O(1) runs per volume at any
//! power-of-two growth depth. Scattered migration history consumes runs;
//! the [`super::checkpoint::STAMP_MAX_RUNS`] encoding budget bounds it
//! loud at encode and at migration preflight.
//!
//! Representation law: mutation and [`SlotSet::from_slots`] construction
//! **normalize** (expand → sort → greedy re-coalesce), so structurally
//! equal sets built through mutations compare equal; the stamp decode
//! path ([`SlotSet::from_runs`]) preserves validated runs verbatim —
//! encode writes a normalized set's runs, so wire round-trips are
//! structural identities.

use super::KvError;

/// One arithmetic progression of slot ids:
/// `{ start + k · stride | 0 ≤ k < count }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotRun {
    /// First slot of the run.
    pub start: u16,
    /// Progression step. `count == 1` runs are canonically `stride = 1`
    /// (a singleton's stride is meaningless; the constructors normalize).
    pub stride: u16,
    /// Members in the run, ≥ 1.
    pub count: u32,
}

impl SlotRun {
    /// The largest slot in the run (validated construction guarantees it
    /// fits u16).
    fn last(&self) -> u64 {
        u64::from(self.start) + u64::from(self.count - 1) * u64::from(self.stride)
    }

    fn contains(&self, slot: u16) -> bool {
        let s = u64::from(slot);
        let start = u64::from(self.start);
        if s < start || s > self.last() {
            return false;
        }
        if self.count == 1 {
            return s == start;
        }
        (s - start) % u64::from(self.stride) == 0
    }

    fn iter(&self) -> impl Iterator<Item = u16> + '_ {
        (0..self.count)
            .map(|k| (u64::from(self.start) + u64::from(k) * u64::from(self.stride)) as u16)
    }
}

/// An ordered set of u16 slot ids as validated, disjoint stride runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotSet {
    /// Disjoint runs, ascending by `start`.
    runs: Vec<SlotRun>,
}

impl SlotSet {
    /// The empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from an arbitrary slot population (duplicates tolerated):
    /// expand → sort → greedy coalesce. Greedy is not globally optimal
    /// run-count-wise, but it is exact on stride progressions (the fresh
    /// identity distribution and stride-doubling growth both yield their
    /// minimal run forms) and bounded-loud everywhere else via the stamp
    /// encoding budget.
    pub fn from_slots(slots: &[u16]) -> Self {
        let mut sorted = slots.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        Self {
            runs: coalesce(&sorted),
        }
    }

    /// Build from explicit runs (the stamp decode path): refuses zero
    /// counts, degenerate `stride = 0` repetitions, runs escaping the
    /// u16 slot-id namespace, and overlapping membership — the §9
    /// validate-before-trust discipline. Valid runs are preserved
    /// verbatim (sorted by start).
    pub fn from_runs(mut runs: Vec<SlotRun>) -> Result<Self, KvError> {
        let mut seen = vec![false; 1 << 16];
        for r in &runs {
            if r.count == 0 {
                return Err(KvError::Corrupt(
                    "slot run with count 0 (empty runs are not encoded)".to_string(),
                ));
            }
            if r.stride == 0 && r.count > 1 {
                return Err(KvError::Corrupt(format!(
                    "slot run at {} repeats with stride 0 (count {})",
                    r.start, r.count
                )));
            }
            if r.last() > u64::from(u16::MAX) {
                return Err(KvError::Corrupt(format!(
                    "slot run ({}, {}, {}) escapes the u16 slot-id namespace (last member {})",
                    r.start,
                    r.stride,
                    r.count,
                    r.last()
                )));
            }
            for s in r.iter() {
                if seen[usize::from(s)] {
                    return Err(KvError::Corrupt(format!(
                        "overlapping slot runs: slot {s} claimed twice"
                    )));
                }
                seen[usize::from(s)] = true;
            }
        }
        runs.sort_unstable_by_key(|r| r.start);
        Ok(Self { runs })
    }

    /// The validated runs, ascending by start.
    pub fn runs(&self) -> &[SlotRun] {
        &self.runs
    }

    /// Set cardinality.
    pub fn len(&self) -> usize {
        self.runs.iter().map(|r| r.count as usize).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// Membership test — O(runs).
    pub fn contains(&self, slot: u16) -> bool {
        self.runs.iter().any(|r| r.contains(slot))
    }

    /// The smallest member (the native-mint-slot derivation reads this).
    pub fn smallest(&self) -> Option<u16> {
        self.runs.first().map(|r| r.start)
    }

    /// Ascending member iteration (expands — control-plane surface).
    pub fn iter(&self) -> impl Iterator<Item = u16> {
        self.to_vec().into_iter()
    }

    /// Ascending member expansion.
    pub fn to_vec(&self) -> Vec<u16> {
        let mut v: Vec<u16> = self.runs.iter().flat_map(|r| r.iter()).collect();
        v.sort_unstable();
        v
    }

    /// The first `n` members ascending (the per-volume mint-set
    /// derivation, design-dynamic-meta-routing §5.4).
    pub fn first_n(&self, n: usize) -> Vec<u16> {
        let mut v = self.to_vec();
        v.truncate(n);
        v
    }

    /// Insert one slot (idempotent) — normalizes.
    pub fn insert(&mut self, slot: u16) {
        if self.contains(slot) {
            return;
        }
        let mut v = self.to_vec();
        v.push(slot);
        v.sort_unstable();
        self.runs = coalesce(&v);
    }

    /// Remove one slot (idempotent) — normalizes.
    pub fn remove(&mut self, slot: u16) {
        if !self.contains(slot) {
            return;
        }
        let v: Vec<u16> = self.to_vec().into_iter().filter(|&s| s != slot).collect();
        self.runs = coalesce(&v);
    }

    /// Keep only members satisfying `f` — normalizes.
    pub fn retain(&mut self, mut f: impl FnMut(u16) -> bool) {
        let v: Vec<u16> = self.to_vec().into_iter().filter(|&s| f(s)).collect();
        self.runs = coalesce(&v);
    }
}

/// Greedy run coalescing over a sorted, deduplicated slot list: open a
/// run with the first element, adopt the stride of the first gap, and
/// extend while the stride holds. Singletons canonicalize to
/// `stride = 1, count = 1`.
fn coalesce(sorted: &[u16]) -> Vec<SlotRun> {
    let mut runs = Vec::new();
    let mut i = 0usize;
    while i < sorted.len() {
        if i + 1 == sorted.len() {
            runs.push(SlotRun {
                start: sorted[i],
                stride: 1,
                count: 1,
            });
            break;
        }
        let start = sorted[i];
        let stride = sorted[i + 1] - start;
        let mut count = 2u32;
        let mut j = i + 1;
        while j + 1 < sorted.len() && sorted[j + 1] - sorted[j] == stride {
            j += 1;
            count += 1;
        }
        if stride == 0 {
            unreachable!("sorted+dedup input has no zero gaps");
        }
        runs.push(SlotRun {
            start,
            stride,
            count,
        });
        i = j + 1;
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_progression_is_one_run() {
        // The fresh distribution for member v of V: {v + kV}.
        for v_count in [1u16, 2, 3, 8] {
            for pos in 0..v_count {
                let slots: Vec<u16> = (pos..=u16::MAX).step_by(usize::from(v_count)).collect();
                let set = SlotSet::from_slots(&slots);
                assert_eq!(set.runs().len(), 1, "V={v_count} pos={pos}");
                assert_eq!(set.len(), slots.len());
                assert_eq!(set.smallest(), Some(pos));
            }
        }
    }

    #[test]
    fn stride_doubling_growth_stays_one_run() {
        // V → 2V: the donor keeps every second member.
        let donor: Vec<u16> = (0..=u16::MAX).step_by(2).collect(); // stride 2
        let kept: Vec<u16> = donor.iter().copied().step_by(2).collect(); // stride 4
        assert_eq!(SlotSet::from_slots(&kept).runs().len(), 1);
    }

    #[test]
    fn first_n_is_the_mint_set() {
        let set = SlotSet::from_slots(&(0..=u16::MAX).step_by(3).collect::<Vec<_>>());
        let mint = set.first_n(64);
        assert_eq!(mint.len(), 64);
        assert_eq!(mint[0], 0);
        assert_eq!(mint[63], 189);
    }
}
