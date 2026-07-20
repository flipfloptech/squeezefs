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
//!   onto the existing runtime and runs the REAL handler
//!   ([`fuse3::raw::Filesystem::read`] / [`Filesystem::write`]) — same
//!   inode locks, same lease/fencing acquisition, same coverage-union
//!   write-through, same W1 patch eligibility. Completion posts back to
//!   the slot from the tokio worker via the [`SlotCompletion`] handle.
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

use crate::fuse_client::{IpcReadProbe, SqueezefsFilesystem, METRICS};
use crate::ipc_host::{DataOp, SessionSink, SlotCompletion};
use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs_ipc::layout::{OP_READ, OP_WRITE};
use std::sync::atomic::Ordering;
use std::sync::Arc;

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
    /// The daemon's existing runtime — handoffs ride it as ordinary
    /// tasks (G-L4-1 leg (iii) priced the wake at ~1–3 µs).
    runtime: tokio::runtime::Handle,
    /// The W1 invalidator; `None` = no kernel to invalidate (pre-L4-6
    /// callers and pure host-isolation tests).
    inval: Option<Arc<Invalidator>>,
    /// Cached process identity for ring-op Requests (constant for the
    /// daemon's lifetime — not two syscalls per op).
    req_uid: u32,
    req_gid: u32,
    req_pid: u32,
}

impl DataPlaneSink {
    pub fn new(fs: SqueezefsFilesystem, runtime: tokio::runtime::Handle) -> Self {
        Self {
            fs: Arc::new(fs),
            runtime,
            inval: None,
            // SAFETY: plain getuid/getgid — always successful.
            req_uid: unsafe { libc::getuid() },
            req_gid: unsafe { libc::getgid() },
            req_pid: std::process::id(),
        }
    }

    /// Wire the §5.6.2 W1 invalidator: `hook(ino)` fires per the
    /// bind/first-write policy above; `window_ms` is the per-ino write
    /// rate window (production default 1000, `SQUEEZEFS_IPC_INVAL_WINDOW_MS`).
    pub fn with_invalidator(
        fs: SqueezefsFilesystem,
        runtime: tokio::runtime::Handle,
        hook: Arc<dyn Fn(u64) + Send + Sync>,
        window_ms: u64,
    ) -> Self {
        Self {
            fs: Arc::new(fs),
            runtime,
            inval: Some(Arc::new(Invalidator {
                hook,
                window: std::time::Duration::from_millis(window_ms),
                last_write: scc::HashMap::new(),
            })),
            // SAFETY: plain getuid/getgid — always successful.
            req_uid: unsafe { libc::getuid() },
            req_gid: unsafe { libc::getgid() },
            req_pid: std::process::id(),
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

    /// READ: try the §5.5.1 sync fast path, demote to the handoff on
    /// contention or in-guard miss.
    fn serve_read(&self, op: DataOp, completion: SlotCompletion) {
        let ino = op.binding.ino;
        let lock = self.fs.get_inode_lock_ref(ino);
        match lock.try_read() {
            Ok(guard) => {
                let probe = self
                    .fs
                    .ipc_read_probe_locked(ino, op.desc.offset, op.desc.len);
                // Drop-guard-before-enqueue (§5.5.1, load-bearing): the
                // guard must be gone before ANY continuation — the Miss
                // handoff re-acquires this lock behind possibly-queued
                // writers, and even the sync completions have no business
                // extending the critical section past the probe.
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
                    IpcReadProbe::Miss => {
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

    /// The read handoff: re-runs the FULL read handler (including its
    /// in-guard async attr fallback) under its own guard, then posts the
    /// bytes into the op's validated arena window.
    fn enqueue_read(&self, op: DataOp, completion: SlotCompletion) {
        METRICS.ipc_async_handoffs.fetch_add(1, Ordering::Relaxed);
        let fs = Arc::clone(&self.fs);
        let request = self.ring_request();
        let ino = op.binding.ino;
        let offset = op.desc.offset;
        let len = op.desc.len;
        self.runtime.spawn(async move {
            // Re-seed a cold attr cache so warm workloads return to the
            // sync fast path after ONE miss demotion (the handler's own
            // fallback reads the backend but does not populate the cache).
            if fs.attr_cache.get(&ino).is_none() {
                fs.refresh_attr_cache(ino).await;
            }
            match fs.read(request, ino, 0, offset, len, 0).await {
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
        self.runtime.spawn(async move {
            match fs.write(request, ino, 0, offset, severed, 0, 0).await {
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
}

/// The ADMIN-lane verb handler over the VL2 job fabric
/// (design-volume-lifecycle §5.1.4). Runs on host ctl threads (plain
/// OS threads) and blocks on the captured runtime handle.
pub struct FabricAdminSink {
    fabric: std::sync::Arc<crate::jobs::JobFabric>,
    rt: tokio::runtime::Handle,
}

impl FabricAdminSink {
    /// Capture the current runtime (call from async context — mount
    /// wiring and tests both are).
    pub fn new(fabric: std::sync::Arc<crate::jobs::JobFabric>) -> Self {
        Self {
            fabric,
            rt: tokio::runtime::Handle::current(),
        }
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
        let arg = arg.trim().to_string();
        let res: Result<String, String> = self.rt.block_on(async move {
            match verb {
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
