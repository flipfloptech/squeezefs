//! Session-arena transparent-huge-page economy — near-zero-copy
//! campaign, 2026-07-31 (`.benchmarks/2026-07-31-near-zero-copy.md`).
//!
//! The L4 session shm (sealed memfd, `squeezefs_ipc::layout`) is
//! **shmem**, and shmem THP is policy-gated separately from anonymous
//! THP (`/sys/kernel/mm/transparent_hugepage/shmem_enabled`): `advise`
//! on the dev rig, **`never` on the field fleet** (Rocky 8 default).
//! Anonymous memory (daemon block pools, jemalloc heaps) already rides
//! 2 MiB pages on every fleet host (`enabled=always`), so the arena is
//! the one hot mapping paying 4 KiB dTLB entries under the ring data
//! plane's large copies (app→arena on client cores, the §5.5.2 sever
//! read on service threads).
//!
//! Two best-effort levers, both refusal-tolerant (the shim calls this
//! inside arbitrary applications — a refusal must be invisible):
//!
//! * [`ThpMode::Advise`] — `MADV_HUGEPAGE`: fault-time PMD allocation
//!   where the shmem policy is `advise`/`within_size`/`always`; inert
//!   where it is `never`. Both sides apply it at map time.
//! * [`ThpMode::PopulateCollapse`] — additionally
//!   `MADV_POPULATE_WRITE` then `MADV_COLLAPSE`: **`MADV_COLLAPSE`
//!   operates independent of every sysfs THP setting** (madvise(2)), so
//!   it is THE lever on `shmem_enabled=never` fleets. Populate-first
//!   makes the collapse deterministic (no hole refusals). Daemon-side
//!   only, at session admission — one-time cost off the data path; the
//!   arena's bytes are already budget-charged at full geometry
//!   (`ipc_arena_bytes`), so the eager commit changes when pages fault,
//!   not what R5 sees.
//!
//! Once the daemon's collapse lands, the memfd's PAGE CACHE pages are
//! PMD-sized; the shim's own mapping of the same memfd then maps them
//! huge wherever its vma alignment allows (its `Advise` posture plus
//! the kernel's shmem `get_unmapped_area` alignment).
//!
//! Canonical file in the `squeezefs-ipc` tree, `#[path]`-included by
//! the root crate and `squeezefs-preload` (the `wake_core`
//! production-sharing precedent): the ipc LIBRARY stays
//! dependency-free while both consumers already link libc. Two type
//! identities exist by construction; instances never cross a crate
//! boundary.
//!
//! Env: `SQUEEZEFS_IPC_ARENA_THP=0` disables both sides (A/B lever;
//! read by the call sites, not here — this core is env-free and pure
//! per call).

/// How aggressively to pursue huge pages for a mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThpMode {
    /// `MADV_HUGEPAGE` only (fault-time PMD allocation where policy
    /// allows) — the shim-side posture.
    Advise,
    /// `MADV_HUGEPAGE` + `MADV_POPULATE_WRITE` + `MADV_COLLAPSE` — the
    /// daemon-side session-admission posture (one-time, off the data
    /// path; collapse bypasses the shmem sysfs policy).
    PopulateCollapse,
}

/// Best-effort outcome — refusals are reported, never raised.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThpOutcome {
    /// `MADV_HUGEPAGE` took.
    pub madvise_ok: bool,
    /// `MADV_COLLAPSE` took (PMD backing established synchronously).
    pub collapse_ok: bool,
}

/// `MADV_COLLAPSE` — in libc but named here to keep one obvious source
/// for the constant against older libc pins (value stable in the UAPI).
const MADV_COLLAPSE: libc::c_int = 25;

/// PMD size on the supported fleets (x86_64 huge page).
const PMD_SIZE: usize = 2 * 1024 * 1024;

/// Runtime page size (never a constant — the mapping/`munmap` granularity
/// the kernel actually enforces; MEM-7b's slack math lives in it).
fn page_size() -> usize {
    // SAFETY: `sysconf` with a valid name; a nonpositive answer degrades to
    // the 4 KiB floor rather than poisoning the arithmetic.
    let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if v > 0 {
        v as usize
    } else {
        4096
    }
}

/// `mmap` a shared RW mapping of `fd` (offset 0, `len` bytes) whose base
/// is **PMD-aligned** — the precondition for huge shmem folios to map
/// through PMDs (a huge folio only maps huge when
/// `vaddr ≡ file_offset (mod 2 MiB)`; the census rig's first smoke
/// caught `collapse_ok=true` with `ShmemPmdMapped=0` on an unaligned
/// default-placement base). Implementation: over-reserve `len + 2 MiB`
/// of `PROT_NONE` address space, then `MAP_FIXED` the real mapping at
/// the aligned offset and trim the slack — never racy (the reservation
/// owns the range).
///
/// Returns `None` on any mmap failure (caller falls back to a plain
/// `mmap` — alignment is an optimization, not a correctness need).
///
/// MEM-7b: `len` need NOT be page-aligned. The mapping the kernel creates
/// always covers `round_up(len, page)`, and every address this function
/// hands to `munmap` is derived from that rounded length — a raw
/// `aligned + len` slack address would be refused by the kernel (leaking
/// the whole tail reservation for the process's life), and rounding it DOWN
/// instead would unmap the live tail page of the mapping just created.
/// The returned pointer is valid for `len` bytes (and, as always with mmap,
/// up to the page-rounded length); the CALLER unmaps `round_up(len, page)`.
pub fn map_shared_pmd_aligned(fd: std::os::raw::c_int, len: usize) -> Option<*mut u8> {
    // The kernel's own granularity — do the slack math in it, never in the
    // caller's arbitrary length.
    let page = page_size();
    let map_len = len.checked_next_multiple_of(page)?;
    let span = map_len.checked_add(PMD_SIZE)?;
    // SAFETY: fresh anonymous PROT_NONE reservation.
    let reserve = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            span,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if reserve == libc::MAP_FAILED {
        return None;
    }
    let addr = reserve as usize;
    let aligned = (addr + PMD_SIZE - 1) & !(PMD_SIZE - 1);
    let head = aligned - addr;
    let tail = span - head - map_len;
    // SAFETY: MAP_FIXED inside our own reservation; the fd mapping
    // replaces the PROT_NONE pages atomically.
    let base = unsafe {
        libc::mmap(
            aligned as *mut libc::c_void,
            map_len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_FIXED,
            fd,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        // SAFETY: unmapping our own reservation.
        unsafe { libc::munmap(reserve, span) };
        return None;
    }
    // SAFETY: trimming the slack of our own reservation.
    unsafe {
        if head > 0 {
            libc::munmap(reserve, head);
        }
        if tail > 0 {
            libc::munmap((aligned + map_len) as *mut libc::c_void, tail);
        }
    }
    Some(base as *mut u8)
}

/// Advise/populate/collapse huge pages over `[base, base + len)`.
/// Never fails: every refusal degrades to a `false` in the outcome.
/// `base` must be page-aligned (an mmap result); `len` need not be —
/// the kernel rounds madvise lengths up to page granularity.
pub fn advise_hugepages(base: *mut u8, len: usize, mode: ThpMode) -> ThpOutcome {
    let mut out = ThpOutcome::default();
    if base.is_null() || len == 0 {
        return out;
    }
    let addr = base as *mut libc::c_void;
    // SAFETY: madvise on a caller-supplied range is advisory — on an
    // invalid range the kernel returns ENOMEM/EINVAL, which we report
    // as a refusal; it never touches memory itself.
    out.madvise_ok = unsafe { libc::madvise(addr, len, libc::MADV_HUGEPAGE) } == 0;
    if matches!(mode, ThpMode::PopulateCollapse) {
        // Populate first so the collapse sees no holes. Refusal (old
        // kernel without MADV_POPULATE_WRITE, or a genuinely bad range)
        // just makes the collapse best-effort.
        // SAFETY: as above — advisory, kernel-validated.
        unsafe { libc::madvise(addr, len, libc::MADV_POPULATE_WRITE) };
        // SAFETY: as above; MADV_COLLAPSE is synchronous best-effort
        // and returns nonzero when any PMD range could not collapse.
        out.collapse_ok = unsafe { libc::madvise(addr, len, MADV_COLLAPSE) } == 0;
    }
    out
}
