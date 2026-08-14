//! IPC **completion doorbell** protocol core (the 2026-07-28 op-economy
//! campaign, lever 2 — completion-side wake economy).
//!
//! One [`CqeDoorbell`] per session, living in the shared session header
//! (line 2 — its own cache line, away from the submit doorbell). It is
//! the completion-direction mirror of the submit direction's
//! `doorbell + daemon_parked` pair:
//!
//! - **Daemon** (any completing thread — service thread fast path, tokio
//!   handler lane, direct-drive reaper): after the slot's DONE publish
//!   ([`SlotCore::complete`]), call [`CqeDoorbell::complete`] — bump the
//!   session completion sequence, then wake the seq word **only when a
//!   reaper is parked**. An unparked-reaper completion stream costs zero
//!   wake syscalls (the pre-campaign per-completion `FUTEX_WAKE` toward
//!   parked per-slot WAITERs was the measured collect-and-wake
//!   serialization term past ~525 k IOPS).
//! - **Client reaper** (the libaio merge loop): to park, call
//!   [`CqeDoorbell::park_begin`] (register parked intent AND snapshot
//!   the expected seq, in that order), then **re-scan the pending set**
//!   (the disarm→scan law: a completion that landed before the intent
//!   was visible is found by the scan), then `FUTEX_WAIT` on
//!   [`CqeDoorbell::seq_word`] against the snapshot. On return (wake,
//!   EAGAIN, timeout) call [`CqeDoorbell::park_end`].
//!
//! ## Why no wake is ever lost
//!
//! All four accesses are `SeqCst`, so they form a single total order
//! with the slot's DONE publication. A strand would need the reaper to
//! (a) miss the DONE in its post-`park_begin` scan, (b) pass the futex
//! admission (seq unchanged), while the daemon (c) read `parked == 0`
//! (no wake). (a) requires the scan to precede the DONE publish; the
//! scan follows `park_begin`'s RMW, so the daemon's later
//! `parked.load` — which follows its own DONE publish AND seq bump —
//! observes the registration and wakes: (c) is impossible. If instead
//! the seq bump precedes the snapshot, the DONE publish (sequenced
//! before the bump) is visible to the scan: (a) is impossible. And a
//! bump between snapshot and wait fails the futex admission: (b) is
//! impossible. The `ipc_cqe_doorbell_*` loom models in `loom-models`
//! check the shipped code; removing either Dekker `fence(SeqCst)`,
//! weakening the daemon's `parked` load to `Relaxed`, or reordering
//! `park_begin`'s two accesses each fails them (verified 2026-07-28).
//!
//! ## Batch-wake threshold (reap-fanin campaign, 2026-08-08)
//!
//! [`CqeDoorbell::park_begin_batch`] extends the park with a wake MARK:
//! the daemon elides wakes toward a parked reaper until the completion
//! seq REACHES the earliest registered mark (`wake_at`, wrapping order)
//! — one wake syscall per K completions instead of one per completion
//! (the flat park's measured wake herd at fan-in: 17.8 M wakes / 22.2 M
//! ops, +43 µs daemon inflight at 32×8), while the reaper's bounded
//! wait (the caller-owned age bound — §5.3.1 rule 5) caps the
//! observation latency at exactly the blind-sleep posture it replaces.
//! `park_begin` ≡ `park_begin_batch(1)` — the shipped k=1 semantics are
//! bit-identical (first post-snapshot completion wakes).
//!
//! Why the threshold can never strand an ADMITTED sleeper (given the
//! caller's liveness cap `k ≤ its own pending population`): admission
//! (`seq == snapshot`) certifies ZERO completions since the snapshot,
//! so all `k` marks lie in the future and each below-mark completion is
//! survivable — the k-th pays the wake. A completion that lands between
//! snapshot and wait FAILS the admission (EAGAIN → re-scan); one that
//! lands before the registration is found by the mandatory post-
//! registration scan. Concurrent parkers compose by EARLIEST mark
//! (wrapping-min relative to the parker's own snapshot); the one benign
//! race — a first-parker's unconditional store overwriting a racing
//! second-parker's earlier min — can only DELAY that parker's wake to
//! its own bounded timeout, never lose it (documented at the store).
//! Weakening evidence re-verified 2026-08-08 with the batch model
//! (`ipc_cqe_batch_parked_reaper_never_stranded`).
//!
//! ## Trust boundary (§5.3.1)
//!
//! Both words are client-writable shm. A hostile client scribbling
//! `parked` to a huge value makes the daemon pay one wake syscall per
//! completion — exactly the pre-campaign posture, bounded, self-harm
//! only. Scribbling `seq` confuses only its own reaper (its parks stop
//! admitting or expire on their §5.3.1-rule-5 bounds). The daemon never
//! waits on either word — it only bumps/loads.
//!
//! Dependency-free on purpose: `loom-models/src/lib.rs` `#[path]`-includes
//! this file (the `slot_core` house convention), so the models check the
//! shipped protocol, not a copy. The main build never sets `cfg(loom)`.
//!
//! [`SlotCore::complete`]: crate::slot_core::SlotCore::complete

#[cfg(loom)]
use loom::sync::atomic::{fence, AtomicU32, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{fence, AtomicU32, Ordering};

/// The completion-direction wake words. `#[repr(C)]` because production
/// embeds this at a fixed offset inside the session header (KD-7
/// version-locked, never a stable ABI). One cache line with the seq on
/// purpose (the PERF-18 falsification: co-locate what the hot writer
/// must read every op; the protocol keeps the OTHER writers sparse).
#[repr(C)]
#[derive(Debug)]
pub struct CqeDoorbell {
    /// Session completion sequence: bumped once per completion. The
    /// futex word parked reapers sleep on.
    seq: AtomicU32,
    /// Parked-reaper count: `park_begin[_batch]` increments, `park_end`
    /// decrements. Nonzero gates the daemon's wake syscall.
    parked: AtomicU32,
    /// Batch-wake mark (reap-fanin 2026-08-08): the earliest ABSOLUTE
    /// completion seq any currently-parked reaper wants its wake at.
    /// Written by parkers (sparse — once per park), read by the daemon
    /// only while `parked != 0`. Client-writable shm like its siblings:
    /// scribbling it forward delays only the scribbler's own reaper
    /// (bounded by its §5.3.1-rule-5 wait bound); scribbling it behind
    /// restores the wake-per-completion posture — bounded self-harm.
    wake_at: AtomicU32,
    /// Wake-collapse latch (wake-economy campaign 2026-08-14, the
    /// former `_pad` — struct stays 16 bytes, same header line): a
    /// per-PARK-ERA once-flag. The daemon's mark-passed completion
    /// CASes 0→1 — the winner pays the era's ONE `FUTEX_WAKE`
    /// (breadth `i32::MAX`), losers collapse the syscall (the shipped
    /// protocol re-paid on EVERY mark-passed completion until re-park —
    /// the counted 0.94 wakes/op fan-in term). Cleared by
    /// `park_begin[_batch]` AFTER the parked registration + fence and
    /// BEFORE the seq snapshot (the ordering the loom strand models
    /// pin); deliberately NOT cleared at `park_end` — a successor-less
    /// era must not bequeath `wake_paid = 1` to the next parker
    /// (`latch_new_era_is_payable`). Client-writable shm like its
    /// siblings: scribbling 1 suppresses only the scribbler's own
    /// reaper's wakes (bounded by its §5.3.1-rule-5 wait bound);
    /// scribbling 0 restores wake-per-completion — bounded self-harm.
    wake_paid: AtomicU32,
}

/// Daemon-side completion outcome (replaces the former bool): `Wake` =
/// pay the `FUTEX_WAKE` syscall; `Collapsed` = a parked reaper is
/// mark-passed but this era's wake is already paid/in flight (the latch
/// elision — counted apart from `Elided` because it is the campaign's
/// engagement instrument); `Elided` = no parked reaper, or every parked
/// mark still ahead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompleteOutcome {
    Elided,
    Collapsed,
    Wake,
}

impl Default for CqeDoorbell {
    fn default() -> Self {
        Self::new()
    }
}

impl CqeDoorbell {
    pub fn new() -> Self {
        Self {
            seq: AtomicU32::new(0),
            parked: AtomicU32::new(0),
            wake_at: AtomicU32::new(0),
            wake_paid: AtomicU32::new(0),
        }
    }

    /// Daemon, after the slot's DONE publish: bump the completion seq;
    /// [`CompleteOutcome::Wake`] = a parked reaper needs a `FUTEX_WAKE`
    /// on [`Self::seq_word`] (breadth `i32::MAX` — split
    /// submitter/reaper pairs may both park). `Elided` = no one is
    /// parked (a concurrent parker's post-registration scan or failed
    /// admission covers it — module docs), or every parked reaper's
    /// batch mark is still ahead (its own admission proof makes each
    /// below-mark completion survivable — §batch-wake threshold).
    ///
    /// `latch`: the wake-collapse arm (wake-economy 2026-08-14). This
    /// file is dependency-free (loom `#[path]`-includes it — module
    /// docs), so it can never read an env knob: the caller resolves
    /// `SQUEEZEFS_IPC_CQE_WAKE_LATCH` once and passes it. `false` = the
    /// shipped wake-per-mark-passed body verbatim (`Wake` on every
    /// mark-passed completion — the pre-campaign posture, the A/B
    /// control). `true` = mark-passed completions CAS the per-era
    /// `wake_paid` latch: the winner pays the era's one syscall,
    /// losers return `Collapsed`. Strand-freedom under the latch is
    /// the loom models' `ipc_cqe_latched_*` obligation (era-scoped
    /// witnesses — module docs on the clear ordering).
    pub fn complete(&self, latch: bool) -> CompleteOutcome {
        // Store→load across two locations (the Dekker shape): the
        // explicit fence is LOAD-BEARING, not belt-and-braces — it is
        // what makes this side's bump globally ordered against the
        // reaper's registration (the W1 §5.1 house pattern; loom's
        // SeqCst-atomics modeling is weaker than the C++ total order,
        // and the `ipc_cqe_*` models fail without it).
        let seq = self.seq.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        fence(Ordering::SeqCst);
        if self.parked.load(Ordering::SeqCst) == 0 {
            return CompleteOutcome::Elided;
        }
        // Batch gate: wake once the bumped seq has REACHED the earliest
        // parked mark (wrapping order — `at ≤ seq`). A stale mark left
        // behind the seq (previous park era, hostile scribble) reads as
        // reached and over-wakes: the pre-campaign posture, never a
        // strand.
        let at = self.wake_at.load(Ordering::SeqCst);
        if seq.wrapping_sub(at) >= (1 << 31) {
            return CompleteOutcome::Elided;
        }
        if !latch {
            // Shipped posture, verbatim: every mark-passed completion
            // pays (the A/B control arm — bit-identical to the retired
            // `return true`).
            return CompleteOutcome::Wake;
        }
        match self
            .wake_paid
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
        {
            // This era's ONE syscall.
            Ok(_) => CompleteOutcome::Wake,
            // A wake for this era is already paid/in flight: elide the
            // syscall (the fan-in collapse — the campaign's lever).
            Err(_) => CompleteOutcome::Collapsed,
        }
    }

    /// Client reaper: register parked intent and snapshot the expected
    /// seq — in that order (registration first is load-bearing: the
    /// daemon's `parked` gate must observe intent no later than the
    /// snapshot the wait admits against). Follow with the pending-set
    /// re-scan, then `FUTEX_WAIT(seq_word, returned snapshot)`.
    /// Shipped semantics preserved verbatim: k = 1 ⇒ the first
    /// post-snapshot completion pays the wake.
    pub fn park_begin(&self) -> u32 {
        self.park_begin_batch(1)
    }

    /// Client reaper, batch form (reap-fanin 2026-08-08): register
    /// parked intent, snapshot the expected seq, and mark the wake at
    /// `snapshot + k` — the daemon elides wakes toward this park until
    /// the k-th post-snapshot completion. LIVENESS IS THE CALLER'S HALF
    /// OF THE CONTRACT: `k` must not exceed the caller's own pending
    /// population on this session (an admitted sleeper needs k more
    /// completions to exist), and every wait on the returned snapshot
    /// must stay deadline-bounded (§5.3.1 rule 5 — the bound is also
    /// what caps the one benign mark-overwrite race below).
    pub fn park_begin_batch(&self, k: u32) -> u32 {
        let k = k.max(1);
        // The mirrored Dekker side — same load-bearing fence (see
        // [`Self::complete`]).
        let prior = self.parked.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        // Wake-collapse era start (2026-08-14): clear the latch AFTER
        // the registration + fence and BEFORE the snapshot (a completer
        // whose bump follows the snapshot also follows this clear, so
        // the era's first mark-passed completion always finds a payable
        // latch — the strand argument's case 2). Honest weakening
        // ledger (2026-08-14 runs): moving the clear BEFORE the
        // registration passes the models — the design's prediction (a)
        // was FALSIFIED, because a completer's parked-gate runs before
        // its CAS, so a pre-registration clear can never be consumed by
        // an eliding completer; the position is kept for the documented
        // era definition, not by counterexample. DELETING the clear is
        // the real teeth (`latch_new_era_is_payable` — a paid era would
        // poison every successor); the two Dekker fence drops fail two
        // models each (re-verified on the latched body). A completer racing the
        // clear→mark window below reads the STALE previous mark
        // (at-or-behind the seq ⇒ reads-as-reached) against the
        // freshly cleared latch and over-pays one wake — benign,
        // bounded, the same class as the racy `wake_at` store
        // documented at the first-parker arm.
        self.wake_paid.store(0, Ordering::SeqCst);
        let snap = self.seq.load(Ordering::SeqCst);
        let target = snap.wrapping_add(k);
        if prior == 0 {
            // First parker of this era: the mark is OURS to set. A
            // racing second parker that squeezed its min in between our
            // fetch_add and this store gets overwritten — its wake may
            // arrive as late as OUR mark or its own bounded timeout,
            // never lost (its admission/scan cover every completion
            // before its sleep; module docs).
            self.wake_at.store(target, Ordering::SeqCst);
        } else {
            // Compose by EARLIEST mark, wrapping-relative to our own
            // snapshot: a mark already behind the seq (stale era) reads
            // as huge and is replaced; a live earlier mark is kept.
            let _ = self
                .wake_at
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
                    if cur.wrapping_sub(snap) <= target.wrapping_sub(snap) {
                        None
                    } else {
                        Some(target)
                    }
                });
        }
        snap
    }

    /// Client reaper: deregister after the wait returns (any reason).
    pub fn park_end(&self) {
        self.parked.fetch_sub(1, Ordering::SeqCst);
    }

    /// The completion-seq futex word (cross-process — never
    /// `FUTEX_PRIVATE`).
    pub fn seq_word(&self) -> &AtomicU32 {
        &self.seq
    }

    /// Current completion seq (diagnostics / model assertions).
    pub fn seq(&self) -> u32 {
        self.seq.load(Ordering::SeqCst)
    }

    /// Current parked-reaper count (diagnostics / model assertions).
    pub fn parked(&self) -> u32 {
        self.parked.load(Ordering::SeqCst)
    }

    /// Current batch-wake mark (diagnostics / model assertions).
    pub fn wake_at(&self) -> u32 {
        self.wake_at.load(Ordering::SeqCst)
    }

    /// Current wake-collapse latch state (diagnostics / model
    /// assertions — the loom strand models' payable-era check).
    pub fn wake_paid(&self) -> u32 {
        self.wake_paid.load(Ordering::SeqCst)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// The latch=false parity arm — these walks pin the SHIPPED
    /// wake-per-mark-passed posture verbatim (the A/B control);
    /// the latch arm's walks live below.
    fn wakes(d: &CqeDoorbell) -> bool {
        matches!(d.complete(false), CompleteOutcome::Wake)
    }

    /// Sequential protocol walk: completions toward an unparked reaper
    /// elide; a parked reaper gates the wake on; park_end releases it.
    #[test]
    fn wake_gated_on_parked_count() {
        let d = CqeDoorbell::new();
        assert!(!wakes(&d), "no reaper parked: elide the wake");
        assert!(!wakes(&d), "still elided");
        let expected = d.park_begin();
        assert_eq!(expected, 2, "snapshot reads the completions so far");
        assert!(wakes(&d), "parked reaper must be woken");
        assert_ne!(d.seq(), expected, "the bump fails the wait's admission");
        d.park_end();
        assert!(!wakes(&d), "after park_end the wake elides again");
    }

    /// Batch threshold walk (reap-fanin 2026-08-08): a k=3 park elides
    /// the first two completions and pays the wake at the third; past
    /// the mark the wake stays on until re-park (the woken reaper
    /// deregisters and re-marks — bounded over-wake window, never a
    /// strand).
    #[test]
    fn batch_park_wakes_at_kth_completion() {
        let d = CqeDoorbell::new();
        let snap = d.park_begin_batch(3);
        assert_eq!(snap, 0, "snapshot reads the completions so far");
        assert!(!wakes(&d), "1st completion below the mark: elide");
        assert!(!wakes(&d), "2nd completion below the mark: elide");
        assert!(wakes(&d), "3rd completion reaches the mark: wake");
        assert!(wakes(&d), "past the mark stays woken until re-park");
        d.park_end();
        assert!(!wakes(&d), "unparked: elide again");
        // Re-park after the era: a fresh mark, counted from the new
        // snapshot (the stale-mark state never leaks into a new park).
        let snap2 = d.park_begin_batch(2);
        assert_eq!(snap2, 5, "five completions so far (incl. the elided one)");
        assert!(!wakes(&d), "fresh era: 1st below the new mark");
        assert!(wakes(&d), "fresh era: 2nd reaches it");
        d.park_end();
    }

    /// `park_begin` keeps its shipped k=1 semantics bit-for-bit: the
    /// first post-snapshot completion pays the wake (the sparse-regime
    /// contract every existing caller relies on).
    #[test]
    fn park_begin_is_batch_one() {
        let d = CqeDoorbell::new();
        let _ = d.park_begin();
        assert!(wakes(&d), "k=1: first completion wakes");
        d.park_end();
    }

    /// Concurrent parkers compose by EARLIEST mark — including when the
    /// later parker's snapshot has advanced past the earlier parker's
    /// (the wrapping-min is relative to the LIVE snapshot, so a stale
    /// behind-the-seq mark is replaced, never inherited).
    #[test]
    fn earliest_mark_governs_across_parkers() {
        let d = CqeDoorbell::new();
        let _s1 = d.park_begin_batch(8); // mark 8
        let _s2 = d.park_begin_batch(2); // mark 2 — earlier, must win
        assert!(!wakes(&d), "1st below both marks: elide");
        assert!(
            wakes(&d),
            "2nd reaches the earlier mark: wake (breadth covers both)"
        );
        d.park_end();
        d.park_end();

        // Later-mark replacement: A parks k=8 at seq 3 (mark 11); two
        // elided completions later B parks k=2 at seq 5 (mark 7) — the
        // fetch_update must adopt 7 (11 is later than 7 relative to B's
        // snapshot), so completion 7 lands the wake.
        assert!(!wakes(&d), "seq 3: unparked interlude, elide");
        let _a = d.park_begin_batch(8); // snap 3, mark 11
        assert!(!wakes(&d), "seq 4: below 11");
        assert!(!wakes(&d), "seq 5: below 11");
        let _b = d.park_begin_batch(2); // snap 5, mark 7 — earlier, wins
        assert!(!wakes(&d), "seq 6: below 7");
        assert!(wakes(&d), "seq 7: B's mark reached");
        d.park_end();
        d.park_end();
    }

    /// Two parkers (split submitter/reaper — a legal libaio shape): the
    /// wake stays on until BOTH deregister.
    #[test]
    fn wake_covers_every_parker() {
        let d = CqeDoorbell::new();
        let _e1 = d.park_begin();
        let _e2 = d.park_begin();
        assert_eq!(d.parked(), 2);
        assert!(wakes(&d));
        d.park_end();
        assert!(wakes(&d), "second parker still needs the wake");
        d.park_end();
        assert!(!wakes(&d));
    }

    // -- The wake-collapse latch arm (wake-economy 2026-08-14) ----------

    /// The collapse walk: under the latch a park era pays exactly ONE
    /// wake — the first mark-passed completion CAS-wins, every later
    /// one collapses (the shipped posture re-paid all three).
    #[test]
    fn latch_collapses_an_era_to_one_wake() {
        let d = CqeDoorbell::new();
        let _e = d.park_begin();
        assert_eq!(d.complete(true), CompleteOutcome::Wake, "era's one syscall");
        assert_eq!(d.complete(true), CompleteOutcome::Collapsed);
        assert_eq!(d.complete(true), CompleteOutcome::Collapsed);
        d.park_end();
        assert_eq!(
            d.complete(true),
            CompleteOutcome::Elided,
            "unparked completions elide regardless of the latch"
        );
    }

    /// The new-era law (loom weakening (b)'s unit half): a fresh
    /// `park_begin` re-arms the latch — the clear lives at era START,
    /// never at `park_end`, so a successor-less era cannot bequeath
    /// `wake_paid = 1` to the next parker.
    #[test]
    fn latch_new_era_is_payable() {
        let d = CqeDoorbell::new();
        let _e = d.park_begin();
        assert_eq!(d.complete(true), CompleteOutcome::Wake);
        assert_eq!(d.complete(true), CompleteOutcome::Collapsed);
        d.park_end();
        // The era ended with the latch SET and no successor cleared it
        // yet — the next park must start payable.
        let _e2 = d.park_begin();
        assert_eq!(
            d.complete(true),
            CompleteOutcome::Wake,
            "a new park era's first mark-passed completion must pay, \
             not inherit the previous era's paid latch"
        );
        d.park_end();
    }

    /// qd1 inertness (the wake-IS-the-contract shape, G3 hard gate):
    /// under the latch a k=1 park's FIRST completion still pays
    /// immediately — the latch changes nothing before the first wake.
    #[test]
    fn latch_first_completion_still_pays_immediately() {
        let d = CqeDoorbell::new();
        let _e = d.park_begin();
        assert_eq!(
            d.complete(true),
            CompleteOutcome::Wake,
            "qd1 RTT: the first post-park completion pays, latch or not"
        );
        d.park_end();
    }

    /// latch=false bit-parity: the control arm walks identically to the
    /// retired bool body — every mark-passed completion pays, below-mark
    /// and unparked completions elide (the batch walk re-run on the
    /// enum, outcomes named).
    #[test]
    fn latch_false_is_the_shipped_posture_verbatim() {
        let d = CqeDoorbell::new();
        assert_eq!(d.complete(false), CompleteOutcome::Elided, "unparked");
        let _s = d.park_begin_batch(3);
        assert_eq!(d.complete(false), CompleteOutcome::Elided, "below mark");
        assert_eq!(d.complete(false), CompleteOutcome::Elided, "below mark");
        assert_eq!(d.complete(false), CompleteOutcome::Wake, "mark reached");
        assert_eq!(
            d.complete(false),
            CompleteOutcome::Wake,
            "past the mark stays woken until re-park — the shipped
             re-pay posture, never Collapsed on the control arm"
        );
        d.park_end();
        assert_eq!(d.complete(false), CompleteOutcome::Elided, "unparked");
    }

    /// The latch composes with the batch mark: below-mark completions
    /// elide (no CAS — the latch is untouched), the k-th pays, the
    /// (k+1)-th collapses.
    #[test]
    fn latch_composes_with_the_batch_mark() {
        let d = CqeDoorbell::new();
        let _s = d.park_begin_batch(2);
        assert_eq!(d.complete(true), CompleteOutcome::Elided, "below mark");
        assert_eq!(d.complete(true), CompleteOutcome::Wake, "k-th pays");
        assert_eq!(d.complete(true), CompleteOutcome::Collapsed, "collapsed");
        d.park_end();
    }
}
