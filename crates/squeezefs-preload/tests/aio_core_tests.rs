//! v1.1 libaio interposition — the **mixed-batch core** red matrix
//! (design-preload-interception.md OQ-1; the "mixed-batch
//! completion-merge surface" the design named as THE cost of libaio
//! support, so it gets a hermetic state machine with injected lanes).
//!
//! Contracts pinned here (libaio / POSIX AIO semantics, exactly):
//!
//! - **`io_submit` is prefix-atomic**: the return value N means iocbs
//!   [0, N) were submitted and [N, ..) were NOT. A mixed batch may
//!   interleave ring-eligible and kernel iocbs; whatever lane an op
//!   takes, the prefix law holds — a kernel sub-run rejecting at its
//!   j-th element truncates the batch there, and ring ops AFTER the
//!   truncation point must never have been dispatched (two-phase or
//!   lazy dispatch — the test observes dispatch order).
//! - **First-op failure returns the errno**, later-op failure returns
//!   the count (libaio convention).
//! - **`io_getevents` merges lanes**: ring completions materialize as
//!   `io_event`s carrying the ORIGINAL iocb pointer and its `data`
//!   cookie, `res` = bytes-or-negative-errno; kernel events pass
//!   through untouched; `min_nr` counts ACROSS lanes (ring-first
//!   harvest, then kernel top-up); `min_nr = 0` never blocks.
//! - **Ring-only waits still respect `min_nr`** (poll-slice loop —
//!   the kernel lane has nothing pending and must not be block-waited
//!   for the full timeout while ring completions sit ready).
//! - **`io_destroy` with in-flight ring ops** abandons them cleanly
//!   (slots are GC'd with the session — §5.7): no hang, no event
//!   delivery after destroy.

use squeezefs_il::aio_core::{
    AioCtxState, AioEvent, IocbClass, KernelLane, RingLane, RingToken, SubmitOutcome,
};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

// ---------------------------------------------------------------------------
// fake lanes (scripted; record dispatch order)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeRing {
    /// scripted per-submit outcomes (pop-front); default = accept
    refusals: VecDeque<bool>, // true = refuse (no slot)
    submitted: Vec<u64>, // iocb ids in dispatch order
    /// completions ready to harvest: (token, result)
    ready: VecDeque<(RingToken, i64)>,
    next_tok: u64,
    inflight: Vec<(RingToken, u64)>, // (token, iocb id)
}

impl RingLane for FakeRing {
    fn try_submit(&mut self, iocb_id: u64) -> Option<RingToken> {
        if self.refusals.pop_front().unwrap_or(false) {
            return None;
        }
        let tok = RingToken(self.next_tok);
        self.next_tok += 1;
        self.submitted.push(iocb_id);
        self.inflight.push((tok, iocb_id));
        Some(tok)
    }

    fn poll(&mut self, tok: RingToken) -> Option<i64> {
        if let Some(pos) = self.ready.iter().position(|(t, _)| *t == tok) {
            let (_, res) = self.ready.remove(pos).unwrap();
            return Some(res);
        }
        None
    }

    fn abandon(&mut self, tok: RingToken) {
        self.inflight.retain(|(t, _)| *t != tok);
    }
}

#[derive(Default)]
struct FakeKernel {
    /// scripted per-RUN acceptance counts (pop-front); default = all
    accepts: VecDeque<isize>,
    submitted_runs: Vec<Vec<u64>>,
    /// scripted events for getevents calls (pop-front per call)
    events: VecDeque<Vec<AioEvent>>,
    getevents_calls: RefCell<Vec<(usize, usize)>>, // (min, max) observed
}

impl KernelLane for FakeKernel {
    fn submit_run(&mut self, iocb_ids: &[u64]) -> isize {
        let n = self.accepts.pop_front().unwrap_or(iocb_ids.len() as isize);
        let taken = if n < 0 { 0 } else { n as usize };
        self.submitted_runs
            .push(iocb_ids[..taken.min(iocb_ids.len())].to_vec());
        n
    }

    fn getevents(&mut self, min: usize, max: usize, _timeout_ms: Option<u64>) -> Vec<AioEvent> {
        self.getevents_calls.borrow_mut().push((min, max));
        self.events.pop_front().unwrap_or_default()
    }
}

fn ev(iocb_id: u64, res: i64) -> AioEvent {
    AioEvent {
        iocb_id,
        data: iocb_id * 100, // cookie convention for the tests
        res,
        res2: 0,
    }
}

/// classes[i] applies to iocb id i (ids are indices for readability)
fn submit(
    st: &mut AioCtxState,
    ring: &mut FakeRing,
    kern: &mut FakeKernel,
    classes: &[IocbClass],
) -> SubmitOutcome {
    let ids: Vec<u64> = (0..classes.len() as u64).collect();
    st.submit_batch(ring, kern, &ids, classes, &|id| id * 100)
}

// ---------------------------------------------------------------------------
// prefix-atomic mixed submission
// ---------------------------------------------------------------------------

#[test]
fn mixed_batch_full_acceptance_dispatches_in_order() {
    let mut st = AioCtxState::new();
    let (mut ring, mut kern) = (FakeRing::default(), FakeKernel::default());
    use IocbClass::*;
    let out = submit(
        &mut st,
        &mut ring,
        &mut kern,
        &[Ring, Kernel, Kernel, Ring, Kernel],
    );
    assert_eq!(out, SubmitOutcome::Submitted(5));
    assert_eq!(
        ring.submitted,
        vec![0, 3],
        "ring lane got its iocbs in order"
    );
    assert_eq!(
        kern.submitted_runs,
        vec![vec![1, 2], vec![4]],
        "kernel iocbs submitted as CONTIGUOUS runs preserving batch order"
    );
    assert_eq!(st.ring_pending(), 2);
}

#[test]
fn kernel_partial_acceptance_truncates_the_prefix_before_later_ring_ops() {
    let mut st = AioCtxState::new();
    let (mut ring, mut kern) = (FakeRing::default(), FakeKernel::default());
    kern.accepts.push_back(1); // the [1,2] run accepts only iocb 1
    use IocbClass::*;
    let out = submit(
        &mut st,
        &mut ring,
        &mut kern,
        &[Ring, Kernel, Kernel, Ring, Ring],
    );
    // Prefix = iocb0 (ring) + iocb1 (kernel) = 2; iocbs 2..5 NOT submitted.
    assert_eq!(out, SubmitOutcome::Submitted(2));
    assert_eq!(
        ring.submitted,
        vec![0],
        "ring ops AFTER the kernel truncation point must never dispatch"
    );
    assert_eq!(st.ring_pending(), 1);
}

#[test]
fn ring_refusal_reroutes_to_the_kernel_lane_never_eagain() {
    // The 2026-07-25 ipc-miss-path design-board item: session slot
    // exhaustion (try_submit = None) is NOT an error — the iocb is a
    // bound-fd pread/pwrite whose REAL kernel call is always correct
    // (§5.4.2 fallback-is-correctness). It reclassifies to the kernel
    // lane AT ITS BATCH POSITION (prefix order preserved: it joins the
    // open kernel run, which any later ring op flushes first).
    let mut st = AioCtxState::new();
    let (mut ring, mut kern) = (FakeRing::default(), FakeKernel::default());
    ring.refusals.push_back(false);
    ring.refusals.push_back(true); // second ring op refused (no slot)
    use IocbClass::*;
    let out = submit(&mut st, &mut ring, &mut kern, &[Ring, Ring, Kernel]);
    assert_eq!(
        out,
        SubmitOutcome::Submitted(3),
        "a mid-batch ring refusal must reroute that iocb to the kernel \
         lane, not end the prefix"
    );
    assert_eq!(
        ring.submitted,
        vec![0],
        "only the accepted ring op rode the ring"
    );
    assert_eq!(
        kern.submitted_runs,
        vec![vec![1, 2]],
        "the refused ring op joins the kernel run at its batch position"
    );
    assert_eq!(st.ring_pending(), 1);
    assert_eq!(
        st.kernel_pending(),
        2,
        "rerouted op counts as kernel-pending"
    );

    // First-op refusal: the kernel lane takes it too — no EAGAIN.
    let mut st2 = AioCtxState::new();
    let (mut ring2, mut kern2) = (FakeRing::default(), FakeKernel::default());
    ring2.refusals.push_back(true);
    let out2 = submit(&mut st2, &mut ring2, &mut kern2, &[Ring, Kernel]);
    assert_eq!(
        out2,
        SubmitOutcome::Submitted(2),
        "first-op slot exhaustion must NOT surface -EAGAIN — the kernel \
         lane serves the op"
    );
    assert!(ring2.submitted.is_empty());
    assert_eq!(kern2.submitted_runs, vec![vec![0, 1]]);

    // Ordering law under reroute: a LATER ring op still flushes the run
    // (which now contains the rerouted op) before itself dispatching.
    let mut st3 = AioCtxState::new();
    let (mut ring3, mut kern3) = (FakeRing::default(), FakeKernel::default());
    ring3.refusals.push_back(true); // first ring op refused
    ring3.refusals.push_back(false); // second accepted
    let out3 = submit(&mut st3, &mut ring3, &mut kern3, &[Ring, Ring]);
    assert_eq!(out3, SubmitOutcome::Submitted(2));
    assert_eq!(
        kern3.submitted_runs,
        vec![vec![0]],
        "the rerouted op's run flushed BEFORE the later ring dispatch"
    );
    assert_eq!(ring3.submitted, vec![1]);
}

#[test]
fn kernel_first_op_errno_propagates_when_batch_starts_kernel() {
    let mut st = AioCtxState::new();
    let (mut ring, mut kern) = (FakeRing::default(), FakeKernel::default());
    kern.accepts.push_back(-(libc::EINVAL as isize)); // whole first run refused
    use IocbClass::*;
    let out = submit(&mut st, &mut ring, &mut kern, &[Kernel, Ring]);
    assert_eq!(out, SubmitOutcome::Errno(libc::EINVAL));
    assert!(
        ring.submitted.is_empty(),
        "nothing after the failed first run"
    );
}

// ---------------------------------------------------------------------------
// completion merge
// ---------------------------------------------------------------------------

#[test]
fn getevents_merges_ring_first_then_kernel_with_min_across_lanes() {
    let mut st = AioCtxState::new();
    let (mut ring, mut kern) = (FakeRing::default(), FakeKernel::default());
    use IocbClass::*;
    submit(&mut st, &mut ring, &mut kern, &[Ring, Kernel, Ring]);

    // Ring op for iocb 2 completes (4096 bytes); kernel has iocb 1 ready.
    let tok2 = ring.inflight.iter().find(|(_, id)| *id == 2).unwrap().0;
    ring.ready.push_back((tok2, 4096));
    kern.events.push_back(vec![ev(1, 512)]);

    let events = st.getevents(&mut ring, &mut kern, 2, 8, Some(1000));
    assert_eq!(events.len(), 2);
    assert_eq!(
        (events[0].iocb_id, events[0].data, events[0].res),
        (2, 200, 4096),
        "ring completion carries the ORIGINAL iocb identity + data cookie + byte result"
    );
    assert_eq!((events[1].iocb_id, events[1].res), (1, 512));
    assert_eq!(st.ring_pending(), 1, "iocb 0 still in flight");
    // The kernel wait's min was reduced by the ring harvest (2 wanted,
    // 1 ring-served ⇒ kernel min 1).
    assert_eq!(kern.getevents_calls.borrow().last(), Some(&(1, 7)));
}

#[test]
fn ring_error_completion_surfaces_negative_errno_in_res() {
    let mut st = AioCtxState::new();
    let (mut ring, mut kern) = (FakeRing::default(), FakeKernel::default());
    submit(&mut st, &mut ring, &mut kern, &[IocbClass::Ring]);
    let tok = ring.inflight[0].0;
    ring.ready.push_back((tok, -(libc::EBADF as i64)));
    let events = st.getevents(&mut ring, &mut kern, 1, 4, Some(100));
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].res, -(libc::EBADF as i64));
}

#[test]
fn min_zero_never_blocks_and_returns_whatever_is_ready() {
    let mut st = AioCtxState::new();
    let (mut ring, mut kern) = (FakeRing::default(), FakeKernel::default());
    submit(&mut st, &mut ring, &mut kern, &[IocbClass::Ring]);
    // Nothing ready anywhere.
    let events = st.getevents(&mut ring, &mut kern, 0, 4, Some(0));
    assert!(events.is_empty());
    // Kernel lane must have been asked with min 0 (non-blocking probe)
    // or not at all — never a blocking min.
    for (min, _) in kern.getevents_calls.borrow().iter() {
        assert_eq!(
            *min, 0,
            "min_nr=0 must never translate into a blocking kernel wait"
        );
    }
}

#[test]
fn ring_only_pending_waits_by_poll_slices_not_kernel_blocking() {
    let mut st = AioCtxState::new();
    let (mut ring, mut kern) = (FakeRing::default(), FakeKernel::default());
    submit(&mut st, &mut ring, &mut kern, &[IocbClass::Ring]);
    let tok = ring.inflight[0].0;
    // The completion appears only after the first poll pass — the merge
    // loop must re-poll the ring rather than park the full timeout on a
    // kernel lane that has NOTHING pending.
    ring.ready.push_back((tok, 4096));
    let events = st.getevents(&mut ring, &mut kern, 1, 4, Some(5000));
    assert_eq!(events.len(), 1);
    for (min, _) in kern.getevents_calls.borrow().iter() {
        assert_eq!(
            *min, 0,
            "with zero kernel-pending ops, kernel waits must be non-blocking probes"
        );
    }
}

#[test]
fn destroy_abandons_ring_pendings_without_delivering() {
    let mut st = AioCtxState::new();
    let (mut ring, mut kern) = (FakeRing::default(), FakeKernel::default());
    submit(
        &mut st,
        &mut ring,
        &mut kern,
        &[IocbClass::Ring, IocbClass::Ring],
    );
    assert_eq!(st.ring_pending(), 2);
    st.destroy(&mut ring);
    assert_eq!(st.ring_pending(), 0);
    assert!(
        ring.inflight.is_empty(),
        "destroy must abandon (not consume) in-flight ring slots — session GC owns them"
    );
    let events = st.getevents(&mut ring, &mut kern, 0, 8, Some(0));
    assert!(events.is_empty(), "no delivery after destroy");
}

// ---------------------------------------------------------------------------
// classification is an allow-list
// ---------------------------------------------------------------------------

#[test]
fn only_pread_pwrite_on_bound_fds_classify_ring() {
    use squeezefs_il::aio_core::classify_iocb;
    // (opcode, bound, read_ok, write_ok) — mirrors the §5.1 v1 surface:
    // IOCB_CMD_PREAD=0, PWRITE=1, FSYNC=2, FDSYNC=3, POLL=5, NOOP=6,
    // PREADV=7, PWRITEV=8.
    assert_eq!(classify_iocb(0, true, true, true), IocbClass::Ring);
    assert_eq!(classify_iocb(1, true, true, true), IocbClass::Ring);
    assert_eq!(
        classify_iocb(0, false, true, true),
        IocbClass::Kernel,
        "unbound fd"
    );
    assert_eq!(
        classify_iocb(0, true, false, true),
        IocbClass::Kernel,
        "no read right"
    );
    assert_eq!(
        classify_iocb(1, true, true, false),
        IocbClass::Kernel,
        "no write right"
    );
    for op in [2u16, 3, 5, 6, 7, 8, 99] {
        assert_eq!(
            classify_iocb(op, true, true, true),
            IocbClass::Kernel,
            "opcode {op} must passthrough (allow-list: PREAD/PWRITE only in v1.1)"
        );
    }
}

// Silence unused-helper warnings when individual tests are filtered.
#[allow(dead_code)]
fn _use(_: Rc<()>) {}
