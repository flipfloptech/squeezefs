//! Owned parallel-assembly machinery — MEM-2 / RES-9 (pre-rc
//! engineering spec §2 / §7).
//!
//! Two rules this module enforces for the multi-block fan-out paths in
//! `routing.rs` (the read assembly; the router's guard-less striped RMW
//! `write_striped` was the salvage hook's consumer until its 2026-09-10
//! retirement — see `routing::WriteFileOutcome`):
//!
//! 1. **The destination is an owned object, never a laundered
//!    pointer.** [`AssemblyDest`] is the reviewed `Send` wrapper (the
//!    `RangedDest`/`SendPtr`/`SendMutPtr` convention) every per-block
//!    task writes through. On the pooled arm it OWNS the `PooledBuf`
//!    backing, so a task that outlives the coordinator (sibling panic,
//!    outer cancellation) writes into memory its own `Arc` keeps alive
//!    — the backing recycles to its pool only when the LAST owner
//!    drops. On the payload arm it carries the §5.4 lease pointer
//!    behind the same reviewed contract.
//!
//! 2. **Tasks are owned, never detached.** [`OwnedTaskSet`] holds the
//!    per-block tasks in a first-party cancel-gated set
//!    (`squeezefs_ipc::sqz_taskset::OwnedSet` over the sqz-meta lanes):
//!    [`OwnedTaskSet::join_all`] quiesces EVERY task before the
//!    destination can be handed out or released and reports the first
//!    error (inner or panic) only after the last sibling resolved —
//!    `try_join_all` over `JoinHandle`s short-circuited on the first
//!    error and dropped (= detached) the rest. Dropping the set
//!    (outer-future cancellation) CANCELS every remaining task: a
//!    cancelled writer's future is dropped at its next poll boundary
//!    (never polled again — it cannot resume past its current await
//!    point, and its captures die with it), and the set's own drop
//!    bounds the mid-poll window with a TICK-bounded quiesce — the
//!    same non-instant window tokio's `abort` had. With a salvage hook
//!    installed, a detached reaper first quiesces the cancelled set
//!    and hands every SURFACED output to the hook — the RES-9 face for
//!    a fan-out whose outputs are minted-and-published block keys that
//!    must be freed rather than leaked to fsck (contracts:
//!    `tests/assembly_ownership_tests.rs`).
//!
//! `MintedBlockGuard` covers the task-interior window RES-9 names:
//! between `allocate_block` and the `Ok` return that surfaces the block
//! to the caller, ANY exit (a `?` error, a panic, or a cancel-gate drop
//! landing at an await) frees the minted offset instead of leaking an
//! allocated-but-unpublished block.

use std::sync::Arc;

use crate::cache::pool::PooledBuf;
use crate::error::{Result, SqueezefsError};

/// The owned destination of a parallel multi-block assembly.
///
/// Shared as `Arc<AssemblyDest>` between the coordinator and every
/// block task; each task writes its EXCLUSIVE region (block regions
/// partition the request span) through [`Self::write_at`] /
/// [`Self::zero_range`]. After [`OwnedTaskSet::join_all`] drains every
/// writer, [`Self::into_bytes`] hands out the zero-copy view.
pub struct AssemblyDest {
    ptr: *mut u8,
    len: usize,
    /// Pooled arm: the owned backing — recycles to its home pool when
    /// the last `Arc<AssemblyDest>` (task clones + the handed-out
    /// `Bytes`) drops. Payload arm: `None` (the transport owns the
    /// region; see [`Self::payload`]).
    pooled: Option<PooledBuf>,
}

// SAFETY: sending/sharing moves or shares access to `ptr`'s pointee.
// Pooled arm: `pooled` is the uniquely-owned aligned backing (its own
// Send + Sync contract), kept alive as long as any owner exists, and
// never resized after construction (pointer-stable). Payload arm: `ptr`
// addresses registered uring payload memory whose access is exclusive
// to one request for the lease's lifetime (§5.4 — the `RangedDest`
// review point, same contract). Concurrent `&self` writers are bound by
// the `unsafe fn` disjoint-region contract on the write methods; no
// safe `&self` method reads bytes a writer may touch.
unsafe impl Send for AssemblyDest {}
// SAFETY: see the `Send` comment — `&self` access is either the
// bounds-checked disjoint-region writes (unsafe fn contract) or reads
// performed only after every writer joined (`into_bytes` usage rule).
unsafe impl Sync for AssemblyDest {}

impl AssemblyDest {
    /// Pooled-arm constructor: takes OWNERSHIP of the backing. The
    /// buffer's logical length is the destination extent — resize it to
    /// the assembled length (zero-filled) before constructing.
    pub fn pooled(mut buf: PooledBuf) -> Self {
        let len = buf.len();
        // The real backing pointer (non-null even for len 0); stable
        // because the buffer is never resized after this point.
        let ptr = buf.as_mut_ptr();
        Self {
            ptr,
            len,
            pooled: Some(buf),
        }
    }

    /// Payload-arm constructor (zero-copy uring destination).
    ///
    /// # Safety
    ///
    /// `ptr` must be valid for `len` bytes of writes for the lifetime
    /// of this value and every clone of the owning `Arc` — on the read
    /// path that is the §5.4 payload-lease exclusivity window (this
    /// request is the region's only accessor until the reply commits).
    /// Cancellation cancels the [`OwnedTaskSet`] so no detached writer
    /// outlives the request: a cancelled writer's future is dropped at
    /// its next poll boundary (never polled again), and quiesce-on-drop
    /// bounds the mid-poll window — the same window tokio's abort had.
    /// The ent re-arm window itself is MEM-1's owner-token territory,
    /// not this type's.
    pub unsafe fn payload(ptr: *mut u8, len: usize) -> Self {
        Self {
            ptr,
            len,
            pooled: None,
        }
    }

    /// Copy `src` into `[at, at + src.len())`. Bounds are LOUD (assert)
    /// — the alternative is a heap overflow through the raw pointer.
    ///
    /// # Safety
    ///
    /// The caller must guarantee no concurrent access overlaps
    /// `[at, at + src.len())` — on the assembly paths each block task
    /// owns a disjoint region by construction — and, on the payload
    /// arm, that the [`Self::payload`] liveness contract holds.
    pub unsafe fn write_at(&self, at: usize, src: &[u8]) {
        let end = at
            .checked_add(src.len())
            .expect("assembly write range overflow");
        assert!(
            end <= self.len,
            "assembly write past the destination (at {at}, src {}, dest {})",
            src.len(),
            self.len
        );
        // SAFETY: in-bounds per the assert; validity/exclusivity per
        // this method's contract and the struct invariants.
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), self.ptr.add(at), src.len()) };
    }

    /// Zero `[at, at + len)` (the reused-payload replay rule: every
    /// unwritten destination byte must be zeroed, never replayed).
    ///
    /// # Safety
    ///
    /// Same contract as [`Self::write_at`].
    pub unsafe fn zero_range(&self, at: usize, len: usize) {
        let end = at.checked_add(len).expect("assembly zero range overflow");
        assert!(
            end <= self.len,
            "assembly write past the destination (zero at {at}, len {len}, dest {})",
            self.len
        );
        // SAFETY: in-bounds per the assert; validity/exclusivity per
        // this method's contract and the struct invariants.
        unsafe { std::ptr::write_bytes(self.ptr.add(at), 0, len) };
    }

    /// Zero-copy handout of the assembled bytes. Call only after
    /// [`OwnedTaskSet::join_all`] returned — every writer has joined,
    /// so the view is quiescent. Pooled arm: the `Bytes` keep-alive
    /// holds this `Arc`, so the backing recycles when the last handle
    /// drops. Payload arm: the same non-owning `UringBufOwner` view the
    /// read path always handed out (the transport owns the region).
    pub fn into_bytes(self: Arc<Self>) -> bytes::Bytes {
        if self.pooled.is_some() {
            bytes::Bytes::from_owner(ArcDestOwner(self))
        } else {
            // SAFETY (MEM-4 ctor contract): the payload arm's `ptr`/`len`
            // came from `AssemblyDest::payload`, whose own contract is that
            // the region stays valid and exclusively this request's for the
            // handler invocation (§5.4 lease exclusivity), and every writer
            // has joined before this handout.
            unsafe {
                bytes::Bytes::from_owner(crate::cache::pool::UringBufOwner::new(self.ptr, self.len))
            }
        }
    }
}

/// Keep-alive owner behind the pooled arm of
/// [`AssemblyDest::into_bytes`]: the backing recycles when the last
/// `Bytes` handle (and any straggler task `Arc`) drops.
struct ArcDestOwner(Arc<AssemblyDest>);

impl AsRef<[u8]> for ArcDestOwner {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: `[0, len)` is initialized (the pooled backing is
        // resize-filled at construction and only overwritten by task
        // writes) and `ptr` is valid while the `Arc` lives.
        unsafe { std::slice::from_raw_parts(self.0.ptr, self.0.len) }
    }
}

/// The cancel-path salvage hook of [`OwnedTaskSet`]: receives every
/// output that SURFACED (tasks that completed before or during the
/// abort) when the owner is dropped without a full [`OwnedTaskSet::join_all`].
pub type Salvage<T> = Box<dyn FnOnce(Vec<T>) -> futures::future::BoxFuture<'static, ()> + Send>;

/// The shared result sink of an [`OwnedTaskSet`]: outputs surfaced so
/// far plus the first error — kept behind an `Arc` (not a caller
/// local) so cancellation mid-[`OwnedTaskSet::join_all`] still hands
/// the surfaced outputs to the salvage hook.
struct TaskOutputs<T> {
    outputs: Vec<T>,
    first_err: Option<SqueezefsError>,
}

/// An OWNED set of parallel per-block tasks (MEM-2): join-all-before-
/// release semantics on the happy/error paths, cancel (plus optional
/// salvage) on drop. Never detaches a task. Backed by the first-party
/// cancel-gated `sqz_taskset::OwnedSet` on the sqz-meta lanes: a
/// cancelled task's future is DROPPED at its next poll boundary
/// (never polled again — its captures die), and the set's own drop
/// bounds the mid-poll window with a TICK-bounded quiesce.
pub struct OwnedTaskSet<T: Send + 'static> {
    set: squeezefs_ipc::sqz_taskset::OwnedSet,
    results: Arc<parking_lot::Mutex<TaskOutputs<T>>>,
    salvage: Option<Salvage<T>>,
    context: &'static str,
}

impl<T: Send + 'static> OwnedTaskSet<T> {
    fn build(context: &'static str, salvage: Option<Salvage<T>>) -> Self {
        Self {
            set: squeezefs_ipc::sqz_taskset::OwnedSet::new(context, crate::meta_exec::spawn_meta),
            results: Arc::new(parking_lot::Mutex::new(TaskOutputs {
                outputs: Vec::new(),
                first_err: None,
            })),
            salvage,
            context,
        }
    }

    /// A set with no salvage hook (the read-assembly shape: outputs
    /// carry no resources; dropping the set cancels every task and each
    /// task's own `Arc<AssemblyDest>` keeps the destination alive).
    pub fn new(context: &'static str) -> Self {
        Self::build(context, None)
    }

    /// A set whose cancel path hands every surfaced output to
    /// `salvage` (the minted-block fan-out shape: outputs are published
    /// block keys that must be freed, RES-9).
    pub fn with_salvage(context: &'static str, salvage: Salvage<T>) -> Self {
        Self::build(context, Some(salvage))
    }

    /// Spawn a task into the owned set (starts immediately).
    pub fn spawn<F>(&mut self, fut: F)
    where
        F: std::future::Future<Output = Result<T>> + Send + 'static,
    {
        let results = Arc::clone(&self.results);
        let context = self.context;
        self.set.spawn(async move {
            // The retired `JoinSet` reported a panicking sibling as the
            // first error after every task joined; catch here so the
            // contract survives the venue change (the panic is
            // REPORTED through `join_all`, never detached).
            let out = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(fut)).await;
            let mut r = results.lock();
            match out {
                Ok(Ok(t)) => r.outputs.push(t),
                Ok(Err(e)) => {
                    if r.first_err.is_none() {
                        r.first_err = Some(e);
                    }
                }
                Err(panic) => {
                    if r.first_err.is_none() {
                        let msg = panic
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "non-string panic payload".to_string());
                        r.first_err = Some(SqueezefsError::Io(std::io::Error::other(format!(
                            "{context} task panicked: {msg}"
                        ))));
                    }
                }
            }
        });
    }

    /// Join EVERY task — never short-circuits (MEM-2). Outputs of
    /// successful tasks and the FIRST error (inner error or panic) are
    /// returned only after the last sibling resolved, so no exit path
    /// releases the destination while a writer is still running.
    pub async fn join_all(&mut self) -> (Vec<T>, Option<SqueezefsError>) {
        self.set.quiesce().await;
        let mut r = self.results.lock();
        (std::mem::take(&mut r.outputs), r.first_err.take())
    }
}

impl<T: Send + 'static> Drop for OwnedTaskSet<T> {
    fn drop(&mut self) {
        // Cancel FIRST: every remaining writer's future is dropped at
        // its next poll boundary (the cancel gate checks the flag
        // before polling), so no cancelled task ever writes the
        // destination again; one mid-poll finishes that poll — the
        // same non-instant window tokio's `abort_all` had. `self.set`'s
        // own drop (fields drop after this body) then bounds that
        // window with a TICK-bounded blocking quiesce.
        self.set.cancel_all();
        if self.set.live() == 0 && self.results.lock().outputs.is_empty() {
            // Normal completion: `join_all` quiesced the set and handed
            // the outputs to the caller (whose happy/error paths own
            // their custody). Nothing to cancel, nothing to salvage.
            return;
        }
        let Some(salvage) = self.salvage.take() else {
            // No salvage hook: the cancel above (plus the set's own
            // bounded drop-quiesce) is the whole story. Destination
            // liveness under cancel is the spawners' business — each
            // task owns its own `Arc<AssemblyDest>` clone.
            return;
        };
        let handle = self.set.handle();
        let results = Arc::clone(&self.results);
        let context = self.context;
        // Detached by design (documented rationale): `Drop` cannot
        // await. The reaper is bounded — it waits for every cancelled
        // task to resolve, drains the surfaced outputs, runs the
        // salvage hook once, and exits (no ambient runtime needed —
        // the sqz-meta pool is process-lifetime).
        crate::meta_exec::spawn_meta("assembly_salvage", async move {
            handle.quiesce().await;
            let outs = std::mem::take(&mut results.lock().outputs);
            log::debug!(
                "{context}: owner cancelled — salvaging {} surfaced output(s)",
                outs.len()
            );
            salvage(outs).await;
        });
    }
}

/// RES-9 mint guard: covers a block-write task's window between
/// `allocate_block` and the `Ok` return that surfaces the block to the
/// caller. ANY exit inside the window — a `?` error, a panic, or a
/// cancel-gate drop landing at an await — frees the minted offset
/// instead of leaking an allocated(-and-possibly-published) block that
/// only fsck could find. Disarm exactly when custody transfers (the
/// surfaced output, whose failure/cancel paths free explicitly).
pub(crate) struct MintedBlockGuard {
    alloc: Arc<crate::block_allocator::BlockAllocator>,
    offset: u64,
    armed: bool,
}

impl MintedBlockGuard {
    pub(crate) fn new(alloc: Arc<crate::block_allocator::BlockAllocator>, offset: u64) -> Self {
        Self {
            alloc,
            offset,
            armed: true,
        }
    }

    /// Custody transferred (block surfaced to the caller, or the arm
    /// frees explicitly): the guard stands down.
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for MintedBlockGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let alloc = Arc::clone(&self.alloc);
        let offset = self.offset;
        // Detached by design (documented rationale): `Drop` cannot
        // await and the free is async. Bounded single free on the
        // sqz-meta pool (no ambient runtime needed; at process teardown
        // the remount allocator rebuild reclaims the offset regardless).
        // Co-writer-aware: the guarded window is allocate→publish, so the
        // offset is never-published BY CONSTRUCTION — on a co-writer it
        // abandons quietly to the next derivation instead of storming the
        // plane gate (the 2026-08-19 post-fence conviction's class).
        crate::meta_exec::spawn_meta("mint_guard_free", async move {
            log::debug!(
                "mint guard: freeing unsurfaced block offset {offset} (task errored or aborted)"
            );
            let _ = alloc.abandon_unpublished_offset(offset).await;
        });
    }
}
