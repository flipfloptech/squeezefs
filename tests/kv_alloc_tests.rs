//! PR K4 integration tests: the KV extent allocator (design §4.7) — the
//! lock-free claim/release + pending-free + reserve core, the A/B durable
//! bitmap pages, the journaled alloc/free delta records, and the typed
//! ENOSPC surface — over temp files, all I/O through `crate::uring_fs`
//! (io_uring).
//!
//! Contracts pinned (design `docs/design-cow-kv-metadata.md`):
//! - §4.7 allocator decision: flat bitmap, 1 bit per extent; claims are
//!   unique under concurrency (the `alloc_core` `fetch_or` protocol
//!   generalized — the bounded-interleaving proof lives in `loom-models/`,
//!   `tests/run_loom.sh`).
//! - §4.7 ENOSPC semantics: user allocation fails **typed**
//!   ([`KvError::NoSpace`]) with the compaction reserve
//!   (`max(8 extents, 2 %)`) intact; compaction/checkpoint internals keep
//!   allocating from the reserve — no write-to-free-space deadlock.
//! - §4.7 CoW reuse rule (risk R3's invariant): a freed extent enters
//!   pending-free tagged with the checkpoint seq that stops referencing
//!   it and is **never claimable before that seq is durable**; the list
//!   is capped and pressure surfaces typed
//!   ([`KvError::PendingFreeFull`]), forcing a checkpoint rather than
//!   unsafe reuse.
//! - §4.7 durability: A/B bitmap pages (generation-stamped, checksummed,
//!   writer alternates slots, reader takes newest valid — the K3 ledger
//!   selection pattern); every alloc/free is a journal record replayed
//!   over the loaded pages at mount, including pending-free rebuild.
//! - §4.1/§4.2: page images checksummed with bounds-checked lengths;
//!   allocator deltas ride the K3 entry framing under
//!   `TREE_ALLOC_RESERVED` with memcmp-ordered big-endian extent keys.
//!
//! Torn-slot and R3 root-fallback crash semantics live in
//! `tests/crash_contract_tests.rs` (the fault-injection harness); this
//! file covers the clean-path contracts.

use squeezefs::meta_backend::kv::alloc_ext::{
    alloc_record, bitmap_pages_for, bitmap_region_len, compaction_reserve_extents,
    decode_alloc_record, decode_bitmap_page, decode_extent_key, encode_bitmap_page, extent_key,
    free_record, AllocDelta, ExtentAllocator, ALLOC_PAGE_BITS, ALLOC_PAGE_DATA_LEN, ALLOC_PAGE_LEN,
    ALLOC_PAGE_MAGIC, EXTENT_KEY_LEN,
};
use squeezefs::meta_backend::kv::alloc_ext_core::{
    AllocClass, ClaimError, ExtCore, PendingFreeFull,
};
use squeezefs::meta_backend::kv::journal::{
    decode_entry_payload, encode_entry_payload, entry_len_for, JournalRing, ReplayedEntry,
    JOURNAL_PAGE_LEN,
};
use squeezefs::meta_backend::kv::journal_core::AdmissionClass;
use squeezefs::meta_backend::kv::record::{Record, TREE_ALLOC_RESERVED, TREE_INODES};
use squeezefs::meta_backend::kv::KvError;
use squeezefs::uring_fs;
use std::collections::HashSet;
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// A zero-filled temp file sized for a bitmap region of `total_extents`
/// at `base`.
fn bitmap_file(base: u64, total_extents: u64) -> NamedTempFile {
    let f = NamedTempFile::new().expect("temp file");
    f.as_file()
        .set_len(base + bitmap_region_len(total_extents))
        .expect("size bitmap file");
    f
}

/// Admit → reserve → write one journal entry carrying `records`; returns
/// the entry seq. The same three phases K6b's commit pipeline composes.
async fn journal_append(ring: &JournalRing, records: Vec<(u8, Record)>) -> u64 {
    let need = entry_len_for(&records).expect("entry under cap");
    let adm = ring
        .core()
        .try_admit(need, AdmissionClass::User)
        .expect("test ring must have room");
    let res = ring.core().reserve(adm);
    // Restamp record seqs with the reservation seq (the K3 identity).
    let records: Vec<(u8, Record)> = records
        .into_iter()
        .map(|(t, mut r)| {
            r.seq = res.seq();
            (t, r)
        })
        .collect();
    ring.write_entry(&res, &records)
        .await
        .expect("clean entry write");
    res.seq()
}

// ---------------------------------------------------------------------------
// §4.7 policy + record encodings (pure).
// ---------------------------------------------------------------------------

/// The §4.7 compaction reserve: `max(8 extents, 2 % of heap)`.
#[test]
fn test_compaction_reserve_policy() {
    assert_eq!(
        compaction_reserve_extents(1),
        8,
        "floor dominates tiny heaps"
    );
    assert_eq!(compaction_reserve_extents(100), 8, "2 % of 100 = 2 < 8");
    assert_eq!(
        compaction_reserve_extents(400),
        8,
        "2 % of 400 = 8 == floor"
    );
    assert_eq!(compaction_reserve_extents(450), 9, "2 % beats the floor");
    assert_eq!(compaction_reserve_extents(1000), 20);
    // 1 TiB heap of 256 KiB extents = 4 M extents ⇒ 2 % = 81,920.
    assert_eq!(compaction_reserve_extents(4 * 1024 * 1024), 83_886);
}

/// Extent keys are 8-byte big-endian — memcmp order == numeric order
/// (§4.2 key discipline) — and decode is length-guarded (§9).
#[test]
fn test_extent_key_memcmp_order_and_bounds() {
    let mut keys: Vec<[u8; EXTENT_KEY_LEN]> = [0u64, 1, 255, 256, 65_535, 1 << 32, u64::MAX]
        .iter()
        .map(|&e| extent_key(e))
        .collect();
    let sorted_by_bytes = {
        let mut k = keys.clone();
        k.sort();
        k
    };
    keys.sort_by_key(|k| decode_extent_key(k).unwrap());
    assert_eq!(
        keys, sorted_by_bytes,
        "memcmp order must equal numeric order"
    );

    for e in [0u64, 7, u64::MAX] {
        assert_eq!(decode_extent_key(&extent_key(e)).unwrap(), e);
    }
    assert!(
        matches!(decode_extent_key(&[1, 2, 3]), Err(KvError::Corrupt(_))),
        "short keys are rejected, never sliced"
    );
    assert!(matches!(
        decode_extent_key(&[0; 9]),
        Err(KvError::Corrupt(_))
    ));
}

/// Alloc/free delta records: `TREE_ALLOC_RESERVED`-tagged `Put`s that
/// round-trip through the K3 entry framing and decode exactly; malformed
/// values are structural corruption (defense in depth, §9).
#[test]
fn test_alloc_delta_records_roundtrip_and_reject_garbage() {
    let (t_a, rec_a) = alloc_record(7, 42);
    assert_eq!(t_a, TREE_ALLOC_RESERVED);
    assert_eq!(rec_a.seq, 42);
    assert_eq!(rec_a.key, extent_key(7).to_vec());
    assert_eq!(
        decode_alloc_record(&rec_a).unwrap(),
        AllocDelta::Allocated { extent: 7 }
    );

    let (t_f, rec_f) = free_record(9, 3, 43);
    assert_eq!(t_f, TREE_ALLOC_RESERVED);
    assert_eq!(
        decode_alloc_record(&rec_f).unwrap(),
        AllocDelta::Freed {
            extent: 9,
            retire_seq: 3
        }
    );
    // retire_seq 0 = never referenced by any checkpoint (§4.7 wrapper
    // docs): a legal, immediately-reusable free.
    let (_, rec_f0) = free_record(9, 0, 44);
    assert_eq!(
        decode_alloc_record(&rec_f0).unwrap(),
        AllocDelta::Freed {
            extent: 9,
            retire_seq: 0
        }
    );

    // Round-trip through the journal payload framing (§4.2: tree_id +
    // record framing).
    let staged = vec![alloc_record(1, 5), free_record(2, 1, 5)];
    let payload = encode_entry_payload(&staged);
    let back = decode_entry_payload(&payload).expect("payload decodes");
    assert_eq!(back, staged);

    // Garbage values: wrong tag, truncated retire_seq, oversized alloc.
    for bad_value in [vec![], vec![99], vec![2, 1, 2, 3], vec![1, 0]] {
        let rec = Record::put(extent_key(1).to_vec(), 1, bad_value);
        assert!(
            matches!(decode_alloc_record(&rec), Err(KvError::Corrupt(_))),
            "malformed delta value must be Corrupt"
        );
    }
    let rec = Record::put(vec![1, 2], 1, vec![1]);
    assert!(
        matches!(decode_alloc_record(&rec), Err(KvError::Corrupt(_))),
        "malformed extent key must be Corrupt"
    );
}

// ---------------------------------------------------------------------------
// The lock-free core: claims, reserve isolation, pending-free gate.
// ---------------------------------------------------------------------------

/// Claims are unique to exhaustion; user claims stop at the reserve with
/// the budget accounting exact; internal claims drain the reserve; an
/// exhausted heap refuses both (§4.7).
#[test]
fn test_core_claims_unique_reserve_isolated() {
    let core = ExtCore::new(16, 4, 8);
    assert_eq!(core.total(), 16);
    assert_eq!(core.reserve(), 4);
    assert_eq!(core.free_extents(), 16);

    let mut seen = HashSet::new();
    for _ in 0..12 {
        let e = core.claim(AllocClass::User).expect("user budget holds 12");
        assert!(e < 16, "extent {e} out of range");
        assert!(seen.insert(e), "duplicate extent {e}");
    }
    assert_eq!(core.free_extents(), 4, "exactly the reserve remains");
    assert_eq!(
        core.claim(AllocClass::User),
        Err(ClaimError::NoSpace),
        "user claims must never dip into the reserve"
    );

    for _ in 0..4 {
        let e = core
            .claim(AllocClass::Internal)
            .expect("the reserve is internal-claimable");
        assert!(seen.insert(e), "duplicate extent from the reserve");
    }
    assert_eq!(core.free_extents(), 0);
    assert_eq!(
        core.claim(AllocClass::Internal),
        Err(ClaimError::NoSpace),
        "an exhausted heap refuses internal claims too"
    );
    assert_eq!(seen.len(), 16, "every extent claimed exactly once");
}

/// The §4.7 pending-free gate at the core: a freed extent stays
/// unclaimable — even at full ENOSPC pressure — until `advance_durable`
/// covers its tag; stale advances release nothing; the drain is FIFO and
/// stops exactly at the watermark.
#[test]
fn test_core_pending_free_gated_until_durable_seq() {
    let core = ExtCore::new(4, 0, 8);
    let mut claimed = Vec::new();
    for _ in 0..4 {
        claimed.push(core.claim(AllocClass::User).unwrap());
    }
    assert_eq!(core.free_extents(), 0);

    // Free two extents under two future checkpoints: tags 2 then 3
    // (non-decreasing, the §4.6 serialized-task contract).
    core.free_pending(claimed[0], 2).unwrap();
    core.free_pending(claimed[1], 3).unwrap();
    assert_eq!(core.pending_count(), 2);
    assert!(
        core.is_allocated(claimed[0]) && core.is_allocated(claimed[1]),
        "pending extents keep their bits set"
    );
    assert_eq!(
        core.claim(AllocClass::Internal),
        Err(ClaimError::NoSpace),
        "pending extents are not claimable at ANY pressure (§4.7 gate)"
    );

    assert_eq!(
        core.advance_durable(1),
        Vec::<u64>::new(),
        "a watermark below every tag releases nothing"
    );
    assert_eq!(core.durable_seq(), 1);

    // Tag 2 durable: exactly the first entry drains (FIFO stops at 3).
    assert_eq!(core.advance_durable(2), vec![claimed[0]]);
    assert_eq!(core.pending_count(), 1);
    assert_eq!(core.free_extents(), 1);
    let reuse = core.claim(AllocClass::User).unwrap();
    assert_eq!(reuse, claimed[0], "the drained extent is the one reused");

    // Tag 3 durable: the second follows; a repeat advance is a no-op.
    assert_eq!(core.advance_durable(3), vec![claimed[1]]);
    assert_eq!(core.advance_durable(3), Vec::<u64>::new());
    assert_eq!(core.pending_count(), 0);
    assert_eq!(core.durable_seq(), 3);
}

/// The pending-free FIFO cap (§4.7 "capped"): pushes beyond capacity are
/// refused with [`PendingFreeFull`] — pressure forces a checkpoint — and
/// draining reopens the FIFO (slots are recycled across laps).
#[test]
fn test_core_pending_free_cap_and_lap_recycling() {
    let core = ExtCore::new(8, 0, 2);
    let a = core.claim(AllocClass::User).unwrap();
    let b = core.claim(AllocClass::User).unwrap();
    let c = core.claim(AllocClass::User).unwrap();

    core.free_pending(a, 1).unwrap();
    core.free_pending(b, 1).unwrap();
    assert_eq!(
        core.free_pending(c, 1),
        Err(PendingFreeFull),
        "the third pending-free must hit the cap"
    );
    assert!(
        core.is_allocated(c),
        "a refused pending-free leaves the extent claimed (caller retries \
         after a checkpoint)"
    );

    let drained = core.advance_durable(1);
    assert_eq!(drained.len(), 2, "both capped entries drain");
    // The FIFO recycled its slots: a second lap of pushes works.
    core.free_pending(c, 2).unwrap();
    assert_eq!(core.advance_durable(2), vec![c]);
    assert_eq!(core.pending_count(), 0);
}

/// Concurrency hammer at the core (the loom models' std-atomics twin,
/// scaled up): racing claimers never double-allocate across fresh bits
/// and drained pending-free bits, and the budget settles exactly.
#[test]
fn test_core_concurrent_claims_unique_across_pending_drain() {
    use std::sync::Arc;

    let total = 2048u64;
    let core = Arc::new(ExtCore::new(total, 0, 64));

    // Park 32 claimed extents as pending (tag 1), then race: one thread
    // advances the watermark (draining them) while 8 claimers exhaust the
    // heap.
    let mut parked = Vec::new();
    for _ in 0..32 {
        let e = core.claim(AllocClass::User).unwrap();
        core.free_pending(e, 1).unwrap();
        parked.push(e);
    }

    let mut handles = Vec::new();
    for _ in 0..8 {
        let core = Arc::clone(&core);
        handles.push(std::thread::spawn(move || {
            let mut mine = Vec::new();
            while let Ok(e) = core.claim(AllocClass::User) {
                mine.push(e);
            }
            mine
        }));
    }
    let drainer = {
        let core = Arc::clone(&core);
        std::thread::spawn(move || core.advance_durable(1).len() as u64)
    };

    let mut all: Vec<u64> = Vec::new();
    for h in handles {
        all.extend(h.join().unwrap());
    }
    let drained = drainer.join().unwrap();
    assert_eq!(drained, 32, "every parked extent drains exactly once");

    // Claim anything the drain released after the claimers gave up.
    while let Ok(e) = core.claim(AllocClass::User) {
        all.push(e);
    }

    let unique: HashSet<u64> = all.iter().copied().collect();
    assert_eq!(all.len(), unique.len(), "an extent was double-allocated");
    assert_eq!(
        all.len() as u64,
        total,
        "every extent — including each drained pending extent exactly \
         once — ends claimed"
    );
    assert_eq!(core.free_extents(), 0);
}

// ---------------------------------------------------------------------------
// The wrapper: typed ENOSPC, pending pressure, unpublished release.
// ---------------------------------------------------------------------------

/// The wrapper's typed §4.7 surfaces: `claim_user` ENOSPC with the
/// reserve intact ([`KvError::NoSpace`] naming free/reserve),
/// `claim_internal` past it, `free_pending` cap pressure
/// ([`KvError::PendingFreeFull`]), and `release_unpublished` immediate
/// reuse (never-referenced extents skip the gate).
#[test]
fn test_wrapper_typed_enospc_and_pending_pressure() {
    let alloc = ExtentAllocator::format(10, 2, 2);
    assert_eq!(alloc.total_extents(), 10);
    assert_eq!(alloc.reserve_extents(), 2);

    let mut user = Vec::new();
    for _ in 0..8 {
        user.push(alloc.claim_user().expect("user budget holds 8"));
    }
    match alloc.claim_user() {
        Err(KvError::NoSpace { free, reserve }) => {
            assert_eq!((free, reserve), (2, 2), "ENOSPC names the intact reserve");
        }
        other => panic!("expected typed NoSpace, got {other:?}"),
    }
    // The op-level error message is the ENOSPC mapping surface (§4.7:
    // today's analog is "Inode table full" → ENOSPC).
    let msg = alloc.claim_user().unwrap_err().to_string();
    assert!(
        msg.contains("ENOSPC"),
        "NoSpace display names ENOSPC: {msg}"
    );

    let i1 = alloc
        .claim_internal()
        .expect("reserve claimable internally");
    let _i2 = alloc
        .claim_internal()
        .expect("reserve claimable internally");
    assert!(matches!(
        alloc.claim_internal(),
        Err(KvError::NoSpace { free: 0, .. })
    ));

    // Pending cap 2: the third pending-free is typed pressure — the
    // §4.7 "capped" rule surfacing as force-a-checkpoint.
    alloc.free_pending(user[0], 5).unwrap();
    alloc.free_pending(user[1], 5).unwrap();
    match alloc.free_pending(user[2], 5) {
        Err(KvError::PendingFreeFull { pending }) => assert_eq!(pending, 2),
        other => panic!("expected typed PendingFreeFull, got {other:?}"),
    }
    assert_eq!(alloc.pending_count(), 2);

    // Unpublished release: no checkpoint ever referenced i1 — straight
    // back to the pool, immediately claimable.
    alloc.release_unpublished(i1);
    assert!(!alloc.is_allocated(i1));
    assert_eq!(alloc.claim_internal().unwrap(), i1);

    // The parked extents still wait for their durable checkpoint.
    assert!(alloc.is_allocated(user[0]) && alloc.is_allocated(user[1]));
    assert_eq!(alloc.advance_durable(5), 2);
    assert!(!alloc.is_allocated(user[0]) && !alloc.is_allocated(user[1]));
    assert_eq!(alloc.durable_seq(), 5);
}

// ---------------------------------------------------------------------------
// A/B bitmap pages: image framing, alternation, newest-valid selection.
// ---------------------------------------------------------------------------

/// Page images: exact round-trip; magic / misdirected-index / checksum /
/// oversize damage all read as invalid (§4.3 every-unit-checksummed, §9
/// bounds) — never loud at selection.
#[test]
fn test_bitmap_page_image_roundtrip_and_damage_detection() {
    let mut bits = vec![0u8; ALLOC_PAGE_DATA_LEN];
    bits[0] = 0b1010_0101;
    bits[ALLOC_PAGE_DATA_LEN - 1] = 0xFF;
    let image = encode_bitmap_page(3, 9, &bits).expect("encode");
    assert_eq!(image.len(), ALLOC_PAGE_LEN as usize);
    assert_eq!(
        u32::from_le_bytes(image[0..4].try_into().unwrap()),
        ALLOC_PAGE_MAGIC
    );

    let (generation, got_bits) = decode_bitmap_page(&image, 3).expect("decode");
    assert_eq!(generation, 9);
    assert_eq!(got_bits, &bits[..]);

    // Shorter payloads zero-pad.
    let small = encode_bitmap_page(0, 1, &[0xAB]).unwrap();
    let (_, b) = decode_bitmap_page(&small, 0).unwrap();
    assert_eq!(b[0], 0xAB);
    assert!(b[1..].iter().all(|&x| x == 0));

    // Damage classes.
    let mut bad = image.clone();
    bad[0] ^= 0xFF;
    assert!(decode_bitmap_page(&bad, 3).is_err(), "bad magic");
    assert!(
        decode_bitmap_page(&image, 4).is_err(),
        "misdirected page index"
    );
    let mut torn = image.clone();
    torn[100] ^= 0x40;
    assert!(
        matches!(
            decode_bitmap_page(&torn, 3),
            Err(KvError::ChecksumMismatch { .. })
        ),
        "torn bits fail the checksum"
    );
    assert!(
        decode_bitmap_page(&image[..100], 3).is_err(),
        "truncated buffer is bounds-rejected"
    );
    assert!(
        encode_bitmap_page(0, 1, &vec![0; ALLOC_PAGE_DATA_LEN + 1]).is_err(),
        "oversized payload refused at encode"
    );
}

/// The A/B write path: the writer alternates slots per page (never
/// overwriting the newest valid copy), generations stamp
/// newest-valid-wins, and `load` selects accordingly (the K3 ledger
/// pattern at page granularity).
#[tokio::test]
async fn test_bitmap_ab_alternation_newest_valid_selection() {
    let f = bitmap_file(0, 10);
    assert_eq!(bitmap_pages_for(10), 1);

    let alloc = ExtentAllocator::format(10, 0, 4);
    let e0 = alloc.claim_user().unwrap();
    let e1 = alloc.claim_user().unwrap();
    let wrote = alloc.write_dirty_pages(f.path(), 0, 1).await.unwrap();
    assert_eq!(wrote, vec![0], "one dirty page");

    // Generation 1 landed in slot A (fresh page): decode raw.
    let region = uring_fs::read_at(f.path(), 0, bitmap_region_len(10) as usize)
        .await
        .unwrap();
    let (g_a, _) = decode_bitmap_page(&region[..ALLOC_PAGE_LEN as usize], 0).unwrap();
    assert_eq!(g_a, 1, "fresh page writes slot A first");
    assert!(
        decode_bitmap_page(&region[ALLOC_PAGE_LEN as usize..], 0).is_err(),
        "slot B still empty"
    );

    // Second checkpoint alternates to slot B; A keeps generation 1.
    let e2 = alloc.claim_user().unwrap();
    let wrote = alloc.write_dirty_pages(f.path(), 0, 2).await.unwrap();
    assert_eq!(wrote, vec![0]);
    let region = uring_fs::read_at(f.path(), 0, bitmap_region_len(10) as usize)
        .await
        .unwrap();
    let (g_a, _) = decode_bitmap_page(&region[..ALLOC_PAGE_LEN as usize], 0).unwrap();
    let (g_b, _) = decode_bitmap_page(&region[ALLOC_PAGE_LEN as usize..], 0).unwrap();
    assert_eq!((g_a, g_b), (1, 2), "alternation preserved the predecessor");

    // Nothing dirty ⇒ nothing written.
    assert_eq!(
        alloc.write_dirty_pages(f.path(), 0, 3).await.unwrap(),
        Vec::<u32>::new()
    );

    // Load: newest valid (generation 2, slot B) wins; state matches.
    let loaded = ExtentAllocator::load(f.path(), 0, 10, 0, 4, 0, &[])
        .await
        .unwrap();
    for e in [e0, e1, e2] {
        assert!(loaded.is_allocated(e), "extent {e} lost by selection");
    }
    assert_eq!(loaded.free_extents(), 7);
    assert_eq!(
        loaded.resume_generation(),
        2,
        "mount resumes generation numbering above the newest on-disk"
    );

    // Third checkpoint from the loaded allocator alternates back onto
    // slot A (generation 3 > 1 replaced there; B keeps 2).
    let _e3 = loaded.claim_user().unwrap();
    loaded.write_dirty_pages(f.path(), 0, 3).await.unwrap();
    let region = uring_fs::read_at(f.path(), 0, bitmap_region_len(10) as usize)
        .await
        .unwrap();
    let (g_a, _) = decode_bitmap_page(&region[..ALLOC_PAGE_LEN as usize], 0).unwrap();
    let (g_b, _) = decode_bitmap_page(&region[ALLOC_PAGE_LEN as usize..], 0).unwrap();
    assert_eq!(
        (g_a, g_b),
        (3, 2),
        "loaded allocator alternates off the newest"
    );
}

/// Multi-page heaps: `format` starts every page dirty (first checkpoint
/// persists the whole bitmap); after that only touched pages write; page
/// boundary extents land in the right page.
#[tokio::test]
async fn test_multi_page_dirty_tracking_and_boundaries() {
    let total = ALLOC_PAGE_BITS + 10; // 2 pages
    assert_eq!(bitmap_pages_for(total), 2);
    let f = bitmap_file(0, total);

    let alloc = ExtentAllocator::format(total, 0, 4);
    assert_eq!(
        alloc.write_dirty_pages(f.path(), 0, 1).await.unwrap(),
        vec![0, 1],
        "a fresh volume persists every page"
    );

    // Dirty only page 1 (an extent past the boundary) via pending-free
    // machinery: claim it… claims scan low-first, so force the boundary
    // extent through the released-hint path instead: mark by claiming the
    // exact low extents first is O(page) — instead claim, park, drain.
    let mut e = alloc.claim_user().unwrap();
    while e < ALLOC_PAGE_BITS {
        // Walk claims up to the boundary; cheap bit ops (µs range).
        e = alloc.claim_user().unwrap();
    }
    assert_eq!(e, ALLOC_PAGE_BITS, "first extent of page 1");
    let wrote = alloc.write_dirty_pages(f.path(), 0, 2).await.unwrap();
    assert_eq!(wrote, vec![0, 1], "claims dirtied both pages on the walk");

    alloc.free_pending(e, 1).unwrap();
    assert_eq!(alloc.advance_durable(1), 1);
    assert_eq!(
        alloc.write_dirty_pages(f.path(), 0, 3).await.unwrap(),
        vec![1],
        "the released boundary extent dirties only its own page"
    );

    // Round-trip: loaded occupancy matches (page-1 boundary extent free
    // again, everything below the boundary allocated).
    let loaded = ExtentAllocator::load(f.path(), 0, total, 0, 4, 1, &[])
        .await
        .unwrap();
    assert!(!loaded.is_allocated(e));
    assert!(loaded.is_allocated(0));
    assert!(loaded.is_allocated(ALLOC_PAGE_BITS - 1));
    assert_eq!(loaded.free_extents(), total - ALLOC_PAGE_BITS);
}

/// A short/absent bitmap region loads as a fresh, all-free heap (zeros
/// verify nothing — the K3 short-read discipline), never loud.
#[tokio::test]
async fn test_load_short_region_reads_fresh() {
    let f = NamedTempFile::new().unwrap();
    f.as_file().set_len(100).unwrap(); // far short of one slot
    let alloc = ExtentAllocator::load(f.path(), 0, 64, 8, 4, 0, &[])
        .await
        .unwrap();
    assert_eq!(alloc.free_extents(), 64);
    assert_eq!(alloc.resume_generation(), 0);
    assert!(!alloc.is_allocated(0));
}

// ---------------------------------------------------------------------------
// Journaled deltas: mount = pages + replay (§4.7 "Mount" rule).
// ---------------------------------------------------------------------------

/// The §4.7 mount rule end-to-end over one file: newest-valid pages, then
/// journal alloc/free records ≥ tail replayed over them — allocs
/// re-marked, gate-passed frees released, still-gated frees rebuilt as
/// pending (never claimable before their checkpoint-durable seq), LWW per
/// key across alloc→free→realloc chains.
#[tokio::test]
async fn test_journal_replay_rebuilds_alloc_free_and_pending() {
    // One file: ring pages [0, 4·4096), bitmap region after.
    let ring_pages = 4u64;
    let bitmap_base = ring_pages * JOURNAL_PAGE_LEN;
    let total = 16u64;
    let f = NamedTempFile::new().unwrap();
    f.as_file()
        .set_len(bitmap_base + bitmap_region_len(total))
        .unwrap();

    let ring = JournalRing::new(f.path(), 0, ring_pages, 0);
    let alloc = ExtentAllocator::format(total, 2, 8);

    // Checkpoint 1's durable past: e0, e1 allocated and persisted in
    // pages; ledger seq 1 assumed durable (the test's mounted_seq).
    let e0 = alloc.claim_internal().unwrap();
    let e1 = alloc.claim_internal().unwrap();
    journal_append(&ring, vec![alloc_record(e0, 0), alloc_record(e1, 0)]).await;
    alloc
        .write_dirty_pages(f.path(), bitmap_base, 1)
        .await
        .unwrap();

    // Post-checkpoint mutations that live ONLY in the journal (the §4.7
    // "bitmap is a checkpoint accelerator, not the sole truth" window):
    // - e2 allocated;
    // - e0 freed under checkpoint 1 (tag 1 ≤ mounted ⇒ released at load);
    // - e1 freed under checkpoint 2 (tag 2 > mounted ⇒ pending at load);
    // - e3 allocated then freed-unreferenced (tag 0) then re-allocated:
    //   the per-key LWW chain.
    let e2 = alloc.claim_internal().unwrap();
    journal_append(&ring, vec![alloc_record(e2, 0)]).await;
    journal_append(&ring, vec![free_record(e0, 1, 0)]).await;
    journal_append(&ring, vec![free_record(e1, 2, 0)]).await;
    let e3 = alloc.claim_internal().unwrap();
    journal_append(
        &ring,
        vec![
            alloc_record(e3, 0),
            free_record(e3, 0, 0),
            alloc_record(e3, 0),
        ],
    )
    .await;

    // "Remount": recover the ring, then load pages + replay window.
    let (_ring2, recovery) = JournalRing::recover(f.path(), 0, ring_pages, 0, 0)
        .await
        .unwrap();
    assert_eq!(recovery.dropped_torn, 0, "clean shutdown replays clean");
    let loaded = ExtentAllocator::load(
        f.path(),
        bitmap_base,
        total,
        2,
        8,
        1, // mounted ledger seq: checkpoint 1 is the durability floor
        &recovery.entries,
    )
    .await
    .unwrap();

    assert!(!loaded.is_allocated(e0), "tag ≤ mounted ⇒ released at load");
    assert!(
        loaded.is_allocated(e1),
        "tag > mounted ⇒ rebuilt as pending, bit set"
    );
    assert_eq!(loaded.pending_count(), 1, "exactly e1 parked");
    assert!(loaded.is_allocated(e2), "journal-only alloc re-marked");
    assert!(
        loaded.is_allocated(e3),
        "LWW: alloc→free→realloc ends allocated"
    );

    // e1 is not claimable at any pressure until checkpoint 2 is durable.
    let mut claimed = HashSet::new();
    loop {
        match loaded.claim_internal() {
            Ok(e) => {
                assert_ne!(
                    e, e1,
                    "pending extent handed out before its durable seq (R3 gate)"
                );
                assert!(claimed.insert(e), "double-allocated extent {e}");
            }
            Err(KvError::NoSpace { .. }) => break,
            Err(other) => panic!("unexpected claim error: {other}"),
        }
    }
    assert_eq!(
        claimed.len() as u64,
        total - 3,
        "everything except {{e1 pending, e2, e3 allocated}} claims exactly once"
    );

    // Post-mount checkpoint 2 becomes durable: e1 drains and is claimable.
    assert_eq!(loaded.advance_durable(2), 1);
    assert!(!loaded.is_allocated(e1));
    assert_eq!(loaded.claim_internal().unwrap(), e1);
}

/// Replayed garbage in a `TREE_ALLOC_RESERVED` record is structural
/// corruption (the entry checksum already verified — writer-bug defense,
/// §9): `load` fails loud rather than guessing allocator state.
#[tokio::test]
async fn test_load_rejects_malformed_replayed_delta() {
    let total = 8u64;
    let f = bitmap_file(0, total);
    let bad = ReplayedEntry {
        seq: 0,
        records: vec![(
            TREE_ALLOC_RESERVED,
            Record::put(extent_key(1).to_vec(), 0, vec![9, 9]),
        )],
    };
    let err = ExtentAllocator::load(f.path(), 0, total, 0, 4, 0, &[bad])
        .await
        .expect_err("malformed delta must fail loud");
    assert!(matches!(err, KvError::Corrupt(_)), "got {err:?}");

    // Non-allocator trees in the window are ignored (mixed entries).
    let mixed = ReplayedEntry {
        seq: 0,
        records: vec![(TREE_INODES, Record::put(vec![1], 0, vec![2]))],
    };
    let alloc = ExtentAllocator::load(f.path(), 0, total, 0, 4, 0, &[mixed])
        .await
        .expect("foreign trees ignored");
    assert_eq!(alloc.free_extents(), total);
}
