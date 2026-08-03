//! Owned parallel-assembly machinery — MEM-2 / RES-9 (pre-rc
//! engineering spec §2 / §7).
//!
//! Two rules this module enforces for the multi-block fan-out paths in
//! `routing.rs` (the read assembly and `write_striped`):
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
//!    per-block tasks in a `tokio::task::JoinSet`:
//!    [`OwnedTaskSet::join_all`] joins EVERY task before the
//!    destination can be handed out or released and reports the first
//!    error (inner or panic) only after the last sibling joined —
//!    `try_join_all` over `JoinHandle`s short-circuited on the first
//!    `JoinError` and dropped (= detached, tokio does not abort on
//!    handle drop) the rest. Dropping the set (outer-future
//!    cancellation) aborts every remaining task; with a salvage hook
//!    installed, a detached reaper first drains the aborted set and
//!    hands every SURFACED output to the hook — `write_striped`'s
//!    RES-9 face, where each output is a minted-and-published block
//!    key that must be freed rather than leaked to fsck.
//!
//! [`MintedBlockGuard`] covers the task-interior window RES-9 names:
//! between `allocate_block` and the `Ok` return that surfaces the block
//! to the caller, ANY exit (a `?` error, a panic, or a `JoinSet` abort
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
    /// Cancellation aborts the [`OwnedTaskSet`] so no detached writer
    /// outlives the request; the ent re-arm window itself is MEM-1's
    /// owner-token territory, not this type's.
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

/// An OWNED set of parallel per-block tasks (MEM-2): join-all-before-
/// release semantics on the happy/error paths, abort (plus optional
/// salvage) on cancellation. Never detaches a task.
pub struct OwnedTaskSet<T: Send + 'static> {
    set: tokio::task::JoinSet<Result<T>>,
    /// Outputs surfaced so far — kept in `self` (not a caller local) so
    /// cancellation mid-[`Self::join_all`] still hands them to the
    /// salvage hook.
    outputs: Vec<T>,
    salvage: Option<Salvage<T>>,
    context: &'static str,
}

impl<T: Send + 'static> OwnedTaskSet<T> {
    /// A set with no salvage hook (the read-assembly shape: outputs
    /// carry no resources; dropping the set aborts every task and each
    /// task's own `Arc<AssemblyDest>` keeps the destination alive).
    pub fn new(context: &'static str) -> Self {
        Self {
            set: tokio::task::JoinSet::new(),
            outputs: Vec::new(),
            salvage: None,
            context,
        }
    }

    /// A set whose cancel path hands every surfaced output to
    /// `salvage` (the `write_striped` shape: outputs are minted block
    /// keys that must be freed, RES-9).
    pub fn with_salvage(context: &'static str, salvage: Salvage<T>) -> Self {
        Self {
            set: tokio::task::JoinSet::new(),
            outputs: Vec::new(),
            salvage: Some(salvage),
            context,
        }
    }

    /// Spawn a task into the owned set (starts immediately).
    pub fn spawn<F>(&mut self, fut: F)
    where
        F: std::future::Future<Output = Result<T>> + Send + 'static,
    {
        self.set.spawn(fut);
    }

    /// Join EVERY task — never short-circuits (MEM-2). Outputs of
    /// successful tasks and the FIRST error (inner error or panic) are
    /// returned only after the last sibling joined, so no exit path
    /// releases the destination while a writer is still running.
    pub async fn join_all(&mut self) -> (Vec<T>, Option<SqueezefsError>) {
        let mut first_err: Option<SqueezefsError> = None;
        while let Some(joined) = self.set.join_next().await {
            match joined {
                Ok(Ok(t)) => self.outputs.push(t),
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(join_err) => {
                    if first_err.is_none() {
                        let kind = if join_err.is_panic() {
                            "panicked"
                        } else {
                            "was aborted"
                        };
                        first_err = Some(SqueezefsError::Io(std::io::Error::other(format!(
                            "{} task {kind}: {join_err:?}",
                            self.context
                        ))));
                    }
                }
            }
        }
        (std::mem::take(&mut self.outputs), first_err)
    }
}

impl<T: Send + 'static> Drop for OwnedTaskSet<T> {
    fn drop(&mut self) {
        if self.set.is_empty() && self.outputs.is_empty() {
            // Normal completion: `join_all` drained the set and handed
            // the outputs to the caller (whose happy/error paths own
            // their custody). Nothing to abort, nothing to salvage.
            return;
        }
        let Some(salvage) = self.salvage.take() else {
            // No salvage hook: letting the `JoinSet` drop aborts every
            // remaining task. Destination liveness under abort is the
            // spawners' business — each task owns its own
            // `Arc<AssemblyDest>` clone.
            return;
        };
        let mut set = std::mem::take(&mut self.set);
        let outputs = std::mem::take(&mut self.outputs);
        let context = self.context;
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // No runtime (teardown): the `JoinSet` drop still aborts
            // the tasks; salvage state is rebuilt at next mount (the
            // allocator state is process-lifetime).
            return;
        };
        // Detached by design (documented rationale): `Drop` cannot
        // await. The reaper is bounded — it aborts the set, drains
        // every surfaced output, runs the salvage hook once, and exits.
        handle.spawn(async move {
            set.abort_all();
            let mut outs = outputs;
            while let Some(joined) = set.join_next().await {
                if let Ok(Ok(t)) = joined {
                    outs.push(t);
                }
            }
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
/// `JoinSet` abort landing at an await — frees the minted offset
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
        // await and `free_block` is async. Bounded single free; on a
        // shutting-down runtime the spawn is a no-op and the remount
        // allocator rebuild reclaims the offset.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                log::debug!(
                    "mint guard: freeing unsurfaced block offset {offset} (task errored or aborted)"
                );
                let _ = alloc.free_block(offset).await;
            });
        }
    }
}
