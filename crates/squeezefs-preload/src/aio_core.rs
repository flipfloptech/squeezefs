//! v1.1 libaio interposition — the **mixed-batch core** (OQ-1): the
//! lane-split `io_submit` and lane-merge `io_getevents` state machine,
//! hermetic behind two small lane traits so its semantics are testable
//! without a kernel or a ring.
//!
//! The laws live in `tests/aio_core_tests.rs`; the short form:
//! `io_submit` is prefix-atomic across interleaved lanes (nothing after
//! the first refusal is ever dispatched), `io_getevents`' `min_nr`
//! counts across lanes with the ring harvested first, waits against a
//! kernel lane that has nothing pending are non-blocking probes inside
//! a poll-slice loop, and `io_destroy` abandons in-flight ring ops to
//! the session's slot GC (§5.7).

/// Opaque handle to one in-flight ring op (the interposer glue maps it
/// to a (session, slot, generation) triple; the fakes to an integer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingToken(pub u64);

/// One completion in libaio's shape (the glue converts to `io_event`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AioEvent {
    /// The ORIGINAL iocb identity (glue: the iocb pointer value).
    pub iocb_id: u64,
    /// The iocb's `data` cookie, returned verbatim.
    pub data: u64,
    /// Bytes transferred, or negative errno (libaio convention).
    pub res: i64,
    pub res2: i64,
}

/// Which lane an iocb takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IocbClass {
    /// Bound-fd PREAD/PWRITE with matching rights: the shm ring.
    Ring,
    /// Everything else: the real kernel context, untouched.
    Kernel,
}

/// `io_submit` outcome in libaio terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// iocbs `[0, n)` were submitted (the accepted prefix).
    Submitted(usize),
    /// The FIRST op failed: nothing submitted, return `-errno`.
    Errno(i32),
}

/// The ring side of one aio context, as the core sees it.
pub trait RingLane {
    /// Fire-and-forget submit of one ring-eligible iocb. `None` = no
    /// slot / not currently servable — the op (and the batch prefix)
    /// ends here; the caller may pass the iocb through instead on the
    /// NEXT submit (the binding stays).
    fn try_submit(&mut self, iocb_id: u64) -> Option<RingToken>;
    /// Non-blocking completion probe; `Some(res)` consumes the token.
    fn poll(&mut self, tok: RingToken) -> Option<i64>;
    /// Abandon an in-flight op (destroy path): the slot is never
    /// consumed by this context again; session GC owns it.
    fn abandon(&mut self, tok: RingToken);
}

/// The real-kernel side of one aio context.
pub trait KernelLane {
    /// Submit one CONTIGUOUS run; returns accepted count or `-errno`
    /// (exactly `io_submit(2)` semantics for that run).
    fn submit_run(&mut self, iocb_ids: &[u64]) -> isize;
    /// Harvest up to `max` kernel events, blocking for at least `min`
    /// (0 = non-blocking probe) within `timeout_ms` (`None` = forever).
    fn getevents(&mut self, min: usize, max: usize, timeout_ms: Option<u64>) -> Vec<AioEvent>;
}

/// Classify one iocb: the v1.1 allow-list — `IOCB_CMD_PREAD` (0) /
/// `IOCB_CMD_PWRITE` (1) on a bound fd whose rights cover the
/// direction. Vectored/fsync/poll/unknown opcodes pass through.
pub fn classify_iocb(opcode: u16, bound: bool, read_ok: bool, write_ok: bool) -> IocbClass {
    match (opcode, bound) {
        (0, true) if read_ok => IocbClass::Ring,
        (1, true) if write_ok => IocbClass::Ring,
        _ => IocbClass::Kernel,
    }
}

/// One in-flight ring op's bookkeeping.
struct PendingRing {
    tok: RingToken,
    iocb_id: u64,
    data: u64,
}

/// Per-context merge state. One per interposed `io_context_t`; the
/// interposer glue serializes access per context exactly as apps must
/// serialize libaio calls on one context (racing `io_submit` on one ctx
/// is app UB kernel-side too).
pub struct AioCtxState {
    pending: Vec<PendingRing>,
    /// Kernel-lane ops submitted and not yet delivered by `getevents`.
    /// Gates the blocking-vs-probe decision on the kernel wait: a
    /// blocking `min` is only ever issued while the kernel lane can
    /// actually complete it.
    kernel_pending: usize,
    destroyed: bool,
}

impl Default for AioCtxState {
    fn default() -> Self {
        Self::new()
    }
}

impl AioCtxState {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            kernel_pending: 0,
            destroyed: false,
        }
    }

    pub fn ring_pending(&self) -> usize {
        self.pending.len()
    }

    /// Kernel-lane ops submitted and not yet delivered.
    pub fn kernel_pending(&self) -> usize {
        self.kernel_pending
    }

    /// The lane-split `io_submit` walk. `classes[i]` classifies
    /// `iocb_ids[i]`; `data_of` supplies each iocb's completion cookie
    /// (captured AT SUBMIT — the client may recycle the iocb after the
    /// event fires, never before). Kernel iocbs are dispatched as the
    /// contiguous runs the batch order induces; the prefix law is
    /// enforced by dispatching each run AT the walk position where it
    /// closes, so nothing later ever moves first.
    pub fn submit_batch<R: RingLane, K: KernelLane>(
        &mut self,
        ring: &mut R,
        kern: &mut K,
        iocb_ids: &[u64],
        classes: &[IocbClass],
        data_of: &dyn Fn(u64) -> u64,
    ) -> SubmitOutcome {
        debug_assert_eq!(iocb_ids.len(), classes.len());
        let mut accepted = 0usize;
        let mut run: Vec<u64> = Vec::new();

        // Flush one pending kernel run; returns accepted-count-within-run
        // (also charged to `kernel_pending`) or an errno when the run's
        // FIRST op failed.
        let flush = |slf: &mut Self, kern: &mut K, run: &mut Vec<u64>| -> Result<usize, i32> {
            if run.is_empty() {
                return Ok(0);
            }
            let r = kern.submit_run(run);
            let n = if r < 0 {
                return Err((-r) as i32);
            } else {
                r as usize
            };
            run.clear();
            slf.kernel_pending += n;
            Ok(n)
        };

        for (i, id) in iocb_ids.iter().enumerate() {
            match classes[i] {
                IocbClass::Kernel => run.push(*id),
                IocbClass::Ring => {
                    // A ring op closes any open kernel run FIRST (order).
                    let run_len = run.len();
                    match flush(self, kern, &mut run) {
                        Ok(n) if n == run_len => accepted += n,
                        Ok(n) => {
                            // Partial kernel run: prefix ends inside it.
                            return SubmitOutcome::Submitted(accepted + n);
                        }
                        Err(e) => {
                            return if accepted == 0 {
                                SubmitOutcome::Errno(e)
                            } else {
                                SubmitOutcome::Submitted(accepted)
                            };
                        }
                    }
                    match ring.try_submit(*id) {
                        Some(tok) => {
                            self.pending.push(PendingRing {
                                tok,
                                iocb_id: *id,
                                data: data_of(*id),
                            });
                            accepted += 1;
                        }
                        None => {
                            // No slot: the prefix ends here (client-visible
                            // backpressure, §5.5.1 — EAGAIN if first).
                            return if accepted == 0 {
                                SubmitOutcome::Errno(libc::EAGAIN)
                            } else {
                                SubmitOutcome::Submitted(accepted)
                            };
                        }
                    }
                }
            }
        }
        let run_len = run.len();
        match flush(self, kern, &mut run) {
            Ok(n) if n == run_len => accepted += n,
            Ok(n) => return SubmitOutcome::Submitted(accepted + n),
            Err(e) => {
                return if accepted == 0 {
                    SubmitOutcome::Errno(e)
                } else {
                    SubmitOutcome::Submitted(accepted)
                };
            }
        }
        SubmitOutcome::Submitted(accepted)
    }

    /// Harvest every currently-complete ring op (up to `max` slots in
    /// `out`), consuming their tokens.
    fn harvest_ring<R: RingLane>(&mut self, ring: &mut R, out: &mut Vec<AioEvent>, max: usize) {
        let mut i = 0;
        while i < self.pending.len() && out.len() < max {
            if let Some(res) = ring.poll(self.pending[i].tok) {
                let p = self.pending.remove(i);
                out.push(AioEvent {
                    iocb_id: p.iocb_id,
                    data: p.data,
                    res,
                    res2: 0,
                });
            } else {
                i += 1;
            }
        }
    }

    /// The lane-merge `io_getevents`. Ring first, kernel top-up; the
    /// kernel wait's `min` is what remains of `min_nr` after the ring
    /// harvest and is issued as a BLOCKING wait only while kernel ops
    /// can actually complete it — with only ring ops pending the loop
    /// poll-slices (non-blocking kernel probes) so ring completions are
    /// picked up promptly and a dead kernel lane never eats the
    /// timeout.
    pub fn getevents<R: RingLane, K: KernelLane>(
        &mut self,
        ring: &mut R,
        kern: &mut K,
        min: usize,
        max: usize,
        timeout_ms: Option<u64>,
    ) -> Vec<AioEvent> {
        let mut out = Vec::new();
        if self.destroyed || max == 0 {
            return out;
        }
        self.harvest_ring(ring, &mut out, max);
        if out.len() >= min || out.len() >= max {
            if out.len() < max && self.kernel_pending > 0 {
                // Opportunistic non-blocking kernel drain.
                let got = kern.getevents(0, max - out.len(), Some(0));
                self.kernel_pending = self.kernel_pending.saturating_sub(got.len());
                out.extend(got);
            }
            return out;
        }
        // Bounded poll-slice loop (§5.3.1-rule-5 posture carried over:
        // every wait is deadline-bounded; `None` = one generous default
        // rather than literal forever — an interposed app can always
        // re-call, and an unbounded park on a dead lane is a hang).
        let deadline_ms = timeout_ms.unwrap_or(30_000);
        let slice_ms: u64 = 5;
        let mut waited: u64 = 0;
        loop {
            let need = min - out.len();
            let room = max - out.len();
            // The kernel wait's min is what remains after the ring
            // harvest, capped by what the kernel lane can actually
            // deliver — with ZERO kernel pendings it must be a
            // non-blocking probe (a blocking wait on an empty lane is
            // a hang). The wait itself is a bounded slice whenever
            // ring ops are also in flight so the loop re-polls the
            // ring instead of parking the full deadline kernel-side.
            let kmin = need.min(self.kernel_pending).min(room);
            let kwait = if self.pending.is_empty() && kmin > 0 {
                deadline_ms.saturating_sub(waited)
            } else {
                slice_ms
            };
            let got = kern.getevents(kmin, room, Some(if kmin == 0 { 0 } else { kwait }));
            self.kernel_pending = self.kernel_pending.saturating_sub(got.len());
            out.extend(got);
            if out.len() < max {
                self.harvest_ring(ring, &mut out, max);
            }
            if out.len() >= min || out.len() >= max {
                return out;
            }
            waited += kwait.max(1);
            if waited >= deadline_ms {
                return out; // timeout: whatever we have (libaio semantics)
            }
        }
    }

    /// `io_destroy`: abandon in-flight ring ops (session GC owns the
    /// slots — §5.7 "abandoned slots are GC'd with the session"); no
    /// event delivery afterwards.
    pub fn destroy<R: RingLane>(&mut self, ring: &mut R) {
        for p in self.pending.drain(..) {
            ring.abandon(p.tok);
        }
        self.destroyed = true;
    }
}
