//! **READ fast-dispatch from the reap thread** (e2e perf audit R-2, read
//! board #2 — `.benchmarks/2026-09-03-4k-random-attribution.md` §4/§7).
//!
//! # The term this kills
//!
//! A fetched FUSE_READ used to travel reap thread → shared inbound queue
//! (channel push + eventfd wake) → the per-queue session dispatch task
//! (parked on the channel, polled on a `fuse3-tpc` lane) → classical
//! framing reconstruction → `handle_read` → a same-lane spawn → the
//! handler's first poll. The attribution pass measured that ingress at
//! `queue_wait` 69 µs + `dispatch_lag` 89 µs = 158 µs of a 439 µs 4 KiB
//! random read (36 %), with the device inside the op at 40 µs. The
//! `read_ingress` bench prices the unloaded mechanism at 6.7 µs (shipped
//! hop) vs 4.0 µs (direct lane mint) vs 53 ns (inline serve); the loaded
//! term is the queueing behind the hop.
//!
//! # The mechanism
//!
//! The queue worker dispatches a READ itself, at the delivery CQE:
//!
//! 1. **Probe inline** — [`ReadFastDispatch::probe`] runs the filesystem's
//!    SYNC, try-only warm ladder (`Filesystem::read_fast_probe`: per-inode
//!    `try_read()`, active-buffer snapshot, staging ring, hot tier,
//!    read-lane hold, NVMe read-cache — the il §5.5.1 sync fast path's
//!    exact legs) against the ent's own payload window.
//! 2. **Serve + commit inline** — a served probe becomes a `CommitMsg`
//!    routed through the worker's own `commit_ready_reply` in the same
//!    pass: no channel, no wake, no lane. `queue_wait == dispatch_lag ==
//!    0` by construction ([`record_served`] records the zeros so the
//!    phase counts keep closing against the READ population);
//!    `transport_fast_dispatch_serves` counts it.
//! 3. **Demote to a lane** — a miss (or any would-block: a writer holding
//!    the inode lock, an overlay, a multi-block shape) mints the FULL
//!    READ handler future ([`ReadFastDispatch::mint`]) and hands it to a
//!    `fuse3-tpc` lane DIRECTLY (`tpc_spawn_on_node` — the handoff-economy
//!    venue, never a runtime-handle spawn), skipping the inbound queue and
//!    the session dispatch task. `queue_wait` is recorded 0 at the mint;
//!    `dispatch_lag` is the lane hop alone. `transport_fast_dispatch_demotes`
//!    counts it.
//!
//! The reap thread never blocks: the probe is try-only (a contended lock
//! is a demote, pinned by the root suite), and a cold read's device work
//! stays on the lanes exactly as before. The §5.4 lease law is untouched
//! (READ deliveries carry no payload lease; the inline commit runs the
//! same lease-gated `commit_ready_reply` every reply runs), and the
//! commit-batch drain + wake coalescing are unchanged — the inline commit
//! rides the pass-bottom flush like every other SQE.
//!
//! `SQUEEZEFS_FUSE_READ_FAST_DISPATCH=0` is the A/B control (the
//! pre-campaign inbound-queue path, byte-identical).
//!
//! # The composed READ dispatch law (R-2 ⊕ R-3, 2026-09-03)
//!
//! R-3 (`.benchmarks/2026-09-03-r3-fill-issue-economy.md`) named the
//! DEMOTED cold READ's next term: around one 40 µs DMA the zc direct leg
//! paid three cross-thread wakes (handler lane → worker, CQE → worker,
//! worker → handler lane), 35–47 µs each under the field's CPU load. The
//! two levers compose into ONE law at the delivery CQE, in this order:
//!
//! - **(a) served inline if warm** — step 1/2 above (R-2);
//! - **(b) else fused onto the queue worker's lane** if the session is
//!   zc-armed and `fuse_read_in.size` is at or under the fusion ceiling
//!   (`SQUEEZEFS_FUSE_ZC_READ_FUSION`, default on — the D16 fused lane;
//!   the SAME [`ReadFastDispatch::mint`] future, polled by the reaping
//!   worker: its fetch message, its CQE resume and its prefilled COMMIT
//!   are same-thread; `fuse3_zc_read_fusions` counts it);
//! - **(c) else handed to the lane homed on the queue's CPU** — step 3
//!   above (`transport_fast_dispatch_demotes` counts it; an eligible READ
//!   the fused lane refused at capacity also counts
//!   `fuse3_zc_read_fusion_demotions`).
//!
//! **Partition law:** `serves + zc_read_fusions + demotes ≡` the READs
//! delivered on an armed session with the lever on — every READ takes
//! exactly one arm, and `zc_read_fusions + demotes ≡ fuse3_read_inplace_
//! replies` (a served READ never takes the handler's in-place arm). The
//! reap thread never blocks on any arm. The instruments coexist without
//! double-counting a stage: `transport_reap_gap_ns` brackets the worker's
//! enters; `read_transport_phase_ns` records `queue_wait ≡ 0` on (b) and
//! (c) with `dispatch_lag` = arrival → the handler's first poll (the run-
//! queue wait on (b), the lane hop on (c)); `zc_bridge_phase_ns` splits
//! the handler's `block_fetch` on the zc leg (`Σ hops ≡ total`), and its
//! `wake_hop` on (b) is the fused pass's run-queue wait, not a thread hop.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::fused::FusedFuture;
use super::InboundUringReq;
use crate::raw::abi::fuse_opcode;
use crate::raw::read_phase::{read_transport_phase_record, transport_instant, TransportPhase};
use crate::raw::reply::FastReadProbe;

/// The sync READ probe's shape: `(nodeid, fh, offset, size, flags, dest)
/// → probe` — `Filesystem::read_fast_probe` behind the filesystem's
/// `Arc`. `dest` names this request's reply window (bounce slot on zc
/// sessions) so a tier serve can land its bytes there directly.
pub type ReadFastProbeFn =
    dyn Fn(u64, u64, u64, u32, u32, Option<(u64, usize)>) -> FastReadProbe + Send + Sync + 'static;

/// The registered fast-dispatch pair (the `FusedWriteDispatch` shape):
/// `probe` is the filesystem's sync warm ladder for one READ delivery,
/// `mint` builds the full READ handler future for a demoted delivery.
pub struct ReadFastDispatch {
    /// Called on the queue-worker thread once per READ delivery: SYNC,
    /// try-only, non-blocking (`Filesystem::read_fast_probe`'s contract).
    pub probe: Arc<ReadFastProbeFn>,
    /// The READ handler future for one delivery (parse + body — the
    /// dispatch loop's `handle_read` as one future).
    pub mint: Arc<dyn Fn(InboundUringReq) -> FusedFuture + Send + Sync + 'static>,
}

/// The pure eligibility core (pinned by unit tests): a delivery takes the
/// fast-dispatch path iff it is a FUSE_READ, the lever is on, and the
/// session has registered a dispatcher. Every other opcode — and every
/// READ before registration or with the lever off — rides the inbound
/// queue exactly as before.
pub fn fast_dispatch_candidate(opcode: u32, enabled: bool, registered: bool) -> bool {
    opcode == fuse_opcode::FUSE_READ as u32 && enabled && registered
}

/// The lever (`SQUEEZEFS_FUSE_READ_FAST_DISPATCH`, default ON; `0` = the
/// A/B control — every READ rides the inbound queue as before). Read
/// once per process (the daemon's startup registry gate already refused
/// a malformed value; absent keeps the default).
pub fn fast_dispatch_enabled() -> bool {
    static CELL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CELL.get_or_init(|| {
        crate::env_knob_core::parse_bool(
            "SQUEEZEFS_FUSE_READ_FAST_DISPATCH",
            std::env::var("SQUEEZEFS_FUSE_READ_FAST_DISPATCH")
                .ok()
                .as_deref(),
        )
        .ok()
        .flatten()
        .unwrap_or(true)
    })
}

static FAST_DISPATCH_SERVES: AtomicU64 = AtomicU64::new(0);
static FAST_DISPATCH_DEMOTES: AtomicU64 = AtomicU64::new(0);

/// Count one READ the probe declined — minted on the reap thread and
/// handed straight to a handler lane.
#[inline]
pub fn note_fast_dispatch_demote() {
    FAST_DISPATCH_DEMOTES.fetch_add(1, Ordering::Relaxed);
}

/// Account one READ served + committed inline on the reap thread:
/// the engagement counter, the transport phases (`queue_wait` and
/// `dispatch_lag` as EXACT zeros — the served op paid neither, and the
/// phase counts keep closing against the READ population;
/// `transport_total` = `arrived → committed`), and the op-trace stamps
/// (`transport_recv` + `fast_dispatch` at the arrival instant, the stitch
/// aliases the latter for `dispatch`/`handler_entry`; `reply_commit` at
/// `committed_ns`). Both stamps are transport-epoch ns the caller already
/// read — no clock read here.
#[inline]
pub fn record_served(unique: u64, arrived_ns: u64, committed_ns: u64) {
    FAST_DISPATCH_SERVES.fetch_add(1, Ordering::Relaxed);
    read_transport_phase_record(TransportPhase::QueueWait, std::time::Duration::ZERO);
    read_transport_phase_record(TransportPhase::DispatchLag, std::time::Duration::ZERO);
    read_transport_phase_record(
        TransportPhase::TransportTotal,
        std::time::Duration::from_nanos(committed_ns.saturating_sub(arrived_ns)),
    );
    let traced = crate::raw::op_trace::traced(unique);
    if traced != 0 {
        let at = transport_instant(arrived_ns);
        crate::raw::op_trace::stamp(traced, crate::raw::op_trace::Stage::TransportRecv, at);
        crate::raw::op_trace::stamp(traced, crate::raw::op_trace::Stage::FastDispatch, at);
        crate::raw::op_trace::stamp(
            traced,
            crate::raw::op_trace::Stage::ReplyCommit,
            transport_instant(committed_ns),
        );
    }
}

/// The transport-epoch clock [`record_served`]'s stamps are read on
/// (test/bench seam — the daemon's suites pin the accounting against
/// stamps they minted themselves).
#[doc(hidden)]
pub fn now_ns() -> u64 {
    crate::raw::read_phase::transport_now_ns()
}

/// `transport_fast_dispatch_serves` (stats inode): on a warm row this
/// must account ≈ every warm READ; 0 on `SQUEEZEFS_FUSE_READ_FAST_DISPATCH=0`
/// mounts and before the session registers the dispatcher.
pub fn fast_dispatch_serves() -> u64 {
    FAST_DISPATCH_SERVES.load(Ordering::Relaxed)
}

/// `transport_fast_dispatch_demotes` (stats inode): READs the inline
/// probe declined, dispatched from the reap thread straight onto a lane
/// (the inbound queue + session dispatch task skipped). `serves +
/// demotes` ≡ the READs delivered on an armed session with the lever on.
pub fn fast_dispatch_demotes() -> u64 {
    FAST_DISPATCH_DEMOTES.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::read_phase::{read_transport_phase_snapshot, transport_now_ns};

    /// The tables + trace pool are process-global; delta tests serialize
    /// with the read_phase suite through their own lock (distinct
    /// statics, so a shared lock is not available — keep the deltas
    /// tight and single-threaded here).
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn candidate_is_read_and_lever_and_registration() {
        let read = fuse_opcode::FUSE_READ as u32;
        assert!(fast_dispatch_candidate(read, true, true));
        assert!(!fast_dispatch_candidate(read, false, true), "lever off");
        assert!(
            !fast_dispatch_candidate(read, true, false),
            "no dispatcher yet"
        );
        assert!(
            !fast_dispatch_candidate(fuse_opcode::FUSE_WRITE as u32, true, true),
            "WRITE keeps the fused/inbound path"
        );
        assert!(
            !fast_dispatch_candidate(fuse_opcode::FUSE_LOOKUP as u32, true, true),
            "metadata ops keep the inbound path"
        );
    }

    /// A served op moves `serves` by one and records exact zeros on
    /// `queue_wait`/`dispatch_lag` (count +1, sum +0) with
    /// `transport_total` = the exact arrival→commit span.
    #[test]
    fn record_served_accounts_zero_ingress_and_the_exact_total() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let words = || {
            let s = read_transport_phase_snapshot();
            let qi = TransportPhase::QueueWait as usize;
            let di = TransportPhase::DispatchLag as usize;
            let ti = TransportPhase::TransportTotal as usize;
            [
                (s[qi].count, s[qi].sum_ns),
                (s[di].count, s[di].sum_ns),
                (s[ti].count, s[ti].sum_ns),
            ]
        };
        let s0 = fast_dispatch_serves();
        let [q0, d0, t0] = words();
        let arrived = transport_now_ns();
        record_served(0xABCD, arrived, arrived + 4_321);
        let [q1, d1, t1] = words();
        assert_eq!(fast_dispatch_serves() - s0, 1);
        assert_eq!(
            (q1.0 - q0.0, q1.1 - q0.1),
            (1, 0),
            "queue_wait: one exact zero"
        );
        assert_eq!(
            (d1.0 - d0.0, d1.1 - d0.1),
            (1, 0),
            "dispatch_lag: one exact zero"
        );
        assert_eq!(
            (t1.0 - t0.0, t1.1 - t0.1),
            (1, 4_321),
            "transport_total: exact span"
        );
    }
}
