//! Per-op trace ring — the STORAGE + policy half (e2e audit A2,
//! `docs/design-e2e-perf-audit.md` §1 honesty precondition 2): the ring
//! pool, the arm state, the task-scoped current op, and the drain. The
//! vocabulary, the sample, the SPSC ring and the sampling law are the
//! `#[path]`-shared [`crate::op_trace_core`]. This module lives in the
//! transport crate because the transport is the FIRST hook on every
//! kernel op and every daemon crate already links it: the root crate
//! reaches the ONE ring set through `fuse3::op_trace` (the
//! `read_phase.rs` precedent — statics here, `pub fn`s for the daemon),
//! and the daemon's `squeezefs::op_trace` is the policy layer over it
//! (knob → derived geometry → [`arm`], the `.trace` inode, the il ticket
//! law).
//!
//! ## Cost contract
//!
//! - **Disarmed** (the shipped default): [`stamp`] / [`stamp_now`] /
//!   [`traced`] are ONE relaxed-class load of the pool pointer (null ⇒
//!   return); [`stamp_current`] is one thread-local read (0 ⇒ return);
//!   [`OpScope::poll`] is a plain field compare. No clock read, no
//!   allocation, no atomic RMW. Priced in
//!   `benches/high_concurrency_bench.rs` (`op_trace_hook_*`).
//! - **Armed**: ≈ one multiply (selection), one thread-local ring-index
//!   read, three relaxed slot stores + one Release head store, one
//!   relaxed per-ring counter RMW. Every ring is allocated at [`arm`];
//!   the hot path never allocates — a full ring DROPS the sample and
//!   counts it (`op_trace_dropped`).
//!
//! ## The op id and the current op
//!
//! The FUSE request `unique` IS the op id for kernel ops (the kernel's
//! `fuse_request_send` / `fuse_request_end` tracepoints carry it — the
//! join key); il ring ops use the slot ticket under the IL namespace bit
//! (`squeezefs::op_trace::il_op_id`); detached work (the write pipeline
//! upload, a publish pass, the journal conveyor's tx) carries the
//! ORIGINATING op's id on the unit it enqueues, and re-enters a scope
//! with it where the work runs. The session wraps every handler future
//! in [`scope`] at spawn, so hooks anywhere below the handler read the
//! op through [`current_op`] — a thread-local the scope sets on EVERY
//! poll and restores on exit (the `sqz_task_local!` mechanism, minus
//! its `Box::pin` + `RefCell<Vec>`: a `Cell<u64>` swap is what the hot
//! path can afford). 0 is never an op id (kernel uniques start at
//! `FUSE_REQ_ID_STEP`; the il law sets bit 63).
//!
//! ## Sampling
//!
//! An op is traced iff `selected(op_id, threshold)` (the core's
//! stride-independent hash law); every hook — explicit-id or
//! scope-derived — agrees per id, so a traced op's chain is complete and
//! an untraced op costs nothing past the load. The divisor is DERIVED by
//! the daemon (`squeezefs::op_trace::derive_geometry`); 1 when a test
//! arms.
//!
//! ## Join recipe (the kernel side is a rig recipe, not daemon code)
//!
//! Every sample's `mono_ns` is CLOCK_MONOTONIC — the clock `bpftrace`'s
//! `nsecs` (`bpf_ktime_get_ns`) reports and `perf record -k
//! CLOCK_MONOTONIC` stamps with. On the box:
//!
//! ```text
//! cat <mnt>/.stats > /tmp/pre.json               # the row's pre snapshot
//! bpftrace -e '
//!   tracepoint:fuse:fuse_request_send { printf("send,%llu,%llu\n", args->unique, nsecs); }
//!   tracepoint:fuse:fuse_request_end  { printf("end,%llu,%llu\n",  args->unique, nsecs); }' \
//!   > /tmp/fuse_tp.csv &
//! <the row>
//! cat <mnt>/.stats > /tmp/post.json
//! cat <mnt>/.trace > /tmp/trace.json            # drains the rings
//! tests/op_trace_stitch.py /tmp/trace.json --tp /tmp/fuse_tp.csv \
//!     --stats-pre /tmp/pre.json --stats-post /tmp/post.json
//! ```
//!
//! `fuse:fuse_request_send` is the kernel's queue-time stamp for
//! `unique`, `fuse_request_end` its completion; the daemon's
//! `transport_recv` … `reply_commit` sit between them, so the stitch
//! reports the kernel-side residue per op (`send → transport_recv`,
//! `reply_commit → end`) as a MEASURED join instead of the historical
//! `fio clat − transport_total` subtraction. `io_uring:*` and `nvme:*`
//! tracepoints join the `dev_submit`/`dev_complete` pair by time
//! containment (the daemon's `user_data` is the worker slot index, not
//! the op id — a stage-pair bracket is the honest join there).

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::Instant;

pub use crate::op_trace_core::{select_threshold, selected, Sample, Stage, TraceRing, SELECT_ALL};

/// The geometry + clock an [`arm`] installs (derived by the daemon's
/// policy layer — never chosen here).
#[derive(Debug, Clone, Copy)]
pub struct ArmConfig {
    /// Rings in the pool (one per stamping thread; a thread past the pool
    /// drops + counts).
    pub rings: usize,
    /// Slots per ring (rounded up to a power of two).
    pub ring_capacity: usize,
    /// Sampling divisor (1 = every op).
    pub divisor: u32,
    /// The conversion pair: an `Instant` and the CLOCK_MONOTONIC ns read
    /// back-to-back with it. Every stamp's `mono_ns` = `epoch_mono_ns +
    /// (at − epoch_instant)`.
    pub epoch_instant: Instant,
    pub epoch_mono_ns: u64,
}

#[repr(align(64))]
struct Padded(AtomicU64);

struct Pool {
    /// Pool generation: the thread-local ring claim is keyed on it, so a
    /// re-arm with a NEW pool re-claims and a re-arm reusing this pool
    /// keeps every thread's ring.
    gen: u32,
    rings: Box<[TraceRing]>,
    /// Per-ring push / drop counters (one line each — a shared counter
    /// would put every stamping thread on one line).
    pushed: Box<[Padded]>,
    dropped: Box<[Padded]>,
    /// Drops by threads that found the pool exhausted (no ring).
    dropped_unringed: AtomicU64,
    next_ring: AtomicUsize,
    threshold: AtomicU32,
    divisor: AtomicU32,
    epoch_instant: Instant,
    epoch_mono_ns: u64,
}

/// The HOT pointer: non-null ⇔ armed. Pools are leaked (`Box::leak`) so
/// a raw pointer read here is valid for the process lifetime — an
/// in-flight stamper can never observe a freed pool.
static POOL: AtomicPtr<Pool> = AtomicPtr::new(std::ptr::null_mut());

/// The drain side: the latest pool (armed or not — a disarmed pool still
/// holds undrained samples). Also the consumer lock: the SPSC drain runs
/// under it.
static LATEST: Mutex<Option<&'static Pool>> = Mutex::new(None);

static NEXT_GEN: AtomicU32 = AtomicU32::new(1);

thread_local! {
    /// The task-scoped current op (0 = none) — see the module doc.
    static CURRENT_OP: Cell<u64> = const { Cell::new(0) };
    /// This thread's ring claim: `gen << 32 | idx`; `NO_RING` = unclaimed.
    /// `idx == u32::MAX` = the pool was exhausted when this thread
    /// claimed (drops count, no ring).
    static RING: Cell<u64> = const { Cell::new(NO_RING) };
}

const NO_RING: u64 = u64::MAX;
const EXHAUSTED: u32 = u32::MAX;

/// Arm (or re-arm) the trace ring. A pool whose geometry matches the
/// latest one is REUSED (its threshold/divisor updated in place — an
/// admin toggling the trace on and off never leaks a pool); a different
/// geometry allocates a fresh leaked pool. Rings are allocated HERE.
pub fn arm(cfg: ArmConfig) {
    let mut latest = LATEST.lock().unwrap_or_else(|e| e.into_inner());
    let cap = cfg.ring_capacity.max(2).next_power_of_two();
    let rings = cfg.rings.max(1);
    let pool: &'static Pool = match *latest {
        Some(p) if p.rings.len() == rings && p.rings[0].capacity() == cap => p,
        _ => {
            let p = Box::leak(Box::new(Pool {
                gen: NEXT_GEN.fetch_add(1, Ordering::Relaxed),
                rings: (0..rings).map(|_| TraceRing::with_capacity(cap)).collect(),
                pushed: (0..rings).map(|_| Padded(AtomicU64::new(0))).collect(),
                dropped: (0..rings).map(|_| Padded(AtomicU64::new(0))).collect(),
                dropped_unringed: AtomicU64::new(0),
                next_ring: AtomicUsize::new(0),
                threshold: AtomicU32::new(SELECT_ALL),
                divisor: AtomicU32::new(1),
                epoch_instant: cfg.epoch_instant,
                epoch_mono_ns: cfg.epoch_mono_ns,
            }));
            *latest = Some(p);
            p
        }
    };
    pool.threshold
        .store(select_threshold(cfg.divisor), Ordering::Relaxed);
    pool.divisor.store(cfg.divisor.max(1), Ordering::Relaxed);
    POOL.store(pool as *const Pool as *mut Pool, Ordering::Release);
}

/// Stop sampling. Undrained samples stay readable through [`drain`];
/// scopes already in flight stamp nothing further (their pushes see the
/// null pointer).
pub fn disarm() {
    POOL.store(std::ptr::null_mut(), Ordering::Release);
}

/// Whether hooks are recording.
#[inline]
pub fn is_armed() -> bool {
    !POOL.load(Ordering::Relaxed).is_null()
}

/// The sampling divisor in force (1 = every op; 0 = never armed).
pub fn divisor() -> u32 {
    match LATEST.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        Some(p) => p.divisor.load(Ordering::Relaxed),
        None => 0,
    }
}

#[inline]
fn pool() -> Option<&'static Pool> {
    let p = POOL.load(Ordering::Acquire);
    if p.is_null() {
        None
    } else {
        // SAFETY: pools are leaked at `arm` and never freed, so a non-null
        // pointer published through `POOL` is valid for the process
        // lifetime; the Acquire pairs with `arm`'s Release so the rings
        // behind it are fully constructed.
        Some(unsafe { &*p })
    }
}

/// The op id if it is traced under the current arm, else 0 — the ONE
/// decision every hook shares (called at ingress: the session's spawn,
/// the il dequeue, the conveyor enqueue).
#[inline]
pub fn traced(op_id: u64) -> u64 {
    match pool() {
        Some(p) if op_id != 0 && selected(op_id, p.threshold.load(Ordering::Relaxed)) => op_id,
        _ => 0,
    }
}

/// The task-scoped current op (0 outside a scope).
#[inline]
pub fn current_op() -> u64 {
    CURRENT_OP.with(|c| c.get())
}

#[inline]
fn push(p: &'static Pool, op_id: u64, stage: Stage, at: Instant) {
    // A stamp from before the epoch (an `Instant` captured before the
    // arm) has no CLOCK_MONOTONIC meaning under this pair — skip it.
    let Some(rel) = at.checked_duration_since(p.epoch_instant) else {
        return;
    };
    let mono_ns = p
        .epoch_mono_ns
        .wrapping_add(rel.as_nanos().min(u64::MAX as u128) as u64);
    push_mono(p, op_id, stage, mono_ns);
}

#[inline]
fn push_mono(p: &'static Pool, op_id: u64, stage: Stage, mono_ns: u64) {
    let key = RING.with(|c| c.get());
    let idx = if key != NO_RING && (key >> 32) as u32 == p.gen {
        key as u32
    } else {
        let claimed = p.next_ring.fetch_add(1, Ordering::Relaxed);
        let idx = if claimed < p.rings.len() {
            claimed as u32
        } else {
            EXHAUSTED
        };
        RING.with(|c| c.set(u64::from(p.gen) << 32 | u64::from(idx)));
        idx
    };
    if idx == EXHAUSTED {
        p.dropped_unringed.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let i = idx as usize;
    if p.rings[i].push(Sample {
        op_id,
        stage: stage as u16,
        mono_ns,
    }) {
        p.pushed[i].0.fetch_add(1, Ordering::Relaxed);
    } else {
        p.dropped[i].0.fetch_add(1, Ordering::Relaxed);
    }
}

/// Stamp `stage` for `op_id` at `at` (an `Instant` the caller already
/// holds — the phase record's own clock read, passed through). Refused
/// silently when disarmed, when `op_id` is 0, or when the op is not in
/// the sample.
#[inline]
pub fn stamp(op_id: u64, stage: Stage, at: Instant) {
    if let Some(p) = pool() {
        if op_id != 0 && selected(op_id, p.threshold.load(Ordering::Relaxed)) {
            push(p, op_id, stage, at);
        }
    }
}

/// Stamp `stage` for `op_id` at an absolute CLOCK_MONOTONIC ns the
/// caller already read (the il direct-drive path's `mono_core` stamps —
/// the ring's native clock, no conversion).
#[inline]
pub fn stamp_mono(op_id: u64, stage: Stage, mono_ns: u64) {
    if let Some(p) = pool() {
        if op_id != 0 && selected(op_id, p.threshold.load(Ordering::Relaxed)) {
            push_mono(p, op_id, stage, mono_ns);
        }
    }
}

/// Stamp `stage` for `op_id` now — reads the clock ONLY for a traced op
/// (the form for sites with no phase clock read of their own).
#[inline]
pub fn stamp_now(op_id: u64, stage: Stage) {
    if let Some(p) = pool() {
        if op_id != 0 && selected(op_id, p.threshold.load(Ordering::Relaxed)) {
            push(p, op_id, stage, Instant::now());
        }
    }
}

/// Stamp `stage` for the task-scoped current op at `at` (the form the
/// phase-record functions use). One thread-local read when no op is
/// bound.
#[inline]
pub fn stamp_current(stage: Stage, at: Instant) {
    let id = current_op();
    if id != 0 {
        if let Some(p) = pool() {
            push(p, id, stage, at);
        }
    }
}

/// Bind `op_id` as the current op for every poll of `fut` — iff the op
/// is traced under the current arm ([`traced`]: a disarmed or unsampled
/// op binds 0, and the wrapper is then a plain field compare per poll).
/// The FIRST poll stamps `entry` when given (the session passes
/// [`Stage::HandlerEntry`]; detached work passes `None`).
pub fn scope_with_entry<F: Future>(op_id: u64, entry: Option<Stage>, fut: F) -> OpScope<F> {
    OpScope {
        op_id: traced(op_id),
        entry: entry.map_or(0, |s| s as u16),
        fut,
    }
}

/// [`scope_with_entry`] with no entry stamp.
pub fn scope<F: Future>(op_id: u64, fut: F) -> OpScope<F> {
    scope_with_entry(op_id, None, fut)
}

/// The current-op binding future (see [`scope`]).
pub struct OpScope<F> {
    op_id: u64,
    /// Pending first-poll stamp (0 = none / already stamped).
    entry: u16,
    fut: F,
}

impl<F: Unpin> Unpin for OpScope<F> {}

/// Restores the previous binding on every exit path (Ready, Pending, a
/// panicking poll) so a thread never keeps a dead op bound.
struct Restore(u64);

impl Drop for Restore {
    fn drop(&mut self) {
        CURRENT_OP.with(|c| c.set(self.0));
    }
}

impl<F: Future> Future for OpScope<F> {
    type Output = F::Output;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: structural pin projection. `fut` is never moved out of
        // an `OpScope` once the scope is pinned (no `Drop` impl, no
        // accessor hands out `&mut F`), and `OpScope<F>: Unpin` only when
        // `F: Unpin`, so pinning the scope pins `fut`.
        let this = unsafe { self.get_unchecked_mut() };
        // SAFETY: as above — `this.fut` is pinned because `this` is.
        let fut = unsafe { Pin::new_unchecked(&mut this.fut) };
        if this.op_id == 0 {
            return fut.poll(cx);
        }
        if this.entry != 0 {
            if let Some(stage) = Stage::from_u16(this.entry) {
                stamp_now(this.op_id, stage);
            }
            this.entry = 0;
        }
        let prev = CURRENT_OP.with(|c| c.replace(this.op_id));
        let _restore = Restore(prev);
        fut.poll(cx)
    }
}

/// The traced members of one conveyor batch (a pass serves N units and
/// its pass-level stages belong to EVERY traced member). Fixed-size —
/// members past the cap are a documented sampling artifact, never an
/// allocation.
#[derive(Debug, Clone, Copy)]
pub struct TracedBatch {
    ids: [u64; Self::CAP],
    n: u8,
}

impl Default for TracedBatch {
    fn default() -> Self {
        Self::new()
    }
}

impl TracedBatch {
    /// Traced members carried per batch.
    pub const CAP: usize = 8;

    pub const fn new() -> Self {
        Self {
            ids: [0; Self::CAP],
            n: 0,
        }
    }

    /// Record a member's trace id (0 = untraced, ignored).
    #[inline]
    pub fn push(&mut self, trace_id: u64) {
        if trace_id != 0 && (self.n as usize) < Self::CAP {
            self.ids[self.n as usize] = trace_id;
            self.n += 1;
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Stamp `stage` at `at` for every traced member.
    #[inline]
    pub fn stamp(&self, stage: Stage, at: Instant) {
        if self.n == 0 {
            return;
        }
        if let Some(p) = pool() {
            for id in &self.ids[..self.n as usize] {
                push(p, *id, stage, at);
            }
        }
    }
}

/// DRAIN every ring (the `.trace` read): the samples, sorted by
/// `(op_id, mono_ns)`; the rings are empty afterwards. Runs under the
/// consumer lock.
pub fn drain() -> Vec<Sample> {
    let latest = LATEST.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::new();
    if let Some(p) = *latest {
        for r in p.rings.iter() {
            r.drain_into(&mut out);
        }
    }
    out.sort_unstable_by_key(|s| (s.op_id, s.mono_ns));
    out
}

/// Samples dropped (full ring or exhausted pool), cumulative over the
/// latest pool's life.
pub fn dropped() -> u64 {
    match LATEST.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        Some(p) => {
            p.dropped
                .iter()
                .map(|c| c.0.load(Ordering::Relaxed))
                .sum::<u64>()
                + p.dropped_unringed.load(Ordering::Relaxed)
        }
        None => 0,
    }
}

/// Samples pushed (cumulative over the latest pool's life, drained or
/// not).
pub fn samples_total() -> u64 {
    match LATEST.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        Some(p) => p.pushed.iter().map(|c| c.0.load(Ordering::Relaxed)).sum(),
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static SERIAL: Mutex<()> = Mutex::new(());

    fn arm_all(rings: usize, cap: usize) {
        arm(ArmConfig {
            rings,
            ring_capacity: cap,
            divisor: 1,
            epoch_instant: Instant::now(),
            epoch_mono_ns: 1_000_000_000,
        });
    }

    #[test]
    fn disarmed_is_inert_and_armed_records() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        disarm();
        let _ = drain();
        stamp(5, Stage::Dispatch, Instant::now());
        assert!(drain().is_empty());
        assert_eq!(traced(5), 0);
        arm_all(2, 16);
        assert_eq!(traced(5), 5);
        assert_eq!(traced(0), 0);
        let t = Instant::now();
        stamp(5, Stage::Dispatch, t);
        stamp(
            5,
            Stage::TransportRecv,
            t - std::time::Duration::from_nanos(10),
        );
        let got = drain();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].stage, Stage::TransportRecv as u16);
        assert_eq!(got[1].mono_ns - got[0].mono_ns, 10);
        assert!(
            got[0].mono_ns >= 1_000_000_000,
            "absolute CLOCK_MONOTONIC ns"
        );
        disarm();
    }

    #[test]
    fn scope_binds_and_restores_even_across_a_panicking_poll() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        disarm();
        let _ = drain();
        arm_all(2, 16);
        struct Boom;
        impl Future for Boom {
            type Output = ();
            fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
                assert_eq!(current_op(), 9, "bound inside the poll");
                panic!("deliberate");
            }
        }
        let r = std::panic::catch_unwind(|| {
            let mut s = scope(9, Boom);
            let w = std::task::Waker::noop();
            let mut cx = Context::from_waker(w);
            let _ = Pin::new(&mut s).poll(&mut cx);
        });
        assert!(r.is_err());
        assert_eq!(current_op(), 0, "restored after the unwind");
        disarm();
        let _ = drain();
    }

    #[test]
    fn pool_is_reused_on_same_geometry_and_counts_drops() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        disarm();
        let _ = drain();
        arm_all(1, 4);
        let base = dropped();
        let t = Instant::now();
        for i in 0..10u64 {
            stamp(100 + i, Stage::Dispatch, t);
        }
        assert_eq!(dropped() - base, 6);
        assert_eq!(drain().len(), 4);
        // Re-arm, same geometry: the divisor changes, the pool stays (the
        // per-thread ring claim survives, so the counters continue).
        arm(ArmConfig {
            rings: 1,
            ring_capacity: 4,
            divisor: 3,
            epoch_instant: Instant::now(),
            epoch_mono_ns: 0,
        });
        assert_eq!(divisor(), 3);
        assert_eq!(dropped() - base, 6, "same pool, same counters");
        disarm();
        let _ = drain();
    }

    #[test]
    fn traced_batch_stamps_every_member() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        disarm();
        let _ = drain();
        arm_all(1, 64);
        let mut b = TracedBatch::new();
        assert!(b.is_empty());
        b.push(0);
        assert!(b.is_empty(), "0 is never a member");
        for id in 1..=(TracedBatch::CAP as u64 + 3) {
            b.push(id);
        }
        b.stamp(Stage::PassBegin, Instant::now());
        let got = drain();
        assert_eq!(got.len(), TracedBatch::CAP, "capped members");
        assert!(got.iter().all(|s| s.stage == Stage::PassBegin as u16));
        disarm();
        let _ = drain();
    }
}
