//! Placed-sever **claims protocol core** (shim-parity campaign,
//! 2026-07-28) — the lock-free word protocol that lets ring WRITE severs
//! target a shared per-(ino, block) assembly buffer safely.
//!
//! A ring write's §5.5.2 sever copy must leave client-writable arena
//! memory synchronously at dequeue, so the single copy's DESTINATION must
//! be decided there — on a foreign service thread, with no inode or block
//! lock held. Multiple in-flight chunks of one block share ONE assembly
//! (that sharing is the whole win: the first merging handler adopts the
//! assembly as the `ActiveBlockBuf` backing and every sibling merge
//! becomes a coverage record with the copy elided). Two hazards make a
//! protocol necessary:
//!
//! 1. **Concurrent claim overlap** — two severs writing one region would
//!    interleave torn content that matches NEITHER write (a POSIX
//!    serialization violation, unlike the benign torn reads of racing
//!    client memory). The page-granular claim bitmap makes every live
//!    claim region exclusive; an overlapping claim refuses (the caller
//!    falls back to the pooled 2-copy sever — correctness owns
//!    ambiguity).
//! 2. **Sever-vs-adoption race** — once an assembly is adopted as a live
//!    overlay backing, its memory is reachable by snapshot readers, so a
//!    still-copying sever would mutate frozen bytes. The
//!    [`PlacedClaims::seal_for_adoption`] / claim-side sealed check is a
//!    store-buffering (Dekker) pair — the same `SeqCst` shape as the W1
//!    §5.1 clone/patch fence: the claimer publishes its writer presence
//!    (`writers`) THEN checks `sealed`; the adopter publishes `sealed`
//!    THEN reads `writers`. `SeqCst` forbids both observing the old
//!    value, so either the claimer backs off before writing a byte or
//!    the adopter refuses the adoption. Sealing is permanent: a refused
//!    adoption leaves the assembly a plain private buffer (every merge
//!    falls back to the ordinary copy — never wrong, only slower).
//!
//! Claim bits are released when the op's payload handle drops (the
//! region stays exclusive for the payload's whole life); `writers` spans
//! only the memcpy window. This module is dependency-free so
//! `loom-models/` `#[path]`-includes the exact shipped code
//! (`placed_core` model: overlap exclusion, the Dekker pair, seal
//! permanence).

#[cfg(loom)]
use loom::sync::atomic::{fence, AtomicBool, AtomicU64, AtomicUsize, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{fence, AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Claim granularity: one bit per 4 KiB page (placed severs are
/// page-aligned by contract — the sink screens `rel % 4096 == 0 &&
/// len % 4096 == 0`).
pub const CLAIM_PAGE: usize = 4096;

/// The per-assembly claim state. All methods are lock-free and callable
/// from any thread.
pub struct PlacedClaims {
    /// One bit per [`CLAIM_PAGE`]; set = a live placed sever owns it.
    words: Box<[AtomicU64]>,
    /// Claimable page count (the words are rounded up to 64).
    pages: usize,
    /// Severs inside their memcpy window (claim granted, copy not done).
    writers: AtomicUsize,
    /// Permanent: no further claims may be granted (adoption reached the
    /// assembly, or an adoption attempt latched it defensively).
    sealed: AtomicBool,
}

impl PlacedClaims {
    /// Claim state for a buffer of `len` bytes (rounded up to whole
    /// pages).
    pub fn new(len: usize) -> Self {
        let pages = len.div_ceil(CLAIM_PAGE).max(1);
        let words = (0..pages.div_ceil(64))
            .map(|_| AtomicU64::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            words,
            pages,
            writers: AtomicUsize::new(0),
            sealed: AtomicBool::new(false),
        }
    }

    /// Word/mask decomposition of a page range (end exclusive).
    fn masks(&self, first_page: usize, page_count: usize) -> Vec<(usize, u64)> {
        let mut out = Vec::new();
        let mut page = first_page;
        let end = first_page + page_count;
        while page < end {
            let word = page / 64;
            let lo = page % 64;
            let hi = (end - word * 64).min(64);
            let bits = hi - lo;
            let mask = if bits == 64 {
                u64::MAX
            } else {
                ((1u64 << bits) - 1) << lo
            };
            out.push((word, mask));
            page = word * 64 + hi;
        }
        out
    }

    /// Begin a placed sever over pages `[first_page, first_page +
    /// page_count)`: grants an exclusive claim AND opens the writer
    /// window. `false` = overlap with a live claim, out-of-range, or the
    /// assembly is sealed — the caller must fall back to the pooled
    /// sever. On success the caller performs its copy, then calls
    /// [`Self::end_write`]; the claim itself stays held until
    /// [`Self::release`].
    pub fn begin_claim(&self, first_page: usize, page_count: usize) -> bool {
        if page_count == 0 || first_page + page_count > self.pages {
            return false;
        }
        let masks = self.masks(first_page, page_count);
        // Phase 1: exclusive bits, word by word (rollback on conflict).
        for (i, &(word, mask)) in masks.iter().enumerate() {
            let prev = self.words[word].fetch_or(mask, Ordering::SeqCst);
            if prev & mask != 0 {
                // Conflict: undo the bits WE set in this word, then every
                // earlier word wholesale (they were conflict-free).
                self.words[word].fetch_and(!(mask & !prev), Ordering::SeqCst);
                for &(w, m) in &masks[..i] {
                    self.words[w].fetch_and(!m, Ordering::SeqCst);
                }
                return false;
            }
        }
        // Phase 2: publish writer presence, THEN check sealed — the
        // claimer half of the Dekker pair (see module docs). The
        // `fence(SeqCst)` between the writer publish and the sealed
        // check is LOAD-BEARING (the W1 §5.1 fence protocol shape,
        // paired with the fence in `seal_for_adoption`): remove it and
        // the store-buffering interleaving lets a sever memcpy proceed
        // into an adopted (snapshot-reachable) backing — the loom model
        // `placed_seal_vs_claim_dekker_never_adopts_over_a_writer`
        // fails on weakening.
        self.writers.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        if self.sealed.load(Ordering::SeqCst) {
            self.writers.fetch_sub(1, Ordering::SeqCst);
            for &(w, m) in &masks {
                self.words[w].fetch_and(!m, Ordering::SeqCst);
            }
            return false;
        }
        true
    }

    /// The sever memcpy finished (the claim stays held).
    pub fn end_write(&self) {
        self.writers.fetch_sub(1, Ordering::SeqCst);
    }

    /// Release a claim granted by [`Self::begin_claim`] (payload-drop
    /// time).
    pub fn release(&self, first_page: usize, page_count: usize) {
        for (w, m) in self.masks(first_page, page_count) {
            self.words[w].fetch_and(!m, Ordering::SeqCst);
        }
    }

    /// The adopter half of the Dekker pair: permanently seal the
    /// assembly, then report whether adoption is safe (`true` ⇔ no sever
    /// is inside its memcpy window; every future claim refuses on the
    /// seal). `false` latches the seal anyway — the assembly stays a
    /// plain private buffer forever (merge-copy fallback, never reused
    /// as a backing).
    pub fn seal_for_adoption(&self) -> bool {
        self.sealed.store(true, Ordering::SeqCst);
        // The adopter half of the Dekker pair — see `begin_claim`'s
        // phase-2 comment; this fence pairs with that one and is equally
        // load-bearing.
        fence(Ordering::SeqCst);
        self.writers.load(Ordering::SeqCst) == 0
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn overlapping_claims_are_exclusive_until_release() {
        let c = PlacedClaims::new(64 * CLAIM_PAGE);
        assert!(c.begin_claim(0, 16), "first claim");
        c.end_write();
        assert!(!c.begin_claim(8, 16), "overlap must refuse");
        assert!(c.begin_claim(16, 16), "disjoint claim proceeds");
        c.end_write();
        c.release(0, 16);
        assert!(c.begin_claim(0, 16), "released region is claimable again");
        c.end_write();
    }

    #[test]
    fn claims_spanning_words_roll_back_cleanly_on_conflict() {
        let c = PlacedClaims::new(256 * CLAIM_PAGE);
        assert!(c.begin_claim(100, 8), "hold pages 100..108");
        c.end_write();
        // 60..160 spans three words and overlaps 100..108 → refuse, and
        // NOTHING of 60..160 may remain claimed.
        assert!(!c.begin_claim(60, 100), "overlap must refuse");
        c.release(100, 8);
        assert!(c.begin_claim(60, 100), "rollback must leave no residue");
        c.end_write();
    }

    #[test]
    fn sealed_refuses_new_claims_and_is_permanent() {
        let c = PlacedClaims::new(64 * CLAIM_PAGE);
        assert!(c.begin_claim(0, 4));
        c.end_write();
        assert!(c.seal_for_adoption(), "no writer in window ⇒ adoptable");
        assert!(!c.begin_claim(8, 4), "sealed refuses every new claim");
        assert!(
            c.seal_for_adoption(),
            "idempotent: still no writer in window"
        );
    }

    #[test]
    fn adoption_refuses_while_a_writer_is_mid_copy() {
        let c = PlacedClaims::new(64 * CLAIM_PAGE);
        assert!(c.begin_claim(0, 4));
        // Writer window open (no end_write yet): adoption must refuse —
        // and the seal latches.
        assert!(!c.seal_for_adoption(), "mid-copy writer blocks adoption");
        c.end_write();
        // The refused adoption still sealed the assembly permanently.
        assert!(!c.begin_claim(8, 4), "sealed refuses new claims");
    }

    #[test]
    fn out_of_range_claims_refuse() {
        let c = PlacedClaims::new(8 * CLAIM_PAGE);
        assert!(!c.begin_claim(0, 0), "empty claim refuses");
        assert!(!c.begin_claim(7, 2), "past-end claim refuses");
        assert!(c.begin_claim(7, 1), "last page is claimable");
        c.end_write();
    }
}
