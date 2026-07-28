//! L4 IPC **data plane** — the daemon service sink behind the session
//! host (`docs/design-preload-interception.md` §5.5, PR L4-4).
//!
//! [`DataPlaneSink`] receives validated READ/WRITE ops from the host's
//! pinned service threads ([`crate::ipc_host`]) and serves them against
//! the SAME daemon state kernel-FUSE requests reach — the handlers
//! themselves, so coherence is structural, never mirrored:
//!
//! - **Sync read fast path (§5.5.1)**: per-inode `try_read()` (sync-
//!   callable on the shipped `tokio::sync::RwLock<()>`), then the read
//!   handler's guarded hit-path probe
//!   ([`SqueezefsFilesystem::ipc_read_probe_locked`]). On a hit the bytes
//!   memcpy into the arena and the slot completes on the service thread —
//!   no tokio, no future, no syscall. On **lock contention** or any
//!   **in-guard miss** the op demotes: the guard is dropped FIRST, then
//!   the async handoff enqueues (**drop-guard-before-enqueue is
//!   load-bearing**: tokio's `RwLock` is write-preferring/FIFO, so a
//!   handoff re-acquiring `lock.read()` behind a queued writer while the
//!   service thread still held its read guard would self-deadlock). The
//!   two demotion counters split regression semantics: `lock` growth on
//!   read-only workloads = unexpected writers; `miss` growth on warm
//!   workloads = fast-path rot.
//! - **Async handoff (read demotions + ALL v1 writes)**: the op packages
//!   onto the **fuse3 per-core handler lanes** ([`handoff_spawn`] →
//!   `tpc_spawn` — the same venue kernel-lane handlers run on; the
//!   2026-07-26 handoff-economy fix, see [`handoff_spawn`]'s docs) and
//!   runs the REAL handler ([`fuse3::raw::Filesystem::read`] /
//!   [`Filesystem::write`]) — same inode locks, same lease/fencing
//!   acquisition, same coverage-union write-through, same W1 patch
//!   eligibility. Completion posts back to the slot from the handler
//!   lane via the [`SlotCompletion`] handle.
//!
//! ## Severance at dequeue (§5.5.2 / §5.3.1 rules 1–2)
//!
//! WRITE payloads live in **client-writable** arena memory for the whole
//! serve. The sink severs the payload into private memory exactly once,
//! synchronously at dequeue — before `ipc_async_handoffs` increments,
//! before the handoff can park — so a hostile mid-serve scribble (or a
//! benign client reusing its buffer after ack) can never alter what the
//! daemon writes. The severed copy IS the payload source handed to the
//! write handler (`bytes::Bytes`), entering the same severance-boundary
//! machinery FUSE leases use. Descriptor fields were snapshot by the host
//! drain (§5.3.1 rule 1) — the `DataOp` carries the snapshot; slot-field
//! mutation mid-serve is inert by construction.

use crate::fuse_client::{IpcDirectIneligible, IpcReadProbe, SqueezefsFilesystem, METRICS};
use crate::ipc_host::{DataOp, SessionSink, SlotCompletion};
use crate::meta_backend::Metadata as _;
use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs_ipc::layout::{OP_READ, OP_WRITE};
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// The handoff venue (§5.5.1 — the 2026-07-26 handoff-economy fix):
/// ring handoffs run **where kernel-lane handlers run** — the fuse3
/// per-core handler lanes ([`fuse3::raw::tpc_spawn`]), never the
/// daemon's multi-thread runtime. A service thread is a foreign OS
/// thread to tokio, so `Handle::spawn` from it lands every op on the
/// **global inject queue**, which busy workers poll only between
/// local-queue batches — the measured ~130 µs/op cold-firehose
/// queueing term (reap-economy note §7) the kernel transport never
/// pays: its dispatch task spawns handlers straight onto the TPC
/// lanes. One venue for both transports also keeps the §5.5 "the
/// handoff path IS the existing handler" argument exact — same
/// handler body, same locks, same executor class. The lanes are
/// per-core current-thread runtimes (unbounded FIFO submission, no
/// inject-queue starvation from a foreign thread), which is what the
/// venue pins below assert.
fn handoff_spawn<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    fuse3::raw::tpc_spawn(fut);
}

/// The read handoff body, factored free of the sink so the direct-drive
/// engine's CQE fallback (`crate::ipc_direct`) rides the IDENTICAL path
/// — same venue (`handoff_spawn` → per-core handler lanes), same
/// handler, same counters. Re-runs the FULL read handler (including its
/// in-guard async attr fallback) under its own guard, then posts the
/// bytes into the op's validated arena window.
pub(crate) fn spawn_read_handoff(
    fs: Arc<SqueezefsFilesystem>,
    request: Request,
    op: DataOp,
    completion: SlotCompletion,
) {
    METRICS.ipc_async_handoffs.fetch_add(1, Ordering::Relaxed);
    let ino = op.binding.ino;
    let offset = op.desc.offset;
    let len = op.desc.len;
    // Read-class parity (BindingRights::odirect): hand the handler
    // exactly the flags the kernel path would (`fuse_read_in.flags`
    // carries the description's O_DIRECT bit on every READ). Dropping
    // it reclassified O_DIRECT ring reads as buffered — on cold
    // working sets that re-enabled the whole-block ghost-admission
    // machinery per miss (the measured 13× il-vs-kernel collapse on
    // the 2026-07-25 fabric-latency rig).
    let flags = if op.binding.odirect {
        libc::O_DIRECT as u32
    } else {
        0
    };
    handoff_spawn(async move {
        // Re-seed a cold attr cache so warm workloads return to the
        // sync fast path after ONE miss demotion (the handler's own
        // fallback reads the backend but does not populate the cache).
        if fs.attr_cache.get(&ino).is_none() {
            fs.refresh_attr_cache(ino).await;
        }
        match fs.read(request, ino, 0, offset, len, flags).await {
            Ok(reply) => {
                // Into the SNAPSHOT window (§5.3.1: bounds validated at
                // dequeue; mid-serve descriptor mutation is inert).
                op.payload.write(&reply.data);
                METRICS.ipc_ops_read.fetch_add(1, Ordering::Relaxed);
                METRICS
                    .ipc_bytes_out
                    .fetch_add(reply.data.len() as u64, Ordering::Relaxed);
                completion.complete(reply.data.len() as i64);
            }
            Err(errno) => {
                completion.complete(i64::from(libc::c_int::from(errno)));
            }
        }
    });
}

/// The §5.6.2 W1 invalidation policy: fire on BIND, fire on the FIRST
/// ring write per (ino, window), suppress in-window repeats, never on
/// reads. The hook is injectable (tests record; production pushes
/// `FUSE_NOTIFY_INVAL_INODE` through the fuse3 [`Notify`] handle) — the
/// policy is identical either way and pinned by the lifecycle suite.
///
/// [`Notify`]: fuse3::raw::Notify
struct Invalidator {
    hook: Arc<dyn Fn(u64) + Send + Sync>,
    window: std::time::Duration,
    /// ino → last write-fired instant (latch-free; bounded by the set of
    /// ring-written inos — entries are two words, never reclaimed within
    /// a mount, same leak class as the shim's fd-table cells).
    last_write: scc::HashMap<u64, std::time::Instant>,
}

impl Invalidator {
    /// Bind-time invalidation: unconditional (the kernel may hold pages
    /// from before this process bound), does NOT consume the write
    /// window (the first write after bind still fires — pinned).
    fn on_bind(&self, ino: u64) {
        METRICS.ipc_inval_notifies.fetch_add(1, Ordering::Relaxed);
        (self.hook)(ino);
    }

    /// Write-path invalidation, rate-limited per (ino, window).
    fn on_write(&self, ino: u64) {
        let now = std::time::Instant::now();
        let mut fire = false;
        match self.last_write.entry_sync(ino) {
            scc::hash_map::Entry::Occupied(mut o) => {
                if now.duration_since(*o.get()) >= self.window {
                    *o.get_mut() = now;
                    fire = true;
                }
            }
            scc::hash_map::Entry::Vacant(v) => {
                v.insert_entry(now);
                fire = true;
            }
        }
        if fire {
            METRICS.ipc_inval_notifies.fetch_add(1, Ordering::Relaxed);
            (self.hook)(ino);
        } else {
            METRICS.ipc_inval_suppressed.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The production [`SessionSink`]: fast path + async handoff over one
/// filesystem instance (the same instance the FUSE session serves).
pub struct DataPlaneSink {
    /// `Arc`, not a value: `SqueezefsFilesystem::clone` deep-clones
    /// dozens of `Arc`/moka/arc-swap fields, and the arc-swap Debt
    /// machinery serializes globally — a per-op clone in the handoff
    /// measured **48.9 % of daemon CPU** on the device-true row
    /// (perf, 2026-07-19), flatlining it at ~125 k IOPS regardless of
    /// concurrency. Handoffs clone this Arc: one refcount bump.
    fs: Arc<SqueezefsFilesystem>,
    /// The W1 invalidator; `None` = no kernel to invalidate (pre-L4-6
    /// callers and pure host-isolation tests).
    inval: Option<Arc<Invalidator>>,
    /// Cached process identity for ring-op Requests (constant for the
    /// daemon's lifetime — not two syscalls per op).
    req_uid: u32,
    req_gid: u32,
    req_pid: u32,
    /// DIALED P1/P1.5 direct-drive engine (`crate::ipc_direct`), built
    /// lazily on the FIRST governed O_DIRECT miss — the ddt posture's
    /// every-miss slice (P1) or the default posture's governor-DENIED
    /// slice (P1.5) — so mounts that never see the shape pay nothing.
    /// `Some(None)` = spawn failed once, loudly — every governed op
    /// falls back to the handler path (fallback-is-correctness).
    direct: std::sync::OnceLock<Option<Arc<crate::ipc_direct::DirectDriveEngine>>>,
}

impl Drop for DataPlaneSink {
    fn drop(&mut self) {
        // Engine teardown is the sink's job (the reaper thread holds an
        // Arc of the engine, so Drop-on-Arc alone would never fire):
        // flag + NOP wake + join, draining in-flight CQEs first.
        if let Some(Some(engine)) = self.direct.get() {
            engine.shutdown();
        }
    }
}

impl DataPlaneSink {
    pub fn new(fs: SqueezefsFilesystem) -> Self {
        Self {
            fs: Arc::new(fs),
            inval: None,
            // SAFETY: plain getuid/getgid — always successful.
            req_uid: unsafe { libc::getuid() },
            req_gid: unsafe { libc::getgid() },
            req_pid: std::process::id(),
            direct: std::sync::OnceLock::new(),
        }
    }

    /// Wire the §5.6.2 W1 invalidator: `hook(ino)` fires per the
    /// bind/first-write policy above; `window_ms` is the per-ino write
    /// rate window (production default 1000, `SQUEEZEFS_IPC_INVAL_WINDOW_MS`).
    pub fn with_invalidator(
        fs: SqueezefsFilesystem,
        hook: Arc<dyn Fn(u64) + Send + Sync>,
        window_ms: u64,
    ) -> Self {
        Self {
            fs: Arc::new(fs),
            inval: Some(Arc::new(Invalidator {
                hook,
                window: std::time::Duration::from_millis(window_ms),
                last_write: scc::HashMap::new(),
            })),
            // SAFETY: plain getuid/getgid — always successful.
            req_uid: unsafe { libc::getuid() },
            req_gid: unsafe { libc::getgid() },
            req_pid: std::process::id(),
            direct: std::sync::OnceLock::new(),
        }
    }

    /// The host's bind hook (translated inos in tests ride the sink
    /// wrapper's override).
    pub fn on_bind(&self, ino: u64) {
        if let Some(iv) = &self.inval {
            iv.on_bind(ino);
        }
    }

    /// Synthetic request identity for ring-origin ops: the daemon serves
    /// with its own credentials — authorization already happened at the
    /// §5.2 fd screen (the kernel-granted fd is the capability), exactly
    /// like the kernel path where permission checks precede the WRITE.
    fn ring_request(&self) -> Request {
        Request {
            unique: 0,
            uid: self.req_uid,
            gid: self.req_gid,
            pid: self.req_pid,
        }
    }

    /// The lazily-spawned direct-drive engine (`None` = spawn failed
    /// once, loudly — governed ops stay on the handler path forever).
    fn direct_engine(&self) -> Option<&Arc<crate::ipc_direct::DirectDriveEngine>> {
        self.direct
            .get_or_init(|| {
                match crate::ipc_direct::DirectDriveEngine::spawn(
                    Arc::clone(&self.fs),
                    self.req_uid,
                    self.req_gid,
                    self.req_pid,
                ) {
                    Ok(engine) => Some(engine),
                    Err(e) => {
                        log::warn!(
                            "ipc direct-drive engine failed to spawn ({e}) — governed \
                             ranged reads stay on the handler path"
                        );
                        None
                    }
                }
            })
            .as_ref()
    }

    /// The prelude decision ledger (the W1 `patch_ineligible_*` shape).
    fn count_direct_ineligible(class: IpcDirectIneligible) {
        let counter = match class {
            IpcDirectIneligible::Shape => &METRICS.ipc_direct_ineligible_shape,
            IpcDirectIneligible::Meta => &METRICS.ipc_direct_ineligible_meta,
            IpcDirectIneligible::Layout => &METRICS.ipc_direct_ineligible_layout,
            IpcDirectIneligible::Overlay => &METRICS.ipc_direct_ineligible_overlay,
            IpcDirectIneligible::Backend => &METRICS.ipc_direct_ineligible_backend,
            IpcDirectIneligible::Policy => &METRICS.ipc_direct_ineligible_policy,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// READ: try the §5.5.1 sync fast path, demote to the handoff on
    /// contention or in-guard miss.
    fn serve_read(&self, op: DataOp, completion: SlotCompletion) {
        let ino = op.binding.ino;
        // Device-true parity (2026-07-25 ipc-miss-path fix): on a
        // `direct_device_true` mount, kernel O_DIRECT reads bypass every
        // tier — an O_DIRECT binding's ring reads must do the same, so
        // the tier fast path is skipped by POLICY (not counted as a miss
        // demotion — that counter means fast-path rot, and a device-true
        // mount handing off 100 % of O_DIRECT reads is its design).
        //
        // DIALED P1 (2026-07-26): this governed shape is exactly the
        // direct-drive charter — the prelude decides synchronously
        // (RAM-authoritative, lock-free) and the service thread submits
        // the device read on the ipc-host uring itself; ANY prelude
        // miss falls back to the handler path (correctness owns
        // ambiguity), recorded in the decision ledger.
        if op.binding.odirect && self.fs.router.direct_device_true() {
            let (op, completion) =
                match self
                    .fs
                    .ipc_direct_read_probe(ino, op.desc.offset, op.desc.len)
                {
                    Ok(snap) => match self.direct_engine() {
                        Some(engine) => match engine.submit(op, completion, snap) {
                            Ok(()) => return,
                            // Backend refusal counted at the refusal site.
                            Err(back) => back,
                        },
                        None => {
                            Self::count_direct_ineligible(IpcDirectIneligible::Backend);
                            (op, completion)
                        }
                    },
                    Err(class) => {
                        Self::count_direct_ineligible(class);
                        (op, completion)
                    }
                };
            return self.enqueue_read(op, completion);
        }
        let lock = self.fs.get_inode_lock_ref(ino);
        match lock.try_read() {
            Ok(guard) => {
                let probe =
                    self.fs
                        .ipc_read_probe_locked(ino, op.desc.offset, op.desc.len, &op.payload);
                // Drop-guard-before-enqueue (§5.5.1, load-bearing): the
                // guard must be gone before ANY continuation — the Miss
                // handoff re-acquires this lock behind possibly-queued
                // writers, and even the sync completions have no business
                // extending the critical section past the probe. (A
                // `Served` probe already copied its payload into the
                // arena under the guard — exactly the memcpy the tier leg
                // used to spend on the intermediate buffer there.)
                drop(guard);
                match probe {
                    IpcReadProbe::Eof => {
                        METRICS.ipc_fast_path_serves.fetch_add(1, Ordering::Relaxed);
                        METRICS.ipc_ops_read.fetch_add(1, Ordering::Relaxed);
                        completion.complete(0);
                    }
                    IpcReadProbe::Hit(bytes) => {
                        op.payload.write(&bytes);
                        METRICS.ipc_fast_path_serves.fetch_add(1, Ordering::Relaxed);
                        METRICS.ipc_ops_read.fetch_add(1, Ordering::Relaxed);
                        METRICS
                            .ipc_bytes_out
                            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                        completion.complete(bytes.len() as i64);
                    }
                    IpcReadProbe::Served(n) => {
                        // Tier leg already wrote the payload into the
                        // arena window (op-economy: zero intermediate
                        // alloc/copy).
                        METRICS.ipc_fast_path_serves.fetch_add(1, Ordering::Relaxed);
                        METRICS.ipc_ops_read.fetch_add(1, Ordering::Relaxed);
                        METRICS.ipc_bytes_out.fetch_add(n as u64, Ordering::Relaxed);
                        completion.complete(n as i64);
                    }
                    IpcReadProbe::Miss => {
                        // DIALED P1.5 (2026-07-27): a governed O_DIRECT
                        // miss on the DEFAULT mount runs the R1b
                        // admission decision synchronously in the
                        // prelude — a DENIED miss (the majority on
                        // working sets ≫ budget) is semantically a
                        // device-true serve (ranged device read, no tier
                        // publish, nothing to invalidate) and
                        // direct-drives on the ipc-host uring. GRANTED
                        // escalations and every prelude/policy refusal
                        // demote to the handler exactly as before; a
                        // direct-driven op is not a demotion (that
                        // counter keeps meaning "went to the async
                        // handoff" — the ddt branch's own posture).
                        // Buffered bindings never enter (the 2026-07-15
                        // hybrid-serve directive stays law).
                        let (op, completion) = if op.binding.odirect {
                            match self.try_direct_drive_default(op, completion) {
                                Ok(()) => return,
                                Err(pair) => pair,
                            }
                        } else {
                            (op, completion)
                        };
                        METRICS
                            .ipc_fast_path_miss_demotions
                            .fetch_add(1, Ordering::Relaxed);
                        self.enqueue_read(op, completion);
                    }
                }
            }
            Err(_) => {
                // A writer holds (or queues on) the inode lock: the op
                // was about to wait anyway — demote (§5.5.1).
                METRICS
                    .ipc_fast_path_lock_demotions
                    .fetch_add(1, Ordering::Relaxed);
                self.enqueue_read(op, completion);
            }
        }
    }

    /// DIALED P1.5 (2026-07-27, `.benchmarks/2026-07-27-ipc-direct-drive-default.md`):
    /// the DEFAULT-mount governed-miss reroute. Runs the R1b admission
    /// decision synchronously — every step latch-free single-word
    /// atomics/moka probes, callable from the foreign service thread
    /// (no inode guard, no node lock, no blocking lock crosses the
    /// prelude):
    ///
    /// 1. **Dispatch parity**: only the `second-touch` admission mode
    ///    (the default) carries the governor decision this reroute
    ///    mirrors; `always`/`never` diagnostic modes keep their verbatim
    ///    handler semantics. The §5.6 `ranged_eligible` rule (threshold +
    ///    stream-classification veto) must hold — a read the handler
    ///    would not range must not direct-drive (whole-block + pipeline
    ///    dispatch stays the handler's).
    /// 2. **The P1 shape/custody prelude** (`ipc_direct_read_probe`):
    ///    identical to the ddt leg — overlay screens both sides, 795
    ///    snapshot, fallback-is-correctness for anything ambiguous.
    /// 3. **The admission decision**: `ranged_escalation_candidate`
    ///    records the ghost TOUCH (always — skewed hot subsets must keep
    ///    accumulating re-hit evidence through the ring or they never
    ///    earn admission) and gates Red/cooldown; a candidate then takes
    ///    the governor's NON-RESERVING peek. **GRANT-shaped ⇒ handler**
    ///    (the admission fetch + publish machinery stays where it is;
    ///    `allow_escalation` there remains the ONLY token reservation
    ///    site). **DENIED ⇒ direct-drive** with the denial accounted by
    ///    the peek and NO cooldown recorded — hot keys retry and win the
    ///    trickle (the governor's skew-convergence design).
    ///
    /// Racy-tolerance note (documented, bounded): a prelude-recorded
    /// touch whose op then falls back to the handler (engine SQ-full /
    /// backend refusal / post-CQE 795 failure) is re-recorded by the
    /// handler's own candidacy check — at worst one EXTRA touch, i.e.
    /// one earlier escalation, still governor-bounded (the GhostTable's
    /// own racy-tolerance class); fallbacks are ≈ 0 in steady state.
    fn try_direct_drive_default(
        &self,
        op: DataOp,
        completion: SlotCompletion,
    ) -> Result<(), (DataOp, SlotCompletion)> {
        let router = &self.fs.router;
        if router.tier_admission != crate::routing::TierAdmission::SecondTouch {
            Self::count_direct_ineligible(IpcDirectIneligible::Policy);
            return Err((op, completion));
        }
        let ino = op.binding.ino;
        let file_path = crate::keys::inode_path(ino);
        if !router.ranged_eligible(&file_path, u64::from(op.desc.len)) {
            Self::count_direct_ineligible(IpcDirectIneligible::Policy);
            return Err((op, completion));
        }
        let snap = match self
            .fs
            .ipc_direct_read_probe(ino, op.desc.offset, op.desc.len)
        {
            Ok(snap) => snap,
            Err(class) => {
                Self::count_direct_ineligible(class);
                return Err((op, completion));
            }
        };
        if router.ranged_escalation_candidate(&snap.key)
            && router
                .cache
                .admission_governor
                .escalation_would_admit(router.block_size.load(Ordering::Relaxed))
        {
            // GRANT: the escalation (whole-block ghost admission +
            // validated publish) rides the unchanged handler path.
            Self::count_direct_ineligible(IpcDirectIneligible::Policy);
            return Err((op, completion));
        }
        match self.direct_engine() {
            Some(engine) => engine.submit(op, completion, snap),
            None => {
                Self::count_direct_ineligible(IpcDirectIneligible::Backend);
                Err((op, completion))
            }
        }
    }

    /// The read handoff: [`spawn_read_handoff`] with this sink's ring
    /// identity (the body is shared with the direct-drive CQE fallback).
    fn enqueue_read(&self, op: DataOp, completion: SlotCompletion) {
        spawn_read_handoff(Arc::clone(&self.fs), self.ring_request(), op, completion);
    }

    /// WRITE (all writes are handoffs in v1 — OQ-3 decides a sync write
    /// fast path by measurement): sever at dequeue, then run the real
    /// write handler with the severed copy as its payload source.
    fn serve_write(&self, op: DataOp, completion: SlotCompletion) {
        // §5.5.2 severance — the ONE arena read, on the service thread,
        // BEFORE the handoff counter increments (tests park the handoff
        // behind a held writer and scribble the arena: the scribble must
        // be inert).
        let severed = bytes::Bytes::from(op.payload.read_severed());
        METRICS.ipc_async_handoffs.fetch_add(1, Ordering::Relaxed);
        let fs = Arc::clone(&self.fs);
        let request = self.ring_request();
        let inval = self.inval.clone();
        let ino = op.binding.ino;
        let offset = op.desc.offset;
        // Killpriv-v2 il parity (2026-07-28 campaign): intercepted
        // write(2) never runs the kernel's file_remove_privs, so the
        // session peer's HELLO-time class (BindingRights::kill_priv —
        // uid + CAP_FSETID, SO_PEERCRED-verified) stands in for the
        // kernel's per-write !capable(CAP_FSETID) and rides the SAME
        // daemon clearing law the kernel path uses. Known-clean inos
        // short-circuit on the handler's latch — the common case pays a
        // contains-check, nothing more.
        let write_flags = if op.binding.kill_priv {
            fuse3::raw::flags::FUSE_WRITE_KILL_SUIDGID
        } else {
            0
        };
        handoff_spawn(async move {
            match fs
                .write(request, ino, 0, offset, severed, write_flags, 0)
                .await
            {
                Ok(reply) => {
                    METRICS.ipc_ops_write.fetch_add(1, Ordering::Relaxed);
                    METRICS
                        .ipc_bytes_in
                        .fetch_add(u64::from(reply.written), Ordering::Relaxed);
                    completion.complete(i64::from(reply.written));
                    // §5.6.2 W1: invalidate AFTER the write landed (the
                    // kernel's refetch must observe the new state);
                    // rate-limited per (ino, window); reads never fire.
                    if let Some(iv) = inval {
                        iv.on_write(ino);
                    }
                }
                Err(errno) => {
                    completion.complete(i64::from(libc::c_int::from(errno)));
                }
            }
        });
    }
}

impl SessionSink for DataPlaneSink {
    fn serve_data(&self, op: DataOp, completion: SlotCompletion) {
        match op.desc.op {
            OP_READ => self.serve_read(op, completion),
            OP_WRITE => self.serve_write(op, completion),
            // The host validated the op code; anything else here is a
            // daemon bug — complete EINVAL loudly rather than strand.
            other => {
                log::error!("ipc data plane: unexpected op code {other} reached the sink");
                completion.complete(-libc::EINVAL as i64);
            }
        }
    }

    fn on_bind(&self, ino: u64) {
        DataPlaneSink::on_bind(self, ino);
    }

    fn flush(&self) {
        // Direct-drive submit-batch economy: one `io_uring_enter` per
        // drain sweep (SessionSink::flush liveness rule — a published
        // SQE must be kernel-visible before the service thread parks).
        if let Some(Some(engine)) = self.direct.get() {
            engine.flush();
        }
    }
}

/// The ADMIN-lane verb handler over the VL2 job fabric
/// (design-volume-lifecycle §5.1.4) — and, since PR VL3, the volume
/// lifecycle verbs against the live filesystem (`volume-add-data`,
/// `volume-list`, the re-homed health overrides). Runs on host ctl
/// threads (plain OS threads) and blocks on the captured runtime handle.
pub struct FabricAdminSink {
    fabric: std::sync::Arc<crate::jobs::JobFabric>,
    /// The live filesystem the volume verbs act on. `None` = fabric-only
    /// wiring (fabric unit tests); volume verbs refuse loudly then.
    fs: Option<std::sync::Arc<SqueezefsFilesystem>>,
    rt: tokio::runtime::Handle,
}

impl FabricAdminSink {
    /// Capture the current runtime (call from async context — mount
    /// wiring and tests both are).
    pub fn new(fabric: std::sync::Arc<crate::jobs::JobFabric>) -> Self {
        Self {
            fabric,
            fs: None,
            rt: tokio::runtime::Handle::current(),
        }
    }

    /// [`Self::new`] with the live filesystem wired — the mount posture
    /// (volume verbs served).
    pub fn with_fs(
        fabric: std::sync::Arc<crate::jobs::JobFabric>,
        fs: std::sync::Arc<SqueezefsFilesystem>,
    ) -> Self {
        Self {
            fabric,
            fs: Some(fs),
            rt: tokio::runtime::Handle::current(),
        }
    }

    fn volume_states_json(fs: &SqueezefsFilesystem) -> serde_json::Value {
        use std::sync::atomic::Ordering;
        let rows: Vec<serde_json::Value> = fs
            .router
            .backend_router
            .volume_states()
            .into_iter()
            .map(|row| {
                let mut v = serde_json::json!({
                    "id": row.id,
                    "backing_dev": row.backing_dev,
                    "state": row.state,
                    "healthy": row.healthy,
                    "capacity_bytes": row.capacity_bytes,
                    "used_bytes": row.used_bytes,
                    "free_bytes": row.free_bytes,
                });
                // §5.2 reporting: when a drain is active, the draining
                // row carries the same math preflight ran (live-updated
                // at every checkpoint).
                if row.state == crate::VOL_STATE_DRAINING {
                    v["evacuate"] = serde_json::json!({
                        "needed_bytes": crate::fuse_client::METRICS
                            .evacuate_needed_bytes.load(Ordering::Relaxed),
                        "avail_bytes": crate::fuse_client::METRICS
                            .evacuate_avail_bytes.load(Ordering::Relaxed),
                        "transient_bytes": crate::fuse_client::METRICS
                            .evacuate_transient_bytes.load(Ordering::Relaxed),
                    });
                }
                v
            })
            .collect();
        serde_json::json!(rows)
    }

    fn status_json(st: &crate::jobs::JobStatus) -> serde_json::Value {
        serde_json::json!({
            "job_id": st.job_id,
            "state": format!("{:?}", st.state).to_lowercase(),
            "tasks_done": st.tasks_done,
            "tasks_total": st.tasks_total,
            "throttle_pct": st.throttle_pct,
        })
    }
}

impl crate::ipc_host::AdminSink for FabricAdminSink {
    fn handle(&self, verb: &str, arg: &str) -> (bool, String) {
        let fabric = self.fabric.clone();
        let fs = self.fs.clone();
        let arg = arg.trim().to_string();
        let res: Result<String, String> = self.rt.block_on(async move {
            // The VL3 volume verbs need the live filesystem.
            let need_fs = || {
                fs.clone()
                    .ok_or_else(|| "volume verbs not wired on this host".to_string())
            };
            match verb {
                // -----------------------------------------------------
                // VL3 volume lifecycle (design-volume-lifecycle §5.3/§6)
                // -----------------------------------------------------
                "volume-add-data" => {
                    let fs = need_fs()?;
                    let mut parts = arg.split_whitespace();
                    let device = parts.next().unwrap_or("");
                    let no_rebalance = match parts.next() {
                        None => false,
                        Some("no-rebalance") => true,
                        Some(other) => {
                            return Err(format!(
                                "usage: volume-add-data <device> [no-rebalance] \
                                 (unknown flag '{other}')"
                            ))
                        }
                    };
                    if device.is_empty() {
                        return Err("usage: volume-add-data <device> [no-rebalance]".to_string());
                    }
                    let rec = fs.admin_add_data_volume(device, no_rebalance).await?;
                    Ok(serde_json::json!({
                        "id": rec.id,
                        "backing_dev": rec.backing_dev,
                        "state": rec.state,
                        "added_ts": rec.added_ts,
                        // §5.3 step 6 (KD-12): the auto-rebalance DEFAULT
                        // is armed — a bounded fabric pass at 25 % unless
                        // opted out.
                        "rebalance": if no_rebalance {
                            "opted out (--no-rebalance)"
                        } else {
                            "auto-rebalance pass submitted (bounded, 25 % throttle — \
                             see `squeezefs job list`)"
                        },
                    })
                    .to_string())
                }
                "volume-remove-data" => {
                    let fs = need_fs()?;
                    let mut parts = arg.split_whitespace();
                    let vol_id = parts.next().unwrap_or("");
                    let throttle: u32 = match parts.next() {
                        None => 100,
                        Some(pct) => pct
                            .parse()
                            .map_err(|_| "usage: volume-remove-data <vol-id> [pct]".to_string())?,
                    };
                    if vol_id.is_empty() {
                        return Err("usage: volume-remove-data <vol-id> [pct]".to_string());
                    }
                    let job_id = fs.admin_remove_data_volume(vol_id, throttle).await?;
                    Ok(serde_json::json!({
                        "volume_id": vol_id,
                        "state": crate::VOL_STATE_DRAINING,
                        "job_id": job_id,
                        "throttle_pct": throttle,
                    })
                    .to_string())
                }
                "volume-undrain" => {
                    let fs = need_fs()?;
                    let vol_id = arg.trim();
                    if vol_id.is_empty() {
                        return Err("usage: volume-undrain <vol-id>".to_string());
                    }
                    fs.admin_undrain_data_volume(vol_id).await?;
                    Ok(serde_json::json!({
                        "volume_id": vol_id,
                        "state": crate::VOL_STATE_ACTIVE,
                    })
                    .to_string())
                }
                "volume-list" => {
                    let fs = need_fs()?;
                    Ok(Self::volume_states_json(&fs).to_string())
                }
                // -----------------------------------------------------
                // PR VL5b (§5.5.2): ONLINE slot migration — submits the
                // migrate-meta-slot fabric job (bulk copy + conveyor
                // delta tee + §5.5.2a cutover gate + §5.5.2b flip) on
                // the live daemon.
                // -----------------------------------------------------
                "migrate-meta-slot" => {
                    let mut parts = arg.split_whitespace();
                    let usage = "usage: migrate-meta-slot <slot> <target-volume-index>";
                    let slot: u16 = parts
                        .next()
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| usage.to_string())?;
                    let target_volume: usize = parts
                        .next()
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| usage.to_string())?;
                    let job_id = fabric
                        .submit(crate::jobs::JobSpec {
                            job_type: crate::jobs::JobType::MigrateMetaSlot {
                                slot,
                                target_volume,
                            },
                            throttle_pct: 100,
                        })
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok(serde_json::json!({
                        "job_id": job_id,
                        "slot": slot,
                        "target_volume": target_volume,
                    })
                    .to_string())
                }
                "volume-disable" | "volume-enable" => {
                    let fs = need_fs()?;
                    let disabled = verb == "volume-disable";
                    fs.router
                        .backend_router
                        .set_health_override(&arg, disabled)
                        .map_err(|e| e.to_string())?;
                    Ok(if disabled {
                        format!(
                            "data volume '{arg}' disabled (fail-stop health override: new \
                             writes fail over; blocks already on it read EIO until \
                             re-enabled — this is not an evacuation)"
                        )
                    } else {
                        format!("data volume '{arg}' enabled (health override cleared)")
                    })
                }
                "meta-volume-disable" | "meta-volume-enable" => {
                    let fs = need_fs()?;
                    let disabled = verb == "meta-volume-disable";
                    fs.admin_set_meta_volume_health(&arg, disabled)?;
                    Ok(if disabled {
                        format!(
                            "metadata volume '{arg}' disabled (fail-stop health override — \
                             not an evacuation)"
                        )
                    } else {
                        format!("metadata volume '{arg}' enabled (health override cleared)")
                    })
                }
                // -----------------------------------------------------
                // PR VL6a (§5.6): online fsck — submits the detection
                // job on the live daemon's fabric (RAM-authoritative
                // C1–C6 scan + optional C7 scrub). PR VL6b (§5.6a):
                // `repair` plans (dry-run), `repair apply` executes on
                // the coordinator under per-object leases.
                // arg: "[scrub|scrub-only] [repair] [apply]
                //       [qdir <path>] [throttle <pct>]"
                // -----------------------------------------------------
                "fsck" => {
                    const USAGE: &str = "usage: fsck [scrub|scrub-only] [repair] [apply] \
                                         [qdir <path>] [throttle <pct>]";
                    let mut scrub = false;
                    let mut scrub_only = false;
                    let mut repair = false;
                    let mut apply = false;
                    let mut quarantine_dir: Option<String> = None;
                    let mut throttle_pct: u32 = 100;
                    let mut parts = arg.split_whitespace();
                    while let Some(tok) = parts.next() {
                        match tok {
                            "scrub" => scrub = true,
                            "scrub-only" => scrub_only = true,
                            "repair" => repair = true,
                            "apply" => apply = true,
                            "qdir" => {
                                quarantine_dir = Some(
                                    parts.next().ok_or_else(|| USAGE.to_string())?.to_string(),
                                );
                            }
                            "throttle" => {
                                throttle_pct = parts
                                    .next()
                                    .and_then(|p| p.parse().ok())
                                    .ok_or_else(|| USAGE.to_string())?;
                            }
                            other => return Err(format!("{USAGE} (unknown token '{other}')")),
                        }
                    }
                    if (apply || quarantine_dir.is_some()) && !repair {
                        return Err("apply/qdir are only valid with repair (§5.6a: \
                                    dry-run default)"
                            .to_string());
                    }
                    let job_id = fabric
                        .submit(crate::jobs::JobSpec {
                            job_type: crate::jobs::JobType::Fsck {
                                scrub,
                                scrub_only,
                                repair,
                                apply,
                                quarantine_dir,
                            },
                            throttle_pct,
                        })
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok(serde_json::json!({
                        "job_id": job_id,
                        "scrub": scrub || scrub_only,
                        "repair": repair,
                        "apply": apply,
                        "throttle_pct": throttle_pct,
                    })
                    .to_string())
                }
                // -----------------------------------------------------
                // PR VL7 (§5.7): the online defragmenter. `report` runs
                // the four-axis measurement inline (publishing the
                // frag_* gauges) and returns the per-volume/per-axis
                // JSON; the mover spellings submit fabric jobs —
                // `data [vol <id>]` (D1/D2), `meta` (D4), `fold` (D3),
                // `rebalance` (the KD-12 operator surface for the VL4
                // job). arg: "report | data [vol <id>] [throttle <pct>]
                // | meta [throttle <pct>] | fold [throttle <pct>]
                // | rebalance [throttle <pct>]"
                // -----------------------------------------------------
                "defrag" => {
                    const USAGE: &str = "usage: defrag report | defrag \
                                         data|meta|fold|rebalance [vol <id>] [throttle <pct>]";
                    let mut parts = arg.split_whitespace();
                    let mode = parts.next().ok_or_else(|| USAGE.to_string())?;
                    let mut volume: Option<String> = None;
                    let mut throttle_pct: u32 = 100;
                    while let Some(tok) = parts.next() {
                        match tok {
                            "vol" => {
                                volume = Some(
                                    parts.next().ok_or_else(|| USAGE.to_string())?.to_string(),
                                );
                            }
                            "throttle" => {
                                throttle_pct = parts
                                    .next()
                                    .and_then(|p| p.parse().ok())
                                    .ok_or_else(|| USAGE.to_string())?;
                            }
                            other => return Err(format!("{USAGE} (unknown token '{other}')")),
                        }
                    }
                    if volume.is_some() && mode != "data" {
                        return Err("vol <id> is only valid with `defrag data`".to_string());
                    }
                    if mode == "report" {
                        let fs = need_fs()?;
                        let meta = fs
                            .meta_backend
                            .as_ref()
                            .ok_or_else(|| "no metadata backend mounted".to_string())?;
                        let report = crate::defrag::measure(meta, &fs.router)
                            .await
                            .map_err(|e| format!("defrag measurement failed: {e}"))?;
                        return serde_json::to_string(&report)
                            .map_err(|e| format!("report encode: {e}"));
                    }
                    let job_type = match mode {
                        "data" => crate::jobs::JobType::DefragData { volume_id: volume },
                        "meta" => crate::jobs::JobType::DefragMeta,
                        "fold" => crate::jobs::JobType::DefragFold,
                        "rebalance" => crate::jobs::JobType::Rebalance,
                        other => return Err(format!("{USAGE} (unknown mode '{other}')")),
                    };
                    let job_id = fabric
                        .submit(crate::jobs::JobSpec {
                            job_type,
                            throttle_pct,
                        })
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok(serde_json::json!({
                        "job_id": job_id,
                        "mode": mode,
                        "throttle_pct": throttle_pct,
                    })
                    .to_string())
                }
                // The persisted `job:{id}:report` payload of a completed
                // fsck job (structured findings JSON).
                "fsck-report" => {
                    let job_id = arg.trim();
                    if job_id.is_empty() {
                        return Err("usage: fsck-report <job-id>".to_string());
                    }
                    let name = format!("{}{job_id}:report", crate::jobs::JOB_XATTR_PREFIX);
                    match fabric
                        .meta_handle()
                        .getxattr(1, &name)
                        .await
                        .map_err(|e| e.to_string())?
                    {
                        Some(bytes) => String::from_utf8(bytes)
                            .map_err(|_| "report payload not UTF-8".to_string()),
                        None => Err(format!(
                            "no report for job {job_id} (still running? see job-status)"
                        )),
                    }
                }
                "job-list" => {
                    let mut out = Vec::new();
                    for rec in crate::jobs::JobFabric::list_records(fabric.meta_handle())
                        .await
                        .map_err(|e| e.to_string())?
                    {
                        // Live state (the record lags by a checkpoint).
                        let st = fabric
                            .status(&rec.job_id)
                            .await
                            .map_err(|e| e.to_string())?;
                        if let Some(st) = st {
                            out.push(Self::status_json(&st));
                        }
                    }
                    Ok(serde_json::json!(out).to_string())
                }
                "job-status" => match fabric.status(&arg).await.map_err(|e| e.to_string())? {
                    Some(st) => Ok(Self::status_json(&st).to_string()),
                    None => Err(format!("unknown job {arg}")),
                },
                "job-pause" => fabric
                    .pause(&arg)
                    .await
                    .map(|_| "paused".into())
                    .map_err(|e| e.to_string()),
                "job-resume" => fabric
                    .resume(&arg)
                    .await
                    .map(|_| "resumed".into())
                    .map_err(|e| e.to_string()),
                "job-cancel" => fabric
                    .cancel(&arg)
                    .await
                    .map(|_| "cancelled".into())
                    .map_err(|e| e.to_string()),
                "job-throttle" => {
                    let (id, pct) = arg
                        .split_once(' ')
                        .ok_or_else(|| "usage: job-throttle <id> <pct>".to_string())?;
                    let pct: u32 = pct.trim().parse().map_err(|_| "bad pct".to_string())?;
                    fabric
                        .throttle(id.trim(), pct)
                        .await
                        .map(|_| "throttled".into())
                        .map_err(|e| e.to_string())
                }
                other => Err(format!(
                    "unknown admin verb `{other}` (see docs/design-volume-lifecycle.md §6)"
                )),
            }
        });
        // Oversize replies refuse instead of truncating (wire cap).
        match res {
            Ok(body) if body.len() > squeezefs_ipc::wire::ADMIN_BODY_MAX => (
                false,
                "reply too large; use the offline probe (`squeezefs job list <sqmeta-uri>`)"
                    .to_string(),
            ),
            Ok(body) => (true, body),
            Err(e) => (false, e),
        }
    }
}

#[cfg(test)]
mod handoff_venue_tests {
    use super::handoff_spawn;
    use std::time::Duration;

    /// The venue contract (§5.5.1 handoff economy): a handoff future
    /// executes on a **per-core current-thread handler lane** — kernel
    /// parity — never on the caller's multi-thread runtime. Weakening
    /// evidence: routing `handoff_spawn` through a captured
    /// multi-thread `Handle::spawn` (the pre-fix shape) fails this pin
    /// with `MultiThread`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn handoff_runs_on_a_current_thread_handler_lane() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        handoff_spawn(async move {
            let flavor = tokio::runtime::Handle::current().runtime_flavor();
            let _ = tx.send(flavor);
        });
        let flavor = tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .expect("handoff future must run promptly")
            .expect("handoff future must complete");
        assert_eq!(
            flavor,
            tokio::runtime::RuntimeFlavor::CurrentThread,
            "ring handoffs must ride the fuse3 per-core handler lanes \
             (kernel parity), not the caller's multi-thread runtime"
        );
    }

    /// Service threads are plain OS threads with NO ambient tokio
    /// context — the handoff venue must accept a spawn from one
    /// without panicking and still complete the future (the pre-fix
    /// shape needed a captured `Handle`; this pin keeps the seam
    /// callable from exactly where the drain runs).
    #[test]
    fn handoff_spawns_from_a_plain_os_thread_without_ambient_runtime() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            handoff_spawn(async move {
                let _ = tx.send(42u32);
            });
        })
        .join()
        .expect("spawning thread must not panic");
        let got = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("handoff future must complete without an ambient runtime");
        assert_eq!(got, 42);
    }
}
