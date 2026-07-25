//! v1.1 libaio interposer glue — the pure cores (OQ-1,
//! `docs/design-preload-interception.md` §12).
//!
//! Everything here is syscall-free and hermetically tested
//! (`tests/aio_glue_tests.rs`): the `#[repr(C)]` mirrors of libaio's
//! *userspace* structs, the glue-level eligibility screen in front of
//! [`crate::aio_core::classify_iocb`], and the `io_context_t` →
//! [`AioCtxState`] registry. The `#[no_mangle]` `io_*` symbols in
//! `interpose.rs` are thin shells over these plus the session
//! ring/kernel lanes.

use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::Mutex;

use crate::aio_core::{AioCtxState, IocbClass};
use crate::bailout::rwf_passthrough;

// ---------------------------------------------------------------------------
// libaio.h userspace ABI (LP64 little-endian — the only shipped targets)
// ---------------------------------------------------------------------------

/// `IOCB_CMD_PREAD` — libaio.h `IO_CMD_PREAD`.
pub const IOCB_CMD_PREAD: u16 = 0;
/// `IOCB_CMD_PWRITE` — libaio.h `IO_CMD_PWRITE`.
pub const IOCB_CMD_PWRITE: u16 = 1;
/// `IOCB_FLAG_RESFD` — completion additionally signals `resfd` (an
/// eventfd) kernel-side; the ring lane cannot deliver that.
pub const IOCB_FLAG_RESFD: u32 = 1;

/// Userspace `struct iocb` exactly as `libaio.so.1` hands it to
/// `io_submit` (NOT the kernel `linux/aio_abi.h` shape). Layout pinned
/// by the size/offset assertions in `tests/aio_glue_tests.rs` — drift
/// here is memory corruption inside a host application.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RawIocb {
    /// Completion cookie, returned verbatim in `io_event.data`.
    pub data: u64,
    pub key: u32,
    /// Per-op `RWF_*` flags (libaio ≥ 0.3.111 `aio_rw_flags`).
    pub aio_rw_flags: u32,
    pub aio_lio_opcode: u16,
    pub aio_reqprio: u16,
    pub aio_fildes: i32,
    // -- union u.c (io_iocb_common), the only arm the screen reads --
    pub buf: u64,
    pub nbytes: u64,
    pub offset: i64,
    pub __pad3: i64,
    pub flags: u32,
    pub resfd: u32,
}

/// Userspace `struct io_event` as `io_getevents` returns it.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RawIoEvent {
    /// The submitted iocb's `data` cookie.
    pub data: u64,
    /// The ORIGINAL iocb pointer.
    pub obj: u64,
    /// Bytes transferred or negative errno.
    pub res: i64,
    pub res2: i64,
}

// ---------------------------------------------------------------------------
// eligibility screen
// ---------------------------------------------------------------------------

/// The glue-level ladder in front of [`classify_iocb`]: everything the
/// classifier cannot see about ONE iocb. `rights` mirrors the fd-table
/// binding (`None` = fd not bound). Allow-list discipline throughout —
/// any shape not positively known ring-servable rides the kernel lane
/// (§5.4.2 fallback-is-correctness; a wrong `Kernel` costs latency, a
/// wrong `Ring` costs semantics).
///
/// [`classify_iocb`]: crate::aio_core::classify_iocb
pub fn screen_iocb(io: &RawIocb, rights: Option<(bool, bool)>, slab: u64) -> IocbClass {
    // Eventfd completion (IOCB_FLAG_RESFD) contracts a kernel-side
    // signal on completion — silently dropping it hangs epoll-driven
    // reactors.
    if io.flags & IOCB_FLAG_RESFD != 0 {
        return IocbClass::Kernel;
    }
    // Per-op RWF flags follow the sync interposers' allow-list law
    // (RWF_HIPRI is a semantics-free hint; SYNC/DSYNC/APPEND/NOWAIT and
    // any unknown future flag demand kernel semantics).
    if rwf_passthrough(io.aio_rw_flags as libc::c_int) {
        return IocbClass::Kernel;
    }
    // One async ring op occupies exactly one slot — no chunking in
    // v1.1 (a multi-slot op would hold slots hostage across an async
    // completion). Zero-length ops are a kernel-owned edge.
    if io.nbytes == 0 || io.nbytes > slab {
        return IocbClass::Kernel;
    }
    let (bound, read_ok, write_ok) = match rights {
        Some((r, w)) => (true, r, w),
        None => (false, false, false),
    };
    crate::aio_core::classify_iocb(io.aio_lio_opcode, bound, read_ok, write_ok)
}

// ---------------------------------------------------------------------------
// ctx registry
// ---------------------------------------------------------------------------

/// Bounded `io_context_t` capacity. Real applications hold a handful of
/// contexts (fio: one per job); beyond capacity `io_setup` still
/// succeeds kernel-side and the whole context passthroughs — degraded,
/// never broken.
const CTX_CAP: usize = 256;

/// Slot-claim sentinel (never a real ctx value: the kernel's
/// `io_context_t` is a page-aligned ring-mmap address).
const RESERVED: u64 = u64::MAX;

/// `io_context_t` → per-context merge state. Same leak-by-design slot
/// discipline as the session `Registry` and the fd table: state boxes
/// are never freed (`remove` only vacates the id), so a data-path
/// reference can never dangle — using a ctx after `io_destroy` is app
/// UB kernel-side too, and the corpse it would touch here stays valid
/// memory. Re-registering a recycled ctx value allocates FRESH state.
///
/// Generic over the per-context payload so the interposer can co-locate
/// its ticket table with the merge state under ONE per-ctx lock
/// (default = the bare state, the hermetically-tested shape).
pub struct AioCtxRegistry<T = AioCtxState> {
    /// The registered `io_context_t` value; 0 = empty slot (the kernel
    /// never hands out ctx 0 — it is the "uninitialized" sentinel
    /// `io_setup` demands on input).
    ids: [AtomicU64; CTX_CAP],
    states: [AtomicPtr<Mutex<T>>; CTX_CAP],
}

impl<T: Default> Default for AioCtxRegistry<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Default> AioCtxRegistry<T> {
    pub fn new() -> Self {
        Self {
            ids: std::array::from_fn(|_| AtomicU64::new(0)),
            states: std::array::from_fn(|_| AtomicPtr::new(std::ptr::null_mut())),
        }
    }

    /// Register a fresh context. `false` = at capacity (the caller
    /// lets the context passthrough wholesale).
    ///
    /// **Collision retro-neutralization (2026-07-25 field crash
    /// class)**: a colliding register is BY CONSTRUCTION a recycled
    /// REAL context — the kernel never hands out one live ctx value
    /// twice, so an existing entry for this value is a corpse whose
    /// `io_destroy` bookkeeping was missed (guard-refused interposer
    /// entry, panicked destroy body, `real!`-miss early return, libaio
    /// builds whose `io_queue_release` binds `io_destroy` internally).
    /// The stale entry is VACATED and the value re-registered fresh:
    /// adopting the corpse's state (stale pendings / ring tickets)
    /// would divert the new real context off the verbatim-real
    /// passthrough path — synthesized events with dangling iocb
    /// pointers, host-app heap corruption.
    ///
    /// Slot protocol: claim the id `0 → RESERVED`, store the fresh
    /// state, then publish the real ctx value — a lookup can only ever
    /// match an id whose state is already the fresh box (no torn
    /// visibility, no cross-wiring under racing registers).
    pub fn register(&self, ctx: u64) -> bool {
        if ctx == 0 || ctx == RESERVED {
            return false;
        }
        self.remove(ctx);
        for i in 0..CTX_CAP {
            if self.ids[i]
                .compare_exchange(0, RESERVED, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            let fresh = Box::into_raw(Box::new(Mutex::new(T::default())));
            self.states[i].store(fresh, Ordering::Release);
            self.ids[i].store(ctx, Ordering::Release);
            return true;
        }
        false
    }

    /// Resolve a registered context's state. `None` = not ours
    /// (passthrough wholesale).
    pub fn lookup(&self, ctx: u64) -> Option<&Mutex<T>> {
        if ctx == 0 {
            return None;
        }
        for i in 0..CTX_CAP {
            if self.ids[i].load(Ordering::Acquire) != ctx {
                continue;
            }
            let p = self.states[i].load(Ordering::Acquire);
            // Re-check: the slot may have been vacated and re-let
            // between the id match and the state load.
            if !p.is_null() && self.ids[i].load(Ordering::Acquire) == ctx {
                // SAFETY: state boxes are leaked by design (never
                // freed), so the pointer stays valid forever.
                return Some(unsafe { &*p });
            }
        }
        None
    }

    /// Vacate a context (`io_destroy` path — the caller has already
    /// drained/abandoned via [`AioCtxState::destroy`]). `true` = it was
    /// ours. The state box is leaked by design (see type docs).
    pub fn remove(&self, ctx: u64) -> bool {
        if ctx == 0 {
            return false;
        }
        for i in 0..CTX_CAP {
            if self.ids[i]
                .compare_exchange(ctx, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
        false
    }
}
