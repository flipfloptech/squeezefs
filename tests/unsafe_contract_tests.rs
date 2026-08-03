//! MEM-7a–d — the `unsafe` contract gaps (pre-rc-engineering-spec §2
//! MEM-7). Each item is a precondition that is REAL but stated nowhere, or
//! stated and then not enforced where it matters.
//!
//! * **7a** `PlacedSeverRegistry::sever`'s `# Safety` omits the
//!   page-alignment precondition that is the only release-build bound on
//!   the destination write — enforced by `debug_assert!` plus a screen in
//!   the single caller, so a second caller (or any release build) has
//!   nothing. Pinned in `src/placed_sever.rs`'s own test module (the
//!   registry is `pub(crate)`: MEM-4's "prefer not exporting" applies).
//! * **7b** `map_shared_pmd_aligned` assumes a page-aligned `len`; a
//!   non-aligned caller's slack `munmap` lands mid-page, which either
//!   fails (leaking the reservation slack forever) or unmaps a live page.
//!   Sound today only via an implicit cross-crate coupling.
//! * **7d** `std::ptr::read` on an alignment-1 `[u8; N]` for an 8-aligned
//!   `#[repr(C)]` struct is UB by the `read` contract; `read_unaligned` is
//!   the operation actually intended.
//!
//! (7c — `ipc_direct`'s reaper error arm returning with `inflight_count >
//! 0` — is pinned by construction rather than by a test: its shape is "the
//! reaper never returns while kernel DMA can still land in the session
//! mapping", asserted in the module's own unit tests where the ring can be
//! driven; see `src/ipc_direct.rs`.)

/// MEM-7b: a non-page-aligned `len` must leave the mapping's LAST page
/// live AND must not leak the reservation slack.
///
/// The reservation dance trims its slack with
/// `munmap(aligned + len, tail)`. With a page-aligned `len` that address is
/// page-aligned and the trim works. With an unaligned one it is not, so the
/// kernel refuses the call (EINVAL) and the ENTIRE tail — up to 2 MiB of
/// address space per mapping — stays reserved for the process's life; had
/// the code rounded the address DOWN instead, it would have unmapped the
/// live tail page of the mapping it just created. Both halves are the same
/// missing precondition.
#[test]
fn a_pmd_aligned_mapping_handles_a_non_page_aligned_len() {
    const PMD: usize = 2 * 1024 * 1024;
    let len = PMD + 1234; // deliberately not page-aligned
    let fd = memfd(len);
    let base =
        squeezefs::thp::map_shared_pmd_aligned(fd.as_raw_fd(), len).expect("PMD-aligned mapping");
    assert_eq!(
        base as usize % PMD,
        0,
        "the whole point of the reservation dance is PMD alignment"
    );
    // The requested range must be live end to end: a trim that rounded the
    // slack address down would have unmapped the last page.
    // SAFETY: `base` maps `len` bytes of the memfd, RW.
    unsafe {
        std::ptr::write(base, 0x5A);
        std::ptr::write(base.add(len - 1), 0xA5);
        assert_eq!(std::ptr::read(base), 0x5A);
        assert_eq!(
            std::ptr::read(base.add(len - 1)),
            0xA5,
            "MEM-7b: the last page of the live mapping was unmapped by the \
             slack trim"
        );
    }
    // …and the slack past the mapping must be GONE: a refused trim leaks
    // the whole tail reservation.
    let mapped_end = base as usize + len.next_multiple_of(4096);
    assert!(
        !address_is_mapped(mapped_end),
        "MEM-7b: the reservation slack at {mapped_end:#x} is still mapped — \
         the trim's `munmap` address was not page-aligned, so the kernel \
         refused it and up to 2 MiB of address space leaks per mapping"
    );
    // SAFETY: unmapping exactly the mapping created above.
    unsafe {
        libc::munmap(base as *mut libc::c_void, len.next_multiple_of(4096));
    }
}

/// Is `addr` inside any mapping of this process? (`/proc/self/maps` — the
/// only answer that distinguishes "reserved PROT_NONE" from "unmapped".)
fn address_is_mapped(addr: usize) -> bool {
    let maps = std::fs::read_to_string("/proc/self/maps").expect("read maps");
    maps.lines().any(|l| {
        let Some((range, _)) = l.split_once(' ') else {
            return false;
        };
        let Some((lo, hi)) = range.split_once('-') else {
            return false;
        };
        match (usize::from_str_radix(lo, 16), usize::from_str_radix(hi, 16)) {
            (Ok(lo), Ok(hi)) => addr >= lo && addr < hi,
            _ => false,
        }
    })
}

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

fn memfd(len: usize) -> OwnedFd {
    let name = c"mem7b";
    // SAFETY: a valid name pointer and flag set; the fd is adopted below.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
    assert!(fd >= 0, "memfd_create: {}", std::io::Error::last_os_error());
    // SAFETY: fresh owned descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let rc = unsafe { libc::ftruncate(fd.as_raw_fd(), len.next_multiple_of(4096) as libc::off_t) };
    assert_eq!(rc, 0, "ftruncate: {}", std::io::Error::last_os_error());
    fd
}

/// MEM-7d: the GDS ioctl argument struct is read out of a byte array whose
/// alignment is 1. `std::ptr::read` requires the pointer to be aligned for
/// the TARGET type (8 here), so the operation must be `read_unaligned`.
/// Pinned as a source-level law — the arm is `#[cfg(feature = "gds")]`, so
/// a behavioral test would not run in the default gate.
#[test]
fn caller_memory_structs_are_read_unaligned() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(root.join("src/fuse_client.rs")).expect("read");
    let offenders: Vec<(usize, &str)> = text
        .lines()
        .enumerate()
        .filter(|(_, l)| {
            let t = l.trim();
            !t.starts_with("//") && t.contains("std::ptr::read(") && t.contains("as *const ")
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "MEM-7d: `std::ptr::read` through a cast from a byte buffer requires \
         the buffer to be aligned for the target type — use \
         `read_unaligned`:\n{}",
        offenders
            .iter()
            .map(|(i, l)| format!("  fuse_client.rs:{}: {}", i + 1, l.trim()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
