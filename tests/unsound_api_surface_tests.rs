//! MEM-4 — no safe public API may hand safe code a way to trigger UB
//! (pre-rc-engineering-spec §2 MEM-4, P1; two of the three were also
//! flagged independently by clippy's `not_unsafe_ptr_arg_deref`).
//!
//! Three `pub` types shipped all-public fields, safe constructors, and
//! safe accessors that dereference caller-supplied raw pointers:
//!
//! * `cache::pool::UringBufOwner` — `AsRef<[u8]>` builds a slice from
//!   `ptr`/`len` (and it is handed to `Bytes::from_owner`, so the slice
//!   outlives the constructor by design).
//! * `routing::RangedDest` — the §5.6 zero-copy destination; the callee
//!   DMAs into `ptr` for `cap` bytes.
//! * `zcrx_lane::area::AreaSlice` — `as_slice()` over a caller pointer.
//!
//! The requirement is either an `unsafe fn` constructor carrying the
//! contract, or private fields plus `pub(crate)`. This suite is the
//! COMPILE-TIME half: it constructs each type the sanctioned way (inside
//! `unsafe`, with a SAFETY comment) and asserts the resulting view. The
//! "safe code cannot do this" half is enforced by the compiler — a
//! regression that re-exposes a safe constructor makes the `unsafe` blocks
//! below `unused_unsafe` warnings, which `-D warnings` fails.
#![deny(unused_unsafe)]

use squeezefs::routing::RangedDest;

#[test]
fn a_ranged_dest_is_only_constructible_through_its_unsafe_contract() {
    let mut buf = vec![0u8; 4096];
    // SAFETY: `buf` outlives `d`, is 4096 bytes, and is exclusively ours
    // for the test's duration (the §5.6 destination contract).
    let d = unsafe { RangedDest::new(buf.as_mut_ptr(), 4096) };
    assert_eq!(d.cap(), 4096);
    assert_eq!(d.ptr(), buf.as_mut_ptr());
}

#[test]
fn a_uring_buf_owner_view_is_only_constructible_through_its_unsafe_contract() {
    let mut buf = vec![7u8; 64];
    // SAFETY: the view is over `buf`, which outlives the owner; nothing
    // else writes it while the owner lives (the payload-dest contract).
    let owner = unsafe { squeezefs::cache::pool::UringBufOwner::new(buf.as_mut_ptr(), 64) };
    let b = bytes::Bytes::from_owner(owner);
    assert_eq!(b.len(), 64);
    assert!(b.iter().all(|&x| x == 7));
}

#[test]
fn an_area_slice_is_only_constructible_through_its_unsafe_contract() {
    use squeezefs::zcrx_lane::area::{AreaSlice, ZcrxArea};
    let area = ZcrxArea::new(4 * 4096, 4096, None).expect("area map");
    let grant = area.try_grant_chunk().expect("a free chunk");
    let ptr = grant.chunk_ptr();
    // SAFETY: `grant` pins the chunk for the slice's life and `ptr` is its
    // base, so `ptr..ptr+64` is chunk-resident and stable.
    let s = unsafe { AreaSlice::new(grant, ptr as *const u8, 64) };
    assert_eq!(s.len(), 64);
    assert_eq!(s.as_slice().len(), 64);
}
