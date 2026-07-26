//! P2 per-op-economy red contract: the moving-custody read fingerprint
//! (fstests generic/795 protocol — fuse_client read handler, layered
//! defense #3) is rebuilt as a compact, alloc-free compare over the shared
//! `block_map` Arc + the per-(ino, block) custody-epoch words, replacing
//! the per-read `Vec<(block, cloned key String, epoch)>` pair whose
//! construction paid TWO moka metadata gets and a String clone per covered
//! block per side (the odirect-randread campaign's named "next CPU term",
//! `.benchmarks/2026-07-25-odirect-randread-concurrency.md` §7).
//!
//! The contract: every movement face the old tuple-vector compare caught,
//! the compact form must catch too — key rebind, binding appear/disappear,
//! custody-epoch bump (the retire face), layout-shape flips — while
//! same-content compares (including a copy-on-write republish of an
//! IDENTICAL map) still match, and bindings OUTSIDE the read window stay
//! out of the verdict exactly as before.

use squeezefs::fuse_client::{bump_block_custody_epoch, ReadCustodyFp};
use squeezefs::routing::CachedMetadata;
use std::collections::HashMap;
use std::sync::Arc;

const BS: u64 = 4 * 1024 * 1024;

fn striped_meta(entries: &[(u32, &str)]) -> CachedMetadata {
    let mut m = HashMap::new();
    for (b, k) in entries {
        m.insert(*b, k.to_string());
    }
    CachedMetadata {
        file_type: "striped".to_string(),
        size: 64 * BS,
        block_map: Some(Arc::new(m)),
        ..Default::default()
    }
}

fn fp(meta: &CachedMetadata, ino: u64, offset: u64, len: usize) -> Option<ReadCustodyFp> {
    ReadCustodyFp::build_sync(meta, ino, BS, offset, len)
}

fn matches(a: &Option<ReadCustodyFp>, b: &Option<ReadCustodyFp>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x.matches(y),
        _ => false,
    }
}

/// Distinct ino space per test: the custody-epoch words are process-global
/// stripes; giving each test its own (ino, block) keys keeps the epoch
/// assertions independent (stripe collisions only ever ADD mismatches,
/// which the same-side rebuild absorbs by re-reading the same words).
fn fresh_ino() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0x7950_0000);
    NEXT.fetch_add(0x100, Ordering::Relaxed)
}

#[test]
fn same_snapshot_matches_via_shared_arc() {
    let ino = fresh_ino();
    let meta = striped_meta(&[(0, "k0"), (1, "k1")]);
    let a = fp(&meta, ino, 0, 4096).expect("striped window fingerprints");
    let b = fp(&meta, ino, 0, 4096).expect("striped window fingerprints");
    assert!(a.matches(&b), "identical snapshot must match");
}

#[test]
fn identical_content_after_cow_republish_matches() {
    // Writers publish copy-on-write: a republish of byte-identical content
    // rides a NEW Arc. Content equality (not pointer equality) is the
    // verdict — a spurious mismatch here would burn a bounded retry per
    // read under any concurrent same-map republish.
    let ino = fresh_ino();
    let meta_a = striped_meta(&[(0, "k0"), (1, "k1")]);
    let meta_b = striped_meta(&[(0, "k0"), (1, "k1")]);
    assert!(!std::ptr::eq(
        Arc::as_ptr(meta_a.block_map.as_ref().unwrap()),
        Arc::as_ptr(meta_b.block_map.as_ref().unwrap())
    ));
    let a = fp(&meta_a, ino, 0, 4096).unwrap();
    let b = fp(&meta_b, ino, 0, 4096).unwrap();
    assert!(a.matches(&b), "byte-identical map content must match");
}

#[test]
fn key_rebind_in_window_differs() {
    let ino = fresh_ino();
    let a = fp(&striped_meta(&[(0, "k0")]), ino, 0, 4096).unwrap();
    let b = fp(&striped_meta(&[(0, "k0-moved")]), ino, 0, 4096).unwrap();
    assert!(!a.matches(&b), "a mid-read block-map rebind must differ");
}

#[test]
fn binding_appearing_in_window_differs() {
    // The write-through publish face: pre-read the block was a hole
    // (map absent for b), post-read it is bound. The old tuple compare
    // caught this as (b, None, e) != (b, Some(k), e).
    let ino = fresh_ino();
    let a = fp(&striped_meta(&[(1, "elsewhere")]), ino, 0, 4096).unwrap();
    let b = fp(&striped_meta(&[(0, "fresh"), (1, "elsewhere")]), ino, 0, 4096).unwrap();
    assert!(!a.matches(&b), "hole → bound inside the window must differ");
}

#[test]
fn binding_vanishing_in_window_differs() {
    let ino = fresh_ino();
    let a = fp(&striped_meta(&[(0, "k0")]), ino, 0, 4096).unwrap();
    let b = fp(&striped_meta(&[(1, "other")]), ino, 0, 4096).unwrap();
    assert!(!a.matches(&b), "bound → hole inside the window must differ");
}

#[test]
fn rebind_outside_window_still_matches() {
    // Only the read window's blocks are fingerprinted (unchanged from the
    // old builder's start..=end walk): movement one block over must not
    // burn this read's retries.
    let ino = fresh_ino();
    let a = fp(&striped_meta(&[(0, "k0"), (7, "far")]), ino, 0, 4096).unwrap();
    let b = fp(&striped_meta(&[(0, "k0"), (7, "far-moved")]), ino, 0, 4096).unwrap();
    assert!(a.matches(&b), "movement outside the window is not this read's business");
}

#[test]
fn custody_epoch_bump_alone_differs() {
    // The retire face (design: every overlay/sibling/record retire bumps
    // the epoch AFTER removal): identical bindings, bumped word ⇒ the
    // reader must re-run. This is the face the binding keys alone cannot
    // see — and the epoch's monotonicity is what kills binding-string ABA.
    let ino = fresh_ino();
    let meta = striped_meta(&[(0, "k0")]);
    let a = fp(&meta, ino, 0, 4096).unwrap();
    bump_block_custody_epoch(ino, 0);
    let b = fp(&meta, ino, 0, 4096).unwrap();
    assert!(!a.matches(&b), "a custody retire inside the window must differ");
}

#[test]
fn epoch_bump_outside_window_matches() {
    let ino = fresh_ino();
    let meta = striped_meta(&[(0, "k0")]);
    let a = fp(&meta, ino, 0, 4096).unwrap();
    bump_block_custody_epoch(ino, 9);
    let b = fp(&meta, ino, 0, 4096).unwrap();
    assert!(a.matches(&b), "a retire on an uncovered block is invisible");
}

#[test]
fn multi_block_window_covers_every_block() {
    let ino = fresh_ino();
    let meta = striped_meta(&[(0, "k0"), (1, "k1"), (2, "k2")]);
    // Window straddles blocks 0..=2.
    let a = fp(&meta, ino, BS - 4096, (2 * BS + 8192) as usize).unwrap();
    bump_block_custody_epoch(ino, 2);
    let b = fp(&meta, ino, BS - 4096, (2 * BS + 8192) as usize).unwrap();
    assert!(!a.matches(&b), "the last covered block is part of the window");
}

#[test]
fn prefix_shape_fingerprints_and_detects_change() {
    let ino = fresh_ino();
    let mk = |prefix: &str| CachedMetadata {
        file_type: "striped".to_string(),
        size: 64 * BS,
        block_prefix: Some(prefix.to_string()),
        ..Default::default()
    };
    let a = fp(&mk("pfx-a"), ino, 0, 4096).expect("prefix shape fingerprints");
    let b = fp(&mk("pfx-a"), ino, 0, 4096).unwrap();
    let c = fp(&mk("pfx-b"), ino, 0, 4096).unwrap();
    assert!(a.matches(&b), "same prefix matches");
    assert!(!a.matches(&c), "prefix change differs");
}

#[test]
fn map_vs_prefix_shape_flip_differs_unless_keys_agree() {
    // A layout flip that lands the SAME derived keys is not movement; one
    // that changes any covered key is. The semantic compare (per-block
    // key + epoch) is the contract, not the representation.
    let ino = fresh_ino();
    let prefix_meta = CachedMetadata {
        file_type: "striped".to_string(),
        size: 64 * BS,
        block_prefix: Some("pfx".to_string()),
        ..Default::default()
    };
    let same_key_map = striped_meta(&[(0, "pfx/part_0")]);
    let other_key_map = striped_meta(&[(0, "moved")]);
    let p = fp(&prefix_meta, ino, 0, 4096).unwrap();
    let m_same = fp(&same_key_map, ino, 0, 4096).unwrap();
    let m_other = fp(&other_key_map, ino, 0, 4096).unwrap();
    assert!(p.matches(&m_same), "same derived key across shapes matches");
    assert!(!p.matches(&m_other), "shape flip with a moved key differs");
}

#[test]
fn non_striped_and_empty_windows_do_not_fingerprint() {
    let ino = fresh_ino();
    let inline = CachedMetadata::default(); // file_type "inline"
    assert!(fp(&inline, ino, 0, 4096).is_none(), "inline has no custody chain");
    let striped = striped_meta(&[(0, "k0")]);
    assert!(fp(&striped, ino, 0, 0).is_none(), "len 0 never fingerprints");
}

#[test]
fn anomalous_map_id_shape_refuses_sync_build() {
    // block_map_id WITHOUT an inline map is the anomalous RAM shape the
    // old builder re-resolved from the backend (load_striped_block_keys).
    // The sync builder must REFUSE it (None) so the caller falls back to
    // the authoritative async path — never fingerprint a shape whose keys
    // it cannot see.
    let ino = fresh_ino();
    let meta = CachedMetadata {
        file_type: "striped".to_string(),
        size: 64 * BS,
        block_map_id: Some("indirect".to_string()),
        ..Default::default()
    };
    assert!(
        fp(&meta, ino, 0, 4096).is_none(),
        "map-id-without-map must defer to the async authoritative builder"
    );
}
