//! Near-zero-copy campaign (2026-07-31) — contracts for the session-arena
//! huge-page economy (`squeezefs::thp`; canonical file
//! `crates/squeezefs-ipc/src/thp.rs`, `#[path]`-shared with the preload
//! shim per the `wake_core` production-sharing precedent — the ipc
//! LIBRARY stays dependency-free).
//!
//! The census showed both sides of the ring data plane sweep the 64 MiB
//! session arena with large copies (app→arena on the client cores, the
//! §5.5.2 sever read on the service threads) while the arena — a shmem
//! (sealed-memfd) mapping — is 4 KiB-paged on every fleet host: anonymous
//! THP is `always` on both the dev rig and the field (so the daemon's
//! pooled block backings already ride 2 MiB pages), but
//! `shmem_enabled` is `advise` (dev) / `never` (field, Rocky 8 default).
//! The arena is therefore the one hot mapping paying 16k dTLB entries
//! per session.
//!
//! Levers, both best-effort and refusal-tolerant:
//! * `MADV_HUGEPAGE` at map time — live where `shmem_enabled=advise`;
//!   inert (not an error) where policy is `never`.
//! * `MADV_POPULATE_WRITE` + `MADV_COLLAPSE` (daemon side only, at
//!   session admission) — `MADV_COLLAPSE` operates independent of the
//!   sysfs policy, so it is THE field lever; populate first makes the
//!   collapse deterministic instead of hole-refused. One-time admission
//!   cost, never on the data path; the arena's bytes are already
//!   budget-charged at full geometry by `ipc_arena_bytes`.
//!
//! Contracts (each refusal-tolerant assert states which kernel posture
//! it needs — the suite must pass on any Linux):
//! 1. Anonymous mappings take `MADV_HUGEPAGE` and the vma shows the `hg`
//!    VmFlag (THP `always`/`madvise` hosts — both fleet postures).
//! 2. The helper never fails hard: unsupported advice degrades to a
//!    reported-false outcome, never an error/panic (the shim calls this
//!    inside app processes — a refusal must be invisible).
//! 3. On a shmem (memfd) mapping with populate+collapse requested, a
//!    granted collapse is visible as `ShmemPmdMapped > 0` in that vma's
//!    smaps entry (the census rig's engagement instrument).

use squeezefs::thp;

fn smaps_entry_for(base: usize) -> String {
    let smaps = std::fs::read_to_string("/proc/self/smaps").expect("read smaps");
    let mut out = String::new();
    let mut inside = false;
    for line in smaps.lines() {
        // Range lines look like "7f..-7f.. rw-s ...".
        if let Some((range, _rest)) = line.split_once(' ') {
            if let Some((start, _end)) = range.split_once('-') {
                if let Ok(s) = usize::from_str_radix(start, 16) {
                    inside = s == base;
                    if inside {
                        out.clear();
                    }
                }
            }
        }
        if inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[test]
fn anon_mapping_takes_madv_hugepage_and_shows_hg_flag() {
    let len = 8 * 1024 * 1024;
    // SAFETY: fresh anonymous private mapping, unmapped at test end.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(base, libc::MAP_FAILED);
    let outcome = thp::advise_hugepages(base as *mut u8, len, thp::ThpMode::Advise);
    assert!(
        outcome.madvise_ok,
        "MADV_HUGEPAGE on anon memory must succeed on Linux (THP compiled in)"
    );
    let entry = smaps_entry_for(base as usize);
    assert!(
        entry
            .lines()
            .any(|l| l.starts_with("VmFlags:") && l.contains(" hg")),
        "vma must carry the hg flag after MADV_HUGEPAGE; smaps entry:\n{entry}"
    );
    // SAFETY: mapped above.
    unsafe { libc::munmap(base, len) };
}

#[test]
fn helper_is_refusal_tolerant_never_fatal() {
    // A deliberately bogus range: the helper must report false, not
    // error or crash (the shim runs this inside arbitrary apps).
    let outcome = thp::advise_hugepages(4096 as *mut u8, 4096, thp::ThpMode::PopulateCollapse);
    assert!(
        !outcome.madvise_ok && !outcome.collapse_ok,
        "advice on an unmapped range must degrade to reported-false"
    );
}

#[test]
fn shmem_populate_collapse_reports_and_shows_pmd_backing_when_granted() {
    let len = 8 * 1024 * 1024;
    // SAFETY: memfd_create + ftruncate + MAP_SHARED — the session-arena
    // shape (squeezefs_ipc::layout sessions ride a sealed memfd).
    let fd = unsafe { libc::memfd_create(c"thp-test".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0);
    // SAFETY: our own fresh fd.
    assert_eq!(unsafe { libc::ftruncate(fd, len as libc::off_t) }, 0);
    // The PMD-alignment law (found by the census rig's first smoke:
    // collapse_ok=true with ShmemPmdMapped=0): huge folios in the page
    // cache only map through PMDs when the vma base is 2 MiB-aligned
    // (vaddr ≡ file offset mod 2 MiB) — so the session-mapping helper
    // must hand back an aligned base, both sides.
    let base = thp::map_shared_pmd_aligned(fd, len).expect("aligned shared mapping");
    assert_eq!(
        base as usize % (2 * 1024 * 1024),
        0,
        "session mappings must be PMD-aligned or the collapse buys nothing"
    );

    let outcome = thp::advise_hugepages(base, len, thp::ThpMode::PopulateCollapse);
    let entry = smaps_entry_for(base as usize);
    if outcome.collapse_ok {
        let pmd = entry
            .lines()
            .find(|l| l.starts_with("ShmemPmdMapped:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        assert!(
            pmd > 0,
            "collapse_ok must mean PMD-backed shmem is visible; smaps entry:\n{entry}"
        );
    } else {
        // Kernel refused (old kernel / no THP shmem support) — the
        // outcome must say so honestly; the mapping stays fully usable.
        // SAFETY: in-bounds write to the live mapping.
        unsafe { *base = 0xA5 };
        // SAFETY: in-bounds read of the byte just written.
        assert_eq!(unsafe { *(base as *const u8) }, 0xA5);
        eprintln!("note: kernel refused shmem collapse here (posture-dependent); outcome honest");
    }
    // SAFETY: mapped/opened above.
    unsafe {
        libc::munmap(base as *mut libc::c_void, len);
        libc::close(fd);
    }
}
