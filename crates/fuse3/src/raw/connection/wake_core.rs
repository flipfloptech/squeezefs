//! FUSE-over-io_uring queue-worker **wake coalescing** protocol core
//! (L3 transport-economy lever B).
//!
//! One [`WakeCoalescer`] per (queue eventfd). Reply submitters
//! (`FuseOverUring::submit_reply`) and payload-lease drops
//! (`EntPayloadLease::drop`) wake the queue worker through the queue
//! eventfd. Without coalescing that is one `write(2)` per reply — 1.67
//! eventfd writes/op measured on the charter workload
//! (`.benchmarks/2026-07-15-iops-parity-decomposition.md`) — even though
//! the worker drains its commit channel in batches: N wakes between two
//! worker passes buy exactly one drain.
//!
//! The protocol makes N producer wakes between worker passes cost ≤ 1
//! eventfd write:
//!
//! - **Producer** (any thread): publish the observable state FIRST (channel
//!   send / lease-refs release), then [`WakeCoalescer::arm`] — write the
//!   eventfd only when it returns `true`.
//! - **Worker** (queue thread, once per pass): drain the eventfd to EAGAIN,
//!   then [`WakeCoalescer::disarm`], then scan every producer-observable
//!   state (commit channel, parked leases, shutdown flag).
//!
//! ## Why no wake is ever lost
//!
//! Lost wakes are unacceptable (a stranded reply = a permanent kernel
//! `waiting ≥ 1`, umount EBUSY); extra wakes are harmless (the eventfd is
//! level-triggered — the worker's PollAdd completes the moment it is armed
//! while the counter is nonzero, and the drain loops until EAGAIN).
//!
//! Both operations are `SeqCst` RMWs on the one flag, so they form a single
//! modification order and each reads its immediate predecessor (release
//! sequence ⇒ happens-before chains through every earlier RMW):
//!
//! 1. `arm() == true` (flag was clear): the producer writes the eventfd.
//!    Either some worker pass consumes that write — and that pass's `disarm`
//!    (this producer's `arm` precedes the consuming pass's `disarm` in the
//!    flag order only if… it does not matter: the write itself keeps the
//!    counter nonzero until a drain, and any drain's pass scans producers
//!    AFTER its `disarm`) — or no pass consumes it and the counter stays
//!    nonzero, so the worker cannot park (PollAdd completes on arm).
//! 2. `arm() == false` (flag already set): walk the flag's modification
//!    order backwards — the flag reads `true`, `disarm` only writes
//!    `false`, so there is an unbroken chain of `arm`s back to one that
//!    returned `true`; call its producer S. S wrote the eventfd. If S's
//!    write is never consumed the counter is nonzero and the worker cannot
//!    park. If a worker pass consumed it, that pass's eventfd read happens
//!    after S's write (it read S's count), its `disarm` follows in program
//!    order, and NO `disarm` sits between S's `arm` and our `arm` (the
//!    chain is unbroken) — so that pass's `disarm` follows our `arm` in
//!    the flag's modification order. The RMW chain makes our pre-`arm`
//!    state publication happen-before that `disarm`, which is sequenced
//!    before the pass's producer scan: the scan observes our state.
//!
//! Either way: state published ⇒ some worker scan observes it, or the
//! eventfd counter is nonzero and the worker wakes again. The
//! `wake_coalescer_*` loom models in `loom-models` check this shipped code
//! (including composed with the `lease_core` parked-commit handoff);
//! `disarm` weakened to a plain store — or the drain/disarm/scan order
//! permuted — fails them.
//!
//! Dependency-free on purpose: `loom-models/src/lib.rs` `#[path]`-includes
//! this file (the `lease_core` house convention), so the models check the
//! real protocol, not a copy. The main build never sets `cfg(loom)`.

#[cfg(loom)]
use loom::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicBool, Ordering};

/// Per-queue wake-elision flag. See the module docs for the protocol and
/// the loss-freedom argument.
#[derive(Debug, Default)]
pub struct WakeCoalescer {
    /// A wake is armed: some producer has written (or is about to write)
    /// the eventfd and the worker has not started a drain pass since.
    armed: AtomicBool,
}

impl WakeCoalescer {
    pub fn new() -> Self {
        Self {
            armed: AtomicBool::new(false),
        }
    }

    /// Producer side. Call AFTER publishing the state the worker must
    /// observe (channel send, lease-refs release, …). Returns `true` when
    /// the caller must write the queue eventfd; `false` when an armed wake
    /// already covers this publication (elided write).
    pub fn arm(&self) -> bool {
        !self.armed.swap(true, Ordering::SeqCst)
    }

    /// Worker side, once per pass: call AFTER draining the eventfd to
    /// EAGAIN and BEFORE scanning any producer-observable state. An RMW
    /// (`swap`, not a plain store) on purpose — reading the predecessor in
    /// the flag's modification order is what carries the happens-before
    /// edge from every covered producer's publication into this pass's
    /// scans (module docs, case 2).
    pub fn disarm(&self) {
        self.armed.swap(false, Ordering::SeqCst);
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// Sequential protocol walk: first arm wins the write, later arms are
    /// elided until the worker disarms, then the next arm wins again.
    #[test]
    fn first_arm_writes_rest_elide_until_disarm() {
        let c = WakeCoalescer::new();
        assert!(c.arm(), "first arm after creation must write the eventfd");
        assert!(!c.arm(), "second arm rides the first write (elided)");
        assert!(!c.arm(), "every further arm elides too");
        c.disarm();
        assert!(c.arm(), "first arm after a worker pass must write again");
        assert!(!c.arm());
    }

    /// disarm on a clear flag is harmless (worker passes triggered by CQE
    /// traffic, not wakes, disarm too).
    #[test]
    fn disarm_without_arm_is_harmless() {
        let c = WakeCoalescer::new();
        c.disarm();
        assert!(c.arm(), "arm after a spurious disarm still writes");
    }
}
