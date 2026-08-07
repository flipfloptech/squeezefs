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
    _pad: u32,
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
            _pad: 0,
        }
    }

    /// Daemon, after the slot's DONE publish: bump the completion seq;
    /// returns `true` when a parked reaper needs a `FUTEX_WAKE` on
    /// [`Self::seq_word`] (breadth `i32::MAX` — split submitter/reaper
    /// pairs may both park). `false` = elide the syscall: no one is
    /// parked (a concurrent parker's post-registration scan or failed
    /// admission covers it — module docs), or every parked reaper's
    /// batch mark is still ahead (its own admission proof makes each
    /// below-mark completion survivable — §batch-wake threshold).
    pub fn complete(&self) -> bool {
        // Store→load across two locations (the Dekker shape): the
        // explicit fence is LOAD-BEARING, not belt-and-braces — it is
        // what makes this side's bump globally ordered against the
        // reaper's registration (the W1 §5.1 house pattern; loom's
        // SeqCst-atomics modeling is weaker than the C++ total order,
        // and the `ipc_cqe_*` models fail without it).
        let seq = self.seq.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        fence(Ordering::SeqCst);
        if self.parked.load(Ordering::SeqCst) == 0 {
            return false;
        }
        // Batch gate: wake once the bumped seq has REACHED the earliest
        // parked mark (wrapping order — `at ≤ seq`). A stale mark left
        // behind the seq (previous park era, hostile scribble) reads as
        // reached and over-wakes: the pre-campaign posture, never a
        // strand.
        let at = self.wake_at.load(Ordering::SeqCst);
        seq.wrapping_sub(at) < (1 << 31)
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
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// Sequential protocol walk: completions toward an unparked reaper
    /// elide; a parked reaper gates the wake on; park_end releases it.
    #[test]
    fn wake_gated_on_parked_count() {
        let d = CqeDoorbell::new();
        assert!(!d.complete(), "no reaper parked: elide the wake");
        assert!(!d.complete(), "still elided");
        let expected = d.park_begin();
        assert_eq!(expected, 2, "snapshot reads the completions so far");
        assert!(d.complete(), "parked reaper must be woken");
        assert_ne!(d.seq(), expected, "the bump fails the wait's admission");
        d.park_end();
        assert!(!d.complete(), "after park_end the wake elides again");
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
        assert!(!d.complete(), "1st completion below the mark: elide");
        assert!(!d.complete(), "2nd completion below the mark: elide");
        assert!(d.complete(), "3rd completion reaches the mark: wake");
        assert!(d.complete(), "past the mark stays woken until re-park");
        d.park_end();
        assert!(!d.complete(), "unparked: elide again");
        // Re-park after the era: a fresh mark, counted from the new
        // snapshot (the stale-mark state never leaks into a new park).
        let snap2 = d.park_begin_batch(2);
        assert_eq!(snap2, 5, "five completions so far (incl. the elided one)");
        assert!(!d.complete(), "fresh era: 1st below the new mark");
        assert!(d.complete(), "fresh era: 2nd reaches it");
        d.park_end();
    }

    /// `park_begin` keeps its shipped k=1 semantics bit-for-bit: the
    /// first post-snapshot completion pays the wake (the sparse-regime
    /// contract every existing caller relies on).
    #[test]
    fn park_begin_is_batch_one() {
        let d = CqeDoorbell::new();
        let _ = d.park_begin();
        assert!(d.complete(), "k=1: first completion wakes");
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
        assert!(!d.complete(), "1st below both marks: elide");
        assert!(
            d.complete(),
            "2nd reaches the earlier mark: wake (breadth covers both)"
        );
        d.park_end();
        d.park_end();

        // Later-mark replacement: A parks k=8 at seq 3 (mark 11); two
        // elided completions later B parks k=2 at seq 5 (mark 7) — the
        // fetch_update must adopt 7 (11 is later than 7 relative to B's
        // snapshot), so completion 7 lands the wake.
        assert!(!d.complete(), "seq 3: unparked interlude, elide");
        let _a = d.park_begin_batch(8); // snap 3, mark 11
        assert!(!d.complete(), "seq 4: below 11");
        assert!(!d.complete(), "seq 5: below 11");
        let _b = d.park_begin_batch(2); // snap 5, mark 7 — earlier, wins
        assert!(!d.complete(), "seq 6: below 7");
        assert!(d.complete(), "seq 7: B's mark reached");
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
        assert!(d.complete());
        d.park_end();
        assert!(d.complete(), "second parker still needs the wake");
        d.park_end();
        assert!(!d.complete());
    }
}
