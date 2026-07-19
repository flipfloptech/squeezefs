//! IPC **op-slot** protocol core — the completion-in-place state machine
//! (design-preload-interception §5.3: the SQE and CQE in one).
//!
//! One [`SlotCore`] per op slot, living in the shared session mapping. The
//! full slot ([`crate::layout::IpcSlot`]) is this core's two protocol words
//! plus the descriptor fields; this module owns every state transition:
//!
//! ```text
//!   FREE ─claim→ CLAIMED ─publish_submitted→ SUBMITTED ─try_begin_serve→
//!   SERVING ─complete→ DONE ─release→ FREE            (+ WAITER bit)
//! ```
//!
//! - **Client** (untrusted process): [`SlotCore::try_claim`] (which stamps
//!   the ABA **generation**), writes the descriptor, then
//!   [`SlotCore::publish_submitted`] and pushes the slot index into the
//!   submission ring. To wait it spins on [`SlotCore::is_done_for`] for a
//!   bounded window, then parks: [`SlotCore::park_prepare`] sets the WAITER
//!   bit and re-checks DONE in one RMW — the publish-then-recheck shape
//!   that closes the missed-wake race exactly like `lease_core`'s parked
//!   commit (§5.3 protocol rule 2).
//! - **Daemon** (trusted, single service thread per session): after popping
//!   the index from the ring, [`SlotCore::try_begin_serve`] — whose success
//!   is the Acquire edge that makes the **one** descriptor snapshot read
//!   valid (§5.3.1 rule 1: snapshot-then-validate, serve-from-snapshot;
//!   the descriptor is never re-read) — then serves, writes the result, and
//!   [`SlotCore::complete`]s: one `swap(DONE, AcqRel)` whose prior value
//!   says whether a parked waiter needs a futex wake.
//!
//! ## Generation (ABA) guard
//!
//! [`SlotCore::try_claim`] bumps the slot generation; every wait/consume
//! check carries the generation returned by its own claim. A waiter from a
//! previous life of the slot (stale futex artifact, hostile replay) can
//! never mistake a recycled slot's DONE for its own: by the time a new
//! DONE is observable (Acquire on the state word), the new claim's
//! generation bump is visible too ([`SlotCore::is_done_for`] orders the
//! loads exactly that way).
//!
//! ## Untrusted shared memory
//!
//! The client can write these words arbitrarily. Every daemon-side
//! transition is a CAS/swap that tolerates any observed value (impossible
//! transitions fail the CAS or trip the caller's validation — the daemon
//! poisons the session loudly per §5.3 rule 4); nothing here can make the
//! daemon dereference client data or block unboundedly (parks are the
//! caller's business and bounded by §5.3.1 rule 5).
//!
//! Dependency-free on purpose: `loom-models/src/lib.rs` `#[path]`-includes
//! this file (the `wake_core` house convention), so the models check the
//! shipped protocol, not a copy. The main build never sets `cfg(loom)`.

#[cfg(loom)]
use loom::sync::atomic::{AtomicU32, AtomicU64, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Slot states (low byte of the state word).
pub const STATE_FREE: u32 = 0;
pub const STATE_CLAIMED: u32 = 1;
pub const STATE_SUBMITTED: u32 = 2;
pub const STATE_SERVING: u32 = 3;
pub const STATE_DONE: u32 = 4;

/// WAITER bit: a client is parked (or about to park) on this state word via
/// futex; [`SlotCore::complete`]'s swap reports it so the service thread
/// issues exactly the wakes that are needed (0 at saturation — the client
/// spins its bounded window instead).
pub const WAITER: u32 = 1 << 8;

const STATE_MASK: u32 = 0xff;

/// Extract the state bits (without the WAITER bit) of a raw state word.
#[inline]
pub fn state_bits(word: u32) -> u32 {
    word & STATE_MASK
}

/// Outcome of [`SlotCore::park_prepare`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkOutcome {
    /// DONE was already published — consume immediately, do NOT park.
    Ready,
    /// Park: `FUTEX_WAIT` on the state word with this expected value (the
    /// word as observed with the WAITER bit set). A racing completion
    /// changes the word, so the wait either returns EAGAIN (re-check) or a
    /// wake arrives ([`SlotCore::complete`] saw the WAITER bit).
    Park { expected: u32 },
}

/// The two protocol words of one op slot. `#[repr(C)]` because production
/// embeds this at a fixed offset inside the 128-byte shm slot
/// ([`crate::layout::IpcSlot`]); the layout is version-locked to the build
/// commit (design KD-7), never a stable ABI.
#[repr(C)]
#[derive(Debug)]
pub struct SlotCore {
    state: AtomicU32,
    _pad: u32,
    generation: AtomicU64,
}

impl Default for SlotCore {
    fn default() -> Self {
        Self::new()
    }
}

impl SlotCore {
    /// A fresh FREE slot at generation 0.
    pub fn new() -> Self {
        Self {
            state: AtomicU32::new(STATE_FREE),
            _pad: 0,
            generation: AtomicU64::new(0),
        }
    }

    /// Client: claim a FREE slot. On success the slot is CLAIMED, its
    /// generation is bumped, and the new generation is returned — carry it
    /// into every subsequent wait/consume check on this op. `None` = slot
    /// not FREE (caller scans on — bounded scan + free-list hint live in
    /// the client library, not here).
    pub fn try_claim(&self) -> Option<u64> {
        // Acquire pairs with `release`'s Release store: the new claimant's
        // descriptor writes are ordered after the previous consumer's
        // result read (slot reuse never tears across lives).
        self.state
            .compare_exchange(
                STATE_FREE,
                STATE_CLAIMED,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .ok()?;
        // Exclusive claimant bumps the generation. Relaxed is sound: the
        // bump is sequenced before `publish_submitted`'s Release store,
        // and every DONE observation Acquire-loads a state word in that
        // store's release sequence — so anyone who sees this life's DONE
        // sees this generation (the `ipc_slot_core` loom model checks it).
        Some(self.generation.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// Client: publish the filled descriptor — CLAIMED → SUBMITTED
    /// (Release; pairs with [`Self::try_begin_serve`]'s Acquire so the
    /// daemon's snapshot sees every descriptor write).
    pub fn publish_submitted(&self) {
        debug_assert_eq!(
            state_bits(self.state.load(Ordering::Relaxed)),
            STATE_CLAIMED,
            "publish_submitted outside CLAIMED"
        );
        // WAITER cannot be set here in the honest protocol (a client only
        // waits on an op it has submitted), so a plain store is exact.
        self.state.store(STATE_SUBMITTED, Ordering::Release);
    }

    /// Daemon: SUBMITTED → SERVING (WAITER bit preserved — a client may
    /// have parked, or be parking, before the serve even starts). `false` =
    /// the slot is not SUBMITTED (hostile/duplicate ring entry — the caller
    /// treats it as a protocol violation per §5.3 rule 4, never a panic).
    /// Success is the Acquire edge licensing the ONE descriptor snapshot
    /// read (§5.3.1 rule 1).
    pub fn try_begin_serve(&self) -> bool {
        let mut cur = self.state.load(Ordering::Relaxed);
        loop {
            if state_bits(cur) != STATE_SUBMITTED {
                return false;
            }
            // Preserve WAITER: a client may already be parked (or parking)
            // on this word. Acquire on success pairs with
            // `publish_submitted`'s Release — the descriptor snapshot that
            // follows sees every client write.
            match self.state.compare_exchange_weak(
                cur,
                STATE_SERVING | (cur & WAITER),
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Daemon: SERVING → DONE after the result is written into the slot.
    /// One `swap(AcqRel)`; returns `true` when the prior word carried the
    /// WAITER bit — the caller must then `FUTEX_WAKE` the state word.
    /// (The swap also clears the WAITER bit: a woken client re-checks via
    /// [`Self::is_done_for`], never via the bit.)
    pub fn complete(&self) -> bool {
        // AcqRel: the Release half publishes the result write (and, via
        // the release sequence headed at `publish_submitted`, the claim's
        // generation bump); the Acquire half orders the WAITER decision.
        let prior = self.state.swap(STATE_DONE, Ordering::AcqRel);
        debug_assert_eq!(
            state_bits(prior),
            STATE_SERVING,
            "complete() outside SERVING"
        );
        prior & WAITER != 0
    }

    /// Anyone: is this slot DONE for generation `gen`? The legit waiter's
    /// poll (spin window + post-wake re-check) and the stale-waiter guard
    /// in one: orders the state load (Acquire) before the generation load,
    /// so a recycled slot's DONE is never attributed to an old generation.
    pub fn is_done_for(&self, gen: u64) -> bool {
        // State FIRST (Acquire — synchronizes with `complete`'s Release
        // half), generation second: once this life's DONE is visible, so
        // is this life's generation bump (it happens-before the DONE
        // publication), so a stale generation can never match. Relaxed on
        // the generation load is sound under that happens-before — the
        // overwritten old generation is unreadable here.
        if state_bits(self.state.load(Ordering::Acquire)) != STATE_DONE {
            return false;
        }
        self.generation.load(Ordering::Relaxed) == gen
    }

    /// Client: two-phase park entry — set WAITER and re-check DONE in one
    /// RMW (publish-then-recheck; §5.3 protocol rule 2). [`ParkOutcome::
    /// Ready`] means DONE already published: consume, do not park.
    pub fn park_prepare(&self) -> ParkOutcome {
        // One RMW does both phases: publish the WAITER bit AND re-check
        // DONE. Either this lands before `complete`'s swap (which then
        // reads the bit and wakes) or after it (prior reads DONE ⇒ Ready).
        // There is no third interleaving — that is the whole protocol.
        // AcqRel: the Acquire half makes a Ready outcome license the
        // result read immediately.
        let prior = self.state.fetch_or(WAITER, Ordering::AcqRel);
        if state_bits(prior) == STATE_DONE {
            ParkOutcome::Ready
        } else {
            ParkOutcome::Park {
                expected: prior | WAITER,
            }
        }
    }

    /// Client: DONE → FREE after consuming the result (Release; pairs with
    /// the next [`Self::try_claim`]'s Acquire so slot reuse never observes
    /// the previous op's stores out of order).
    pub fn release(&self) {
        debug_assert_eq!(
            state_bits(self.state.load(Ordering::Relaxed)),
            STATE_DONE,
            "release() outside DONE"
        );
        // Release pairs with the next `try_claim`'s Acquire (see there).
        // Clears a late parker's WAITER bit with the rest of the word.
        self.state.store(STATE_FREE, Ordering::Release);
    }

    /// Current generation (diagnostics / model assertions).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Raw state word (diagnostics / model assertions / futex plumbing).
    pub fn raw_state(&self) -> u32 {
        self.state.load(Ordering::Acquire)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn full_cycle_walk_and_generation_stamps() {
        let s = SlotCore::new();
        assert_eq!(s.raw_state(), STATE_FREE);
        assert_eq!(s.generation(), 0);

        let gen = s.try_claim().expect("fresh slot must claim");
        assert_eq!(gen, 1, "first claim stamps generation 1");
        assert_eq!(state_bits(s.raw_state()), STATE_CLAIMED);
        assert!(s.try_claim().is_none(), "claimed slot must refuse a claim");

        s.publish_submitted();
        assert_eq!(state_bits(s.raw_state()), STATE_SUBMITTED);
        assert!(!s.is_done_for(gen), "submitted is not done");

        assert!(s.try_begin_serve(), "submitted slot must begin serve");
        assert_eq!(state_bits(s.raw_state()), STATE_SERVING);
        assert!(!s.try_begin_serve(), "double begin_serve must refuse");

        let need_wake = s.complete();
        assert!(!need_wake, "no waiter was ever parked");
        assert!(s.is_done_for(gen), "completed op must be done for its gen");
        assert!(!s.is_done_for(gen + 1), "wrong generation must not be done");

        s.release();
        assert_eq!(s.raw_state(), STATE_FREE);

        let gen2 = s.try_claim().expect("released slot must re-claim");
        assert_eq!(gen2, 2, "generations are monotonic per slot");
    }

    #[test]
    fn begin_serve_refuses_every_non_submitted_state() {
        let s = SlotCore::new();
        assert!(!s.try_begin_serve(), "FREE must refuse serve");
        s.try_claim().unwrap();
        assert!(!s.try_begin_serve(), "CLAIMED must refuse serve");
        s.publish_submitted();
        assert!(s.try_begin_serve());
        s.complete();
        assert!(!s.try_begin_serve(), "DONE must refuse serve");
        s.release();
        assert!(!s.try_begin_serve(), "FREE (recycled) must refuse serve");
    }

    #[test]
    fn waiter_bit_preserved_across_begin_serve_and_reported_by_complete() {
        let s = SlotCore::new();
        let gen = s.try_claim().unwrap();
        s.publish_submitted();

        // Client parks before the daemon even dequeues.
        match s.park_prepare() {
            ParkOutcome::Park { expected } => {
                assert_eq!(state_bits(expected), STATE_SUBMITTED);
                assert_eq!(expected & WAITER, WAITER, "park must set WAITER");
            }
            ParkOutcome::Ready => panic!("nothing is done yet — must park"),
        }

        assert!(s.try_begin_serve(), "serve must start despite WAITER");
        assert_eq!(
            s.raw_state() & WAITER,
            WAITER,
            "begin_serve must preserve the WAITER bit"
        );
        assert!(s.complete(), "complete must report the parked waiter");
        assert_eq!(s.raw_state() & WAITER, 0, "complete clears WAITER");
        assert!(s.is_done_for(gen));
        s.release();
    }

    #[test]
    fn park_prepare_after_done_is_ready_not_stranded() {
        let s = SlotCore::new();
        let gen = s.try_claim().unwrap();
        s.publish_submitted();
        assert!(s.try_begin_serve());
        s.complete();
        // The client lost the race: DONE published before it parked.
        assert_eq!(
            s.park_prepare(),
            ParkOutcome::Ready,
            "publish-then-recheck: a post-DONE park attempt must consume, not sleep"
        );
        assert!(s.is_done_for(gen));
        s.release();
    }

    #[test]
    fn stale_generation_never_sees_recycled_done() {
        let s = SlotCore::new();
        let gen1 = s.try_claim().unwrap();
        s.publish_submitted();
        assert!(s.try_begin_serve());
        s.complete();
        assert!(s.is_done_for(gen1));
        s.release();

        // Second life of the slot.
        let gen2 = s.try_claim().unwrap();
        assert_ne!(gen1, gen2);
        s.publish_submitted();
        assert!(s.try_begin_serve());
        s.complete();
        assert!(
            !s.is_done_for(gen1),
            "gen-1 waiter must never consume gen-2's DONE (ABA guard)"
        );
        assert!(s.is_done_for(gen2));
        s.release();
    }

    /// Real-atomics stress: one slot recycled through many claim →
    /// submit → serve → complete → consume → release cycles with the
    /// consumer racing the server; exactly one consume per generation.
    #[test]
    fn cycle_stress_exactly_once_per_generation() {
        use std::sync::Arc;

        const CYCLES: u64 = 10_000;
        let slot = Arc::new(SlotCore::new());

        let server = {
            let slot = Arc::clone(&slot);
            std::thread::spawn(move || {
                let mut served = 0u64;
                while served < CYCLES {
                    if slot.try_begin_serve() {
                        slot.complete();
                        served += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            })
        };

        for i in 1..=CYCLES {
            let gen = loop {
                if let Some(g) = slot.try_claim() {
                    break g;
                }
                std::hint::spin_loop();
            };
            assert_eq!(gen, i, "generations monotonic under recycling");
            slot.publish_submitted();
            let mut consumed = 0u32;
            loop {
                if slot.is_done_for(gen) {
                    consumed += 1;
                    break;
                }
                std::hint::spin_loop();
            }
            assert_eq!(consumed, 1);
            slot.release();
        }
        server.join().unwrap();
    }
}
