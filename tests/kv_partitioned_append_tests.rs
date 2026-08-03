//! **Multi-writer append partitioning** — the three §6.2 single-appender
//! durable-format assumptions, broken (pre-RC engineering spec §6.2 items
//! 2/3/4; execution-plan rulings **D8**/**D9**; design
//! `docs/design-cow-kv-metadata.md` §4.1/§4.4/§4.6/§4.7/§4.10).
//!
//! What this file pins — and what it deliberately does NOT:
//!
//! | Item | Assumption today | Partitioned form |
//! |---|---|---|
//! | §6.2 #2 | ONE journal ring head per volume — a second committer's pages classify as **torn and are silently dropped** | one sub-ring per appender + a replay **merge** that reconstructs a correct order, and a foreign appender's pages are DETECTED (`foreign_pages`), never mistaken for a tear |
//! | §6.2 #3 | ONE A/B extent bitmap + ONE `advance_durable` tail (whole-volume single-appender) | bitmap **pages** partitioned per appender: per-partition free budget, hint, pending-free FIFO, and durable tail |
//! | §6.2 #4 | ONE A/B root ledger, `slot = seq % 32`, newest-valid-wins — two checkpointers overwrite each other | per-appender **slot ranges** (≥ 2 slots each, so the torn-newest-slot fallback survives per writer) |
//!
//! **NOT built here** (spec §6.9 S4/S8's problem): who may append, how
//! appenders are admitted, and the partitioning that keeps them disjoint.
//! These tests only prove the *formats* are expressible for N appenders
//! and that a partitioning violation is refused **loud** rather than
//! silently dropped.
//!
//! Ruling **D9** compatibility posture: everything rides
//! `FEATURE_INCOMPAT_KV_PARTITIONED_APPEND`, which is **built but NOT
//! stamped** — `SuperblockV3::plan` never sets it, mount never sets it,
//! and an un-stamped volume's structures are byte-identical to today
//! (pinned by `solo_*` cases here and the superblock-unchanged mount
//! case at the end).

use squeezefs::meta_backend::kv::alloc_ext::{
    alloc_record, free_record, ExtentAllocator, ALLOC_PAGE_BITS,
};
use squeezefs::meta_backend::kv::alloc_ext_core::{AllocClass, ExtCore, PartitionMap};
use squeezefs::meta_backend::kv::checkpoint::{
    ledger_slot_for, ledger_slots_per_writer, read_newest_ledger, read_partitioned_ledger,
    write_ledger_slot, LedgerRecord, TreeRoot, ROOT_LEDGER_HDR_LEN, ROOT_LEDGER_MAGIC,
    ROOT_LEDGER_SLOTS, ROOT_LEDGER_SLOT_LEN,
};
use squeezefs::meta_backend::kv::journal::{
    detect_partition_violations, entry_len_for, merge_replay_windows, page_header_image,
    partition_ring_base, partition_ring_pages, partitioned_ring_geometry_ok, replay_merge, tag_for,
    AppendPartition, JournalRecovery, JournalRing, MergedEntry, PartitionViolation,
    JOURNAL_PAGE_HDR_LEN, JOURNAL_PAGE_LEN, JOURNAL_PAGE_MAGIC, MAX_ENTRY_LEN,
};
use squeezefs::meta_backend::kv::journal_core::AdmissionClass;
use squeezefs::meta_backend::kv::record::{
    inode_key, InodeValue, Record, TREE_DENTRIES, TREE_INODES,
};
use squeezefs::uring_fs;
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// A zero-filled temp file big enough for `pages` journal pages at `base`.
fn ring_file(base: u64, pages: u64) -> NamedTempFile {
    let f = NamedTempFile::new().expect("temp file");
    f.as_file()
        .set_len(base + pages * JOURNAL_PAGE_LEN)
        .expect("size ring file");
    f
}

fn put(ino: u64, seq: u64) -> (u8, Record) {
    let v = InodeValue {
        mode: 0o100644,
        nlink: 1,
        mtime: seq,
        ctime: seq,
        ..Default::default()
    };
    (
        TREE_INODES,
        Record::put(inode_key(ino).to_vec(), seq, v.encode()),
    )
}

/// Admit → reserve → write one entry into `ring`; returns its seq.
async fn append(ring: &JournalRing, records: &[(u8, Record)]) -> u64 {
    let len = entry_len_for(records).expect("entry fits the cap");
    let adm = ring
        .try_admit(len, AdmissionClass::User)
        .expect("ring has space");
    let res = ring.reserve_registered(adm);
    ring.commit_entry(&res, records)
        .await
        .expect("entry write must succeed");
    res.seq()
}

fn part(writers: u16, writer_id: u16) -> AppendPartition {
    AppendPartition::new(writers, writer_id).expect("legal partition")
}

fn ledger_rec(seq: u64, partition: Option<AppendPartition>, roots: usize) -> LedgerRecord {
    LedgerRecord {
        seq,
        tree_roots: (0..roots)
            .map(|i| TreeRoot {
                tree_id: [TREE_INODES, TREE_DENTRIES][i.min(1)],
                node_addr: 0x1000 + seq,
                node_seq: 7 + seq,
            })
            .collect(),
        journal_tail_seq: 10 * seq,
        next_ino: 100 + seq,
        alloc_bitmap_generation: seq,
        node_seq_watermark: 100 + seq,
        membership_stamp: None,
        append_partition: partition,
    }
}

// ===========================================================================
// §6.2 item 2 — the journal ring: per-appender sub-rings.
// ===========================================================================

/// The partition descriptor's legality rules, each with a reason:
/// `writers ≥ 1`, `writer_id < writers`, `writers ≤ MAX_APPENDERS` (the
/// 32-slot ledger must leave ≥ 2 slots per writer so the torn-newest-slot
/// fallback survives per appender), and — the shipped posture — solo is
/// `writers == 1, writer_id == 0`.
#[test]
fn test_append_partition_legality() {
    assert!(AppendPartition::SOLO.is_solo());
    assert_eq!(AppendPartition::SOLO.writers(), 1);
    assert_eq!(AppendPartition::SOLO.writer_id(), 0);
    assert!(
        AppendPartition::SOLO.is_root_authority(),
        "writer 0 is the structural (tree-root) authority"
    );

    for (writers, id) in [(2u16, 0u16), (2, 1), (4, 3), (16, 15)] {
        let p = AppendPartition::new(writers, id).expect("legal");
        assert_eq!((p.writers(), p.writer_id()), (writers, id));
        assert!(!p.is_solo());
        assert_eq!(p.is_root_authority(), id == 0);
    }

    for (writers, id) in [(0u16, 0u16), (1, 1), (2, 2), (3, 0), (32, 0), (17, 0)] {
        assert!(
            AppendPartition::new(writers, id).is_err(),
            "writers={writers} writer_id={id} must refuse loud"
        );
    }
}

/// Sub-ring placement: writer regions are disjoint, equal, in writer
/// order, and inside the journal extent. Solo is the WHOLE extent —
/// byte-for-byte today's ring.
#[test]
fn test_sub_ring_placement_disjoint_and_solo_is_whole_extent() {
    const BASE: u64 = 4096;
    const PAGES: u64 = 2048; // the 8 MiB clamp floor.

    assert_eq!(
        partition_ring_pages(PAGES, 1),
        PAGES,
        "solo owns every page"
    );
    assert_eq!(
        partition_ring_base(BASE, PAGES, AppendPartition::SOLO),
        BASE,
        "solo starts at the journal extent's base"
    );

    for writers in [2u16, 4, 8, 16] {
        let per = partition_ring_pages(PAGES, writers);
        assert_eq!(per, PAGES / u64::from(writers));
        let mut prev_end = BASE;
        for id in 0..writers {
            let b = partition_ring_base(BASE, PAGES, part(writers, id));
            assert_eq!(
                b, prev_end,
                "writer {id}'s sub-ring must abut its predecessor"
            );
            prev_end = b + per * JOURNAL_PAGE_LEN;
        }
        assert_eq!(
            prev_end,
            BASE + PAGES * JOURNAL_PAGE_LEN,
            "the partition must exactly cover the journal extent"
        );
    }
}

/// The format-time sizing law: every sub-ring must be able to hold the
/// largest legal transaction ([`MAX_ENTRY_LEN`]) — a writer whose sub-ring
/// cannot is a latent admission wedge (it parks forever on space it can
/// never have). The shipped 8 MiB floor holds up to 16 appenders.
#[test]
fn test_partitioned_ring_geometry_preflight() {
    const FLOOR_PAGES: u64 = 8 * 1024 * 1024 / JOURNAL_PAGE_LEN; // 2048
    for writers in [1u16, 2, 4, 8, 16] {
        partitioned_ring_geometry_ok(FLOOR_PAGES, writers)
            .unwrap_or_else(|e| panic!("8 MiB ring must serve {writers} appenders: {e}"));
    }
    // The smallest legal SOLO ring (the `SuperblockV3::plan` override
    // floor: one checkpoint reserve + one max entry ≈ 384 KiB) cannot
    // serve two appenders — each sub-ring would be half of a ring that was
    // already at the floor.
    let floor_ring = 97; // 97 × 4,072 B ≥ 256 KiB + 128 KiB
    assert!(partitioned_ring_geometry_ok(floor_ring, 1).is_ok());
    let err = partitioned_ring_geometry_ok(floor_ring, 2).expect_err("must refuse loud");
    let msg = format!("{err}");
    assert!(
        msg.contains("128") || msg.contains(&MAX_ENTRY_LEN.to_string()),
        "the refusal must name the entry cap it cannot hold: {msg}"
    );
}

/// **Un-stamped byte identity (ruling D9).** A solo page header is
/// byte-for-byte the shipped image: magic, lap, `first_entry_off`, four
/// explicit zero bytes, then the xxh3 over `[0..16)`. The appender id
/// lives in two of those pad bytes, so writer 0 stamps the same zeros
/// today's code does.
#[test]
fn test_solo_page_header_bytes_are_unchanged() {
    let lap = 3u32;
    let feo = JOURNAL_PAGE_HDR_LEN as u16;

    let mut expect = [0u8; JOURNAL_PAGE_HDR_LEN as usize];
    expect[0..4].copy_from_slice(&JOURNAL_PAGE_MAGIC.to_le_bytes());
    expect[4..8].copy_from_slice(&lap.to_le_bytes());
    expect[8..10].copy_from_slice(&feo.to_le_bytes());
    // [10..16) stays zero: `_pad`, then the appender id (0 = solo), then
    // the alignment pad.
    let sum = xxhash_rust::xxh3::xxh3_64(&expect[0..16]);
    expect[16..24].copy_from_slice(&sum.to_le_bytes());

    assert_eq!(
        page_header_image(lap, feo, 0),
        expect,
        "a solo (writer 0) page header must be byte-identical to the shipped image"
    );
    assert_ne!(
        page_header_image(lap, feo, 1),
        expect,
        "a second appender's pages must be distinguishable on disk"
    );
}

/// **The headline case.** Two independent appenders, each with its own
/// sub-ring of ONE journal extent: both windows recover, and the merge
/// reconstructs a correct order carrying every entry with its appender
/// identity. Today's shared ring cannot express this — the loser's pages
/// fail the lap/seq identity and are dropped as torn.
#[tokio::test]
async fn test_two_appenders_both_survive_replay() {
    const PAGES: u64 = 8;
    let f = ring_file(0, PAGES);
    let (p0, p1) = (part(2, 0), part(2, 1));

    let r0 = JournalRing::new_in_partition(f.path(), 0, PAGES, 0, p0);
    let r1 = JournalRing::new_in_partition(f.path(), 0, PAGES, 0, p1);
    assert_eq!(r0.partition(), p0);
    assert_eq!(r1.partition(), p1);

    // Disjoint objects (the partitioning contract): writer 0 owns even
    // inos, writer 1 owns odd ones.
    let mut w0 = Vec::new();
    let mut w1 = Vec::new();
    for i in 0..5u64 {
        w0.push(append(&r0, &[put(2 * i + 2, 100 + i)]).await);
        w1.push(append(&r1, &[put(2 * i + 3, 200 + i)]).await);
    }

    let (_, rec0) = JournalRing::recover_in_partition(f.path(), 0, PAGES, 0, 0, p0)
        .await
        .expect("writer 0 window");
    let (_, rec1) = JournalRing::recover_in_partition(f.path(), 0, PAGES, 0, 0, p1)
        .await
        .expect("writer 1 window");
    assert_eq!(rec0.entries.len(), 5, "writer 0's entries must all recover");
    assert_eq!(rec1.entries.len(), 5, "writer 1's entries must all recover");
    assert_eq!((rec0.dropped_torn, rec1.dropped_torn), (0, 0));
    assert_eq!(
        (rec0.foreign_pages, rec1.foreign_pages),
        (0, 0),
        "neither appender may see the other's pages in its own sub-ring"
    );

    let merged =
        replay_merge(vec![(p0, rec0), (p1, rec1)], None).expect("a partitioned window must merge");
    assert_eq!(merged.entries.len(), 10);
    for (writer, seqs) in [(0u16, &w0), (1u16, &w1)] {
        let got: Vec<u64> = merged
            .entries
            .iter()
            .filter(|e| e.writer_id == writer)
            .map(|e| e.seq)
            .collect();
        assert_eq!(&got, &seqs.to_vec());
    }
}

/// The merge order is **deterministic** (replay twice ⇒ identical state,
/// §4.10) and **preserves per-ring order** — which is the whole
/// correctness requirement under partitioning: per-key LWW-by-seq folds
/// identically under any interleaving of disjoint keyspaces, so the merge
/// needs a total order only for determinism, not for correctness.
#[test]
fn test_merge_is_deterministic_and_preserves_per_ring_order() {
    let (p0, p1) = (part(2, 0), part(2, 1));
    let mk = |seqs: &[u64], ino_base: u64| JournalRecovery {
        entries: seqs
            .iter()
            .map(|s| squeezefs::meta_backend::kv::journal::ReplayedEntry {
                seq: *s,
                records: vec![put(ino_base + *s, *s)],
            })
            .collect(),
        head_pos: seqs.last().copied().unwrap_or(0) + 1,
        dropped_torn: 0,
        foreign_pages: 0,
    };

    let a = merge_replay_windows(vec![(p0, mk(&[0, 40, 900], 2)), (p1, mk(&[5, 40, 41], 3))]);
    let b = merge_replay_windows(vec![(p1, mk(&[5, 40, 41], 3)), (p0, mk(&[0, 40, 900], 2))]);
    let key = |m: &squeezefs::meta_backend::kv::journal::MergedReplay| {
        m.entries
            .iter()
            .map(|e| (e.writer_id, e.seq))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        key(&a),
        key(&b),
        "the merge must not depend on the order the windows are handed in"
    );
    for w in [0u16, 1] {
        let seqs: Vec<u64> = a
            .entries
            .iter()
            .filter(|e| e.writer_id == w)
            .map(|e| e.seq)
            .collect();
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        assert_eq!(
            seqs, sorted,
            "writer {w}'s own order must survive the merge"
        );
    }
    // Solo windows merge to exactly today's seq-sorted single window.
    let solo = merge_replay_windows(vec![(AppendPartition::SOLO, mk(&[0, 7, 90], 2))]);
    assert_eq!(
        solo.entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![0, 7, 90]
    );
    assert!(solo.entries.iter().all(|e| e.writer_id == 0));
}

/// A foreign appender's page inside my sub-ring is **detected**, not
/// silently classified as a tear. This is the dangerous half of §6.2 #2:
/// today a second writer does not fail loudly, it loses transactions.
#[tokio::test]
async fn test_foreign_appender_page_is_detected_not_silently_dropped() {
    const PAGES: u64 = 4;
    let f = ring_file(0, PAGES);
    let p0 = part(2, 0);
    let r0 = JournalRing::new_in_partition(f.path(), 0, PAGES, 0, p0);
    let seq0 = append(&r0, &[put(2, 100)]).await;

    // Forge writer 1's header over writer 0's page 1 and plant a
    // checksum-valid entry behind it — a would-be second appender that
    // shared writer 0's ring.
    let sub_base = partition_ring_base(0, PAGES, p0);
    let per = partition_ring_pages(PAGES, 2);
    assert!(per >= 2, "the test needs a 2-page sub-ring");
    let hdr = page_header_image(0, JOURNAL_PAGE_HDR_LEN as u16, 1);
    uring_fs::write_at(f.path(), sub_base + JOURNAL_PAGE_LEN, hdr.to_vec())
        .await
        .expect("plant the foreign header");

    let (_, rec0) = JournalRing::recover_in_partition(f.path(), 0, PAGES, 0, 0, p0)
        .await
        .expect("the ring scan itself never fails loud on ring contents (§4.1)");
    assert!(
        rec0.entries.iter().any(|e| e.seq == seq0),
        "writer 0's own entry must still recover"
    );
    assert_eq!(
        rec0.foreign_pages, 1,
        "a foreign appender's page must be counted as such, never folded \
         into the torn census"
    );

    // The volume-level decision IS loud: the merge refuses.
    let err = replay_merge(vec![(p0, rec0)], None)
        .expect_err("a foreign appender's pages must refuse the merge loud");
    let msg = format!("{err}");
    assert!(
        msg.contains("foreign") && msg.contains("appender"),
        "the refusal must name the mechanism: {msg}"
    );
}

/// A partitioning violation — the same object touched by two appenders in
/// one window — is refused LOUD, naming both writers and the key. This is
/// the case a merge cannot decide: with disjoint keyspaces any
/// interleaving folds identically, so an overlap means the merge has no
/// correct order to reconstruct.
#[test]
fn test_partition_violation_same_key_two_writers_refused_loud() {
    let (p0, p1) = (part(2, 0), part(2, 1));
    let shared = put(42, 7);
    let w = |seq: u64, r: (u8, Record)| squeezefs::meta_backend::kv::journal::ReplayedEntry {
        seq,
        records: vec![r],
    };
    let rec0 = JournalRecovery {
        entries: vec![w(0, put(2, 1)), w(60, shared.clone())],
        head_pos: 61,
        dropped_torn: 0,
        foreign_pages: 0,
    };
    let rec1 = JournalRecovery {
        entries: vec![w(0, shared.clone())],
        head_pos: 1,
        dropped_torn: 0,
        foreign_pages: 0,
    };

    let merged = merge_replay_windows(vec![(p0, rec0), (p1, rec1)]);
    let violations = detect_partition_violations(&merged.entries, None);
    assert_eq!(
        violations.len(),
        1,
        "exactly one overlapping key: {violations:?}"
    );
    match &violations[0] {
        PartitionViolation::Key {
            tree_id,
            key,
            writers,
            ..
        } => {
            assert_eq!(*tree_id, TREE_INODES);
            assert_eq!(key.as_slice(), &inode_key(42)[..]);
            assert_eq!(*writers, (0, 1));
        }
        other => panic!("expected a Key violation, got {other:?}"),
    }

    // …and the policy on top of the detector is loud refusal.
    let rec0 = JournalRecovery {
        entries: vec![w(60, shared.clone())],
        head_pos: 61,
        dropped_torn: 0,
        foreign_pages: 0,
    };
    let rec1 = JournalRecovery {
        entries: vec![w(0, shared)],
        head_pos: 1,
        dropped_torn: 0,
        foreign_pages: 0,
    };
    let err = replay_merge(vec![(p0, rec0), (p1, rec1)], None).expect_err("must refuse");
    let msg = format!("{err}");
    assert!(
        msg.contains("partition") && msg.contains("writer"),
        "the refusal must name the violated invariant: {msg}"
    );
}

/// Structure authority (the §6.2 #4 corollary): interior-pointer records
/// (`level > 0`) may only come from the root-authority appender — SMOs
/// are serialized on ONE task by design (§4.6), and the two-phase replay
/// applies flips in `(level DESC, seq)` order, which is only a sound
/// total order if all flips come from one ring.
#[test]
fn test_partition_violation_interior_record_from_non_authority() {
    let (p0, p1) = (part(2, 0), part(2, 1));
    let interior = (
        tag_for(TREE_INODES, 1),
        Record::put(inode_key(9).to_vec(), 5, vec![1, 2, 3]),
    );
    let entries = merge_replay_windows(vec![
        (
            p0,
            JournalRecovery {
                entries: vec![squeezefs::meta_backend::kv::journal::ReplayedEntry {
                    seq: 0,
                    records: vec![interior.clone()],
                }],
                head_pos: 1,
                dropped_torn: 0,
                foreign_pages: 0,
            },
        ),
        (
            p1,
            JournalRecovery {
                entries: vec![squeezefs::meta_backend::kv::journal::ReplayedEntry {
                    seq: 4,
                    records: vec![interior],
                }],
                head_pos: 5,
                dropped_torn: 0,
                foreign_pages: 0,
            },
        ),
    ]);
    let violations = detect_partition_violations(&entries.entries, None);
    assert!(
        violations.iter().any(|v| matches!(
            v,
            PartitionViolation::Structure {
                writer_id: 1,
                level: 1,
                ..
            }
        )),
        "a non-authority interior record must be flagged: {violations:?}"
    );
    assert!(
        !violations
            .iter()
            .any(|v| matches!(v, PartitionViolation::Structure { writer_id: 0, .. })),
        "the authority's own interior records are legitimate: {violations:?}"
    );
}

/// Extent ownership (the §6.2 #3 face of the same law): an allocator
/// delta record may only name extents in the emitting appender's own
/// bitmap partition.
#[test]
fn test_partition_violation_alloc_record_for_foreign_extent() {
    let (p0, p1) = (part(2, 0), part(2, 1));
    // 4 extents per page, 2 appenders: page 0 → writer 0, page 1 → writer 1.
    let map = PartitionMap::new(2, 4);
    assert_eq!(map.owner_of_extent(3), 0);
    assert_eq!(map.owner_of_extent(4), 1);

    let mine = alloc_record(1, 10); // extent 1: writer 0's page.
    let theirs = alloc_record(5, 11); // extent 5: writer 1's page.
    let entries = merge_replay_windows(vec![
        (
            p0,
            JournalRecovery {
                entries: vec![squeezefs::meta_backend::kv::journal::ReplayedEntry {
                    seq: 0,
                    records: vec![mine, theirs.clone()],
                }],
                head_pos: 1,
                dropped_torn: 0,
                foreign_pages: 0,
            },
        ),
        (
            p1,
            JournalRecovery {
                entries: vec![squeezefs::meta_backend::kv::journal::ReplayedEntry {
                    seq: 0,
                    records: vec![theirs],
                }],
                head_pos: 1,
                dropped_torn: 0,
                foreign_pages: 0,
            },
        ),
    ]);
    let violations = detect_partition_violations(&entries.entries, Some(map));
    assert!(
        violations.iter().any(|v| matches!(
            v,
            PartitionViolation::Extent {
                writer_id: 0,
                extent: 5,
                owner: 1,
                ..
            }
        )),
        "writer 0 naming writer 1's extent must be flagged: {violations:?}"
    );
    // Without the map the arm is simply not evaluated (the allocator owns
    // that geometry) — but the same-key arm still catches the overlap.
    let blind = detect_partition_violations(&entries.entries, None);
    assert!(
        blind
            .iter()
            .all(|v| !matches!(v, PartitionViolation::Extent { .. })),
        "the extent arm needs the allocator's partition map: {blind:?}"
    );
}

/// Crash isolation per appender: a power cut that loses appender 1's
/// un-barriered window leaves appender 0's barriered window whole, and
/// vice versa — the sub-rings share no page, so neither can drop the
/// other's entries or resync through them.
#[tokio::test]
async fn test_crash_mid_append_recovers_per_writer() {
    const PAGES: u64 = 8;
    let f = ring_file(0, PAGES);
    let (p0, p1) = (part(2, 0), part(2, 1));
    let r0 = JournalRing::new_in_partition(f.path(), 0, PAGES, 0, p0);
    let r1 = JournalRing::new_in_partition(f.path(), 0, PAGES, 0, p1);

    uring_fs::arm_power_cut(f.path());
    let durable0 = append(&r0, &[put(2, 100)]).await;
    let durable1 = append(&r1, &[put(3, 200)]).await;
    uring_fs::fdatasync(f.path().to_path_buf())
        .await
        .expect("barrier");
    // Past the barrier: appender 1 keeps committing, then the box dies.
    let lost1 = append(&r1, &[put(5, 201)]).await;
    let reverted = uring_fs::power_cut(f.path());
    assert!(reverted >= 1, "the un-barriered append must be reverted");

    let (_, rec0) = JournalRing::recover_in_partition(f.path(), 0, PAGES, 0, 0, p0)
        .await
        .unwrap();
    let (_, rec1) = JournalRing::recover_in_partition(f.path(), 0, PAGES, 0, 0, p1)
        .await
        .unwrap();
    uring_fs::clear_faults();

    assert_eq!(
        rec0.entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![durable0],
        "appender 0's barriered window survives a peer's crash intact"
    );
    assert!(
        rec1.entries.iter().any(|e| e.seq == durable1),
        "appender 1's barriered entry survives"
    );
    assert!(
        !rec1.entries.iter().any(|e| e.seq == lost1),
        "appender 1's un-barriered entry is lost — un-acked, per §4.10 D1"
    );
    assert_eq!(
        rec0.dropped_torn, 0,
        "a peer's lost window must not read as a tear in mine"
    );
    let merged = replay_merge(vec![(p0, rec0), (p1, rec1)], None).expect("merge");
    assert_eq!(merged.entries.len(), 2);
}

// ===========================================================================
// §6.2 item 4 — the root ledger: per-appender slot ranges.
// ===========================================================================

/// Slot placement: solo is `seq % 32` verbatim; a partitioned appender
/// round-robins inside its own contiguous range, and ranges are disjoint
/// with ≥ 2 slots each (the torn-newest-slot fallback, per appender).
#[test]
fn test_ledger_slot_ranges_disjoint_and_solo_unchanged() {
    for seq in [0u64, 1, 31, 32, 4242] {
        assert_eq!(
            ledger_slot_for(seq, AppendPartition::SOLO),
            seq % ROOT_LEDGER_SLOTS,
            "solo placement must stay `slot = seq % 32`"
        );
    }
    for writers in [2u16, 4, 8, 16] {
        let per = ledger_slots_per_writer(writers);
        assert_eq!(per, ROOT_LEDGER_SLOTS / u64::from(writers));
        assert!(
            per >= 2,
            "each appender needs ≥ 2 slots so a torn newest slot can fall \
             back to its own predecessor"
        );
        for id in 0..writers {
            let p = part(writers, id);
            let lo = u64::from(id) * per;
            for seq in 0..(2 * per + 3) {
                let slot = ledger_slot_for(seq, p);
                assert!(
                    (lo..lo + per).contains(&slot),
                    "writer {id} placed seq {seq} in slot {slot}, outside [{lo}, {})",
                    lo + per
                );
            }
            // Round-robin inside the range.
            assert_eq!(ledger_slot_for(0, p), lo);
            assert_eq!(ledger_slot_for(per, p), lo);
            assert_eq!(ledger_slot_for(per - 1, p), lo + per - 1);
        }
    }
}

/// **Un-stamped byte identity (ruling D9).** A record with no append
/// partition encodes to exactly today's image — the field list, the
/// payload length, and every byte — so an existing volume's ledger is
/// bit-for-bit unchanged.
#[test]
fn test_solo_ledger_slot_bytes_are_unchanged() {
    let rec = ledger_rec(11, None, 2);
    let got = rec.encode_slot().expect("fits");

    // Rebuild today's image by hand (the §4.1 field list).
    let payload_len = 8 + 8 + 8 + 8 + 2 + 2 * (1 + 8 + 8);
    let mut want = vec![0u8; ROOT_LEDGER_SLOT_LEN as usize];
    want[0..4].copy_from_slice(&ROOT_LEDGER_MAGIC.to_le_bytes());
    want[4..8].copy_from_slice(&(payload_len as u32).to_le_bytes());
    want[8..16].copy_from_slice(&rec.seq.to_le_bytes());
    let mut pos = ROOT_LEDGER_HDR_LEN;
    want[pos..pos + 8].copy_from_slice(&rec.journal_tail_seq.to_le_bytes());
    pos += 8;
    want[pos..pos + 8].copy_from_slice(&rec.next_ino.to_le_bytes());
    pos += 8;
    want[pos..pos + 8].copy_from_slice(&rec.alloc_bitmap_generation.to_le_bytes());
    pos += 8;
    want[pos..pos + 8].copy_from_slice(&rec.node_seq_watermark.to_le_bytes());
    pos += 8;
    want[pos..pos + 2].copy_from_slice(&2u16.to_le_bytes());
    pos += 2;
    for root in &rec.tree_roots {
        want[pos] = root.tree_id;
        want[pos + 1..pos + 9].copy_from_slice(&root.node_addr.to_le_bytes());
        want[pos + 9..pos + 17].copy_from_slice(&root.node_seq.to_le_bytes());
        pos += 17;
    }
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(&want[..16]);
    h.update(&[0u8; 8]);
    h.update(&want[ROOT_LEDGER_HDR_LEN..ROOT_LEDGER_HDR_LEN + payload_len]);
    want[16..24].copy_from_slice(&h.digest().to_le_bytes());

    assert_eq!(
        got, want,
        "an un-partitioned ledger slot must be byte-identical"
    );
    let back = LedgerRecord::decode_slot(&got).expect("decode");
    assert_eq!(back, rec);
    assert!(back.append_partition.is_none());
    assert_eq!(rec.slot_index(), rec.seq % ROOT_LEDGER_SLOTS);
}

/// The partitioned record round-trips, and its suffix costs exactly the
/// four bytes it encodes (the §5.3 encoding budget stays intact).
#[test]
fn test_partitioned_ledger_record_roundtrip() {
    let solo = ledger_rec(7, None, 2).encode_slot().expect("fits");
    let rec = ledger_rec(7, Some(part(4, 2)), 2);
    let img = rec.encode_slot().expect("fits");
    let solo_len = u32::from_le_bytes(solo[4..8].try_into().unwrap());
    let part_len = u32::from_le_bytes(img[4..8].try_into().unwrap());
    assert_eq!(
        part_len - solo_len,
        4,
        "the append-partition suffix is writer_id ‖ writer_count"
    );
    let back = LedgerRecord::decode_slot(&img).expect("decode");
    assert_eq!(back, rec);
    assert_eq!(back.append_partition, Some(part(4, 2)));
}

/// **Two checkpointers do not overwrite each other** (§6.2 #4). Both
/// appenders write a record at the same checkpoint seq — which under
/// `slot = seq % 32` is the SAME slot — and both survive, each carrying
/// its own journal tail.
#[tokio::test]
async fn test_two_checkpointers_do_not_overwrite_each_other() {
    let f = NamedTempFile::new().unwrap();
    f.as_file()
        .set_len(ROOT_LEDGER_SLOTS * ROOT_LEDGER_SLOT_LEN)
        .unwrap();
    let (p0, p1) = (part(2, 0), part(2, 1));

    // Writer 0 is the root authority (roots + global fields); writer 1
    // publishes its own journal tail only.
    let mut a = ledger_rec(9, Some(p0), 2);
    a.journal_tail_seq = 1_000;
    let mut b = ledger_rec(9, Some(p1), 0);
    b.journal_tail_seq = 2_000;
    assert_ne!(a.slot_index(), b.slot_index(), "same seq, different slots");

    write_ledger_slot(f.path(), 0, &a).await.expect("write a");
    write_ledger_slot(f.path(), 0, &b).await.expect("write b");

    let led = read_partitioned_ledger(f.path(), 0, 2)
        .await
        .expect("a partitioned ledger must read");
    assert_eq!(led.writers, 2);
    assert_eq!(led.per_writer.len(), 2);
    assert_eq!(led.per_writer[0].as_ref().map(|r| r.seq), Some(9));
    assert_eq!(led.per_writer[1].as_ref().map(|r| r.seq), Some(9));
    assert_eq!(
        led.tails,
        vec![1_000, 2_000],
        "each appender's ring replays from its OWN durable tail"
    );
    assert_eq!(
        led.authority().map(|r| r.tree_roots.len()),
        Some(2),
        "the tree roots come from the root-authority appender"
    );
}

/// Loud refusals on the ledger: a record in a slot its appender does not
/// own (misdirected/foreign write), a record whose `writer_count`
/// disagrees with the mount's, and a non-authority record carrying tree
/// roots (two structural authorities is exactly the §6.2 #4 hazard).
#[tokio::test]
async fn test_partitioned_ledger_loud_refusals() {
    async fn one(rec: LedgerRecord, slot: u64, writers: u16, needle: &str) {
        let f = NamedTempFile::new().unwrap();
        f.as_file()
            .set_len(ROOT_LEDGER_SLOTS * ROOT_LEDGER_SLOT_LEN)
            .unwrap();
        let img = rec.encode_slot().expect("fits");
        uring_fs::write_at(f.path(), slot * ROOT_LEDGER_SLOT_LEN, img)
            .await
            .expect("plant the slot");
        let err = read_partitioned_ledger(f.path(), 0, writers)
            .await
            .expect_err("must refuse loud");
        let msg = format!("{err}");
        assert!(
            msg.contains(needle),
            "the refusal must name {needle:?}; got {msg}"
        );
    }

    // Writer 1's record planted in writer 0's slot range.
    one(ledger_rec(4, Some(part(2, 1)), 0), 1, 2, "slot").await;
    // A record from a differently-partitioned era.
    one(ledger_rec(4, Some(part(4, 1)), 0), 8, 2, "writer_count").await;
    // A second structural authority.
    one(ledger_rec(4, Some(part(2, 1)), 2), 16, 2, "roots").await;
}

/// The Phase-8 transition (ruling D9): the first mount after the bit is
/// stamped finds the volume's newest record in the PRE-partition
/// (suffix-less) form, written under `slot = seq % 32`. It is the
/// authority's record wherever it sits — ignoring it would fall back
/// arbitrarily far, and only the immediately-preceding record's replay
/// window is protected (§4.6 pt 3).
#[tokio::test]
async fn test_pre_partition_record_serves_as_authority() {
    let f = NamedTempFile::new().unwrap();
    f.as_file()
        .set_len(ROOT_LEDGER_SLOTS * ROOT_LEDGER_SLOT_LEN)
        .unwrap();
    // seq 20 → slot 20, which belongs to writer 1 under N = 2.
    let solo = ledger_rec(20, None, 2);
    assert_eq!(solo.slot_index(), 20);
    write_ledger_slot(f.path(), 0, &solo).await.expect("write");

    let led = read_partitioned_ledger(f.path(), 0, 2)
        .await
        .expect("a pre-partition record must not refuse the mount");
    assert_eq!(
        led.per_writer[0].as_ref().map(|r| r.seq),
        Some(20),
        "a suffix-less record is the authority's, wherever the solo law placed it"
    );
    assert!(led.per_writer[1].is_none(), "no peer record exists yet");
    assert_eq!(led.pre_partition_records, 1);
    assert_eq!(led.tails, vec![200, 0]);
    // The solo reader still sees exactly what it always saw.
    let newest = read_newest_ledger(f.path(), 0)
        .await
        .expect("read")
        .expect("some");
    assert_eq!(newest, solo);
}

// ===========================================================================
// §6.2 item 3 — the A/B extent bitmap: per-appender page partitions.
// ===========================================================================

/// The ownership function: bitmap PAGES are the partition unit (a page is
/// the A/B write unit, so one owner per page is what makes the A/B slot
/// alternation single-appender), interleaved so an existing volume's
/// allocation spreads evenly across appenders instead of handing writer 0
/// a full partition and its peers an empty one.
#[test]
fn test_partition_map_page_ownership() {
    let solo = PartitionMap::solo(ALLOC_PAGE_BITS);
    assert!(solo.is_solo());
    for e in [0u64, 1, ALLOC_PAGE_BITS, 5 * ALLOC_PAGE_BITS + 7] {
        assert_eq!(solo.owner_of_extent(e), 0);
    }
    let map = PartitionMap::new(4, ALLOC_PAGE_BITS);
    for page in 0..12u64 {
        assert_eq!(map.owner_of_page(page), page % 4);
        assert_eq!(map.owner_of_extent(page * ALLOC_PAGE_BITS + 3), page % 4);
    }
}

/// Claims are partitioned: an appender only ever claims extents in its
/// own pages, so two appenders can never claim the same extent and no
/// bitmap page is ever written by two appenders.
#[test]
fn test_partitioned_claims_stay_inside_their_pages() {
    // 4 extents per page, 2 appenders, 16 extents ⇒ pages 0,2 → writer 0.
    let core = ExtCore::new_partitioned(16, 0, 2, PartitionMap::new(2, 4));
    assert_eq!(core.free_extents(), 16);
    assert_eq!(core.free_extents_in(0), 8);
    assert_eq!(core.free_extents_in(1), 8);

    let mut mine = Vec::new();
    while let Ok(e) = core.claim_in(0, AllocClass::User) {
        mine.push(e);
    }
    assert_eq!(mine.len(), 8, "writer 0's budget is its own pages only");
    mine.sort_unstable();
    assert_eq!(mine, vec![0, 1, 2, 3, 8, 9, 10, 11]);
    assert_eq!(core.free_extents_in(0), 0);
    assert_eq!(
        core.free_extents_in(1),
        8,
        "an exhausted appender must not consume its peer's space"
    );
    let theirs = core.claim_in(1, AllocClass::User).expect("peer has space");
    assert!((4..8).contains(&theirs) || (12..16).contains(&theirs));
}

/// A heap whose last page is PARTIAL (the ordinary case — extents rarely
/// tile pages exactly): budgets and scans must both stop at `total`, and
/// an appender owning a partial page must still be able to claim inside
/// it.
#[test]
fn test_partitioned_claims_over_a_ragged_last_page() {
    // 10 extents, 4 per page ⇒ pages 0,1 full and page 2 holding {8,9}.
    // Writer 0 owns pages 0 and 2 (6 extents), writer 1 owns page 1 (4).
    let core = ExtCore::new_partitioned(10, 0, 2, PartitionMap::new(2, 4));
    assert_eq!(
        core.free_extents_in(0),
        6,
        "writer 0: page 0 + the ragged page 2"
    );
    assert_eq!(core.free_extents_in(1), 4);
    assert_eq!(
        core.free_extents(),
        10,
        "the partitions tile the heap exactly"
    );

    let mut mine = Vec::new();
    while let Ok(e) = core.claim_in(0, AllocClass::User) {
        mine.push(e);
    }
    mine.sort_unstable();
    assert_eq!(mine, vec![0, 1, 2, 3, 8, 9]);
    let mut theirs = Vec::new();
    while let Ok(e) = core.claim_in(1, AllocClass::User) {
        theirs.push(e);
    }
    theirs.sort_unstable();
    assert_eq!(theirs, vec![4, 5, 6, 7]);
    assert_eq!(core.free_extents(), 0);
}

/// Reserve isolation is per partition (§4.7 ENOSPC semantics): each
/// appender must keep its OWN compaction reserve, or a peer at ENOSPC
/// could not fold appends and free space.
#[test]
fn test_partitioned_reserve_isolation() {
    // 8 extents per page, 2 appenders, 32 extents; whole-volume reserve 4
    // ⇒ 2 per appender.
    let core = ExtCore::new_partitioned(32, 4, 2, PartitionMap::new(2, 8));
    assert_eq!(core.reserve(), 2, "the reserve divides across appenders");
    let mut n = 0;
    while core.claim_in(0, AllocClass::User).is_ok() {
        n += 1;
    }
    assert_eq!(n, 16 - 2, "writer 0's user claims stop at its own reserve");
    assert!(
        core.claim_in(0, AllocClass::Internal).is_ok(),
        "compaction may consume the appender's own reserve"
    );
    assert!(
        core.claim_in(1, AllocClass::User).is_ok(),
        "writer 1's budget is untouched by writer 0's exhaustion"
    );
}

/// The pending-free coverage gate is **per appender** (§4.7 + the §2-A
/// Option-A clock): gate seqs are positions in the freeing appender's OWN
/// ring, so one appender's durable tail must never release another's
/// parked extent. Under a single tail, writer 0 reaching seq 500 would
/// free an extent writer 1 still routes into.
#[test]
fn test_pending_free_gate_is_per_appender() {
    let core = ExtCore::new_partitioned(16, 0, 4, PartitionMap::new(2, 4));
    let mine = core.claim_in(0, AllocClass::Internal).expect("claim");
    let theirs = core.claim_in(1, AllocClass::Internal).expect("claim");
    core.free_pending(mine, 100).expect("park mine");
    core.free_pending(theirs, 100).expect("park theirs");
    assert_eq!(core.pending_count(), 2);

    // Writer 0's durable tail passes 100: only ITS entry releases.
    let released = core.advance_durable_in(0, 100);
    assert_eq!(released, vec![mine]);
    assert!(
        core.is_allocated(theirs),
        "a peer's parked extent must stay unclaimable until ITS OWN tail covers it"
    );
    assert_eq!(core.pending_count_in(1), 1);
    assert_eq!(core.durable_seq_in(1), 0);

    assert_eq!(core.advance_durable_in(1, 99), Vec::<u64>::new());
    assert_eq!(core.advance_durable_in(1, 100), vec![theirs]);
    assert!(!core.is_allocated(theirs));
}

/// Solo mode is the shipped core verbatim: one partition owning every
/// extent, the whole-volume reserve, one tail — the `ExtCore::new`
/// constructor the loom models and K4 tests use.
#[test]
fn test_solo_core_is_unchanged() {
    let core = ExtCore::new(10, 2, 4);
    assert!(core.map().is_solo());
    assert_eq!(core.reserve(), 2);
    assert_eq!(core.free_extents(), 10);
    let mut got = Vec::new();
    while let Ok(e) = core.claim(AllocClass::User) {
        got.push(e);
    }
    assert_eq!(
        got,
        (0..8).collect::<Vec<_>>(),
        "user claims stop at the reserve"
    );
    let extra = core
        .claim(AllocClass::Internal)
        .expect("reserve is internal");
    assert_eq!(extra, 8);
    core.free_pending(extra, 42).expect("park");
    assert_eq!(core.advance_durable(41), Vec::<u64>::new());
    assert_eq!(core.advance_durable(42), vec![extra]);
}

/// Two appenders' bitmap pages survive each other on disk: each writes
/// only its own pages' A/B slots, so a load sees BOTH appenders' bits —
/// the clobber §6.2 #3 describes cannot happen.
#[tokio::test]
async fn test_two_appenders_bitmap_pages_do_not_clobber() {
    // 2 pages' worth of extents so each appender owns exactly one page.
    let total = 2 * ALLOC_PAGE_BITS;
    let f = NamedTempFile::new().unwrap();
    f.as_file()
        .set_len(2 * 2 * 4096 + 4096)
        .expect("A/B region for 2 pages");

    let a0 = ExtentAllocator::format_partitioned(total, 0, 4, part(2, 0));
    let a1 = ExtentAllocator::format_partitioned(total, 0, 4, part(2, 1));
    assert_eq!(a0.partition().writer_id(), 0);
    let e0 = a0.claim_user().expect("writer 0 claim");
    let e1 = a1.claim_user().expect("writer 1 claim");
    assert_eq!(
        (e0 / ALLOC_PAGE_BITS, e1 / ALLOC_PAGE_BITS),
        (0, 1),
        "each appender claims inside its own page"
    );

    // Each appender persists ITS OWN pages, in either order.
    let w1 = a1
        .write_dirty_pages(f.path(), 0, 5)
        .await
        .expect("w1 pages");
    let w0 = a0
        .write_dirty_pages(f.path(), 0, 7)
        .await
        .expect("w0 pages");
    assert_eq!(w1, vec![1], "writer 1 wrote only its page");
    assert_eq!(w0, vec![0], "writer 0 wrote only its page");
    assert_eq!(
        (a0.foreign_page_writes(), a1.foreign_page_writes()),
        (0, 0),
        "neither appender touched a page it does not own"
    );

    let loaded =
        ExtentAllocator::load_partitioned(f.path(), 0, total, 0, 4, part(2, 0), &[0, 0], &[])
            .await
            .expect("load");
    assert!(loaded.is_allocated(e0), "writer 0's bit survived");
    assert!(
        loaded.is_allocated(e1),
        "writer 1's bit survived the peer's write"
    );
}

/// A replayed allocator delta naming an extent outside the emitting
/// appender's partition is refused LOUD at load — defense in depth behind
/// the merge's own detector (never a silently applied foreign bit).
#[tokio::test]
async fn test_load_refuses_foreign_alloc_record() {
    let total = 2 * ALLOC_PAGE_BITS;
    let f = NamedTempFile::new().unwrap();
    f.as_file().set_len(2 * 2 * 4096 + 4096).unwrap();
    // Writer 1 claims an extent on page 0 — writer 0's page.
    let foreign = MergedEntry {
        writer_id: 1,
        seq: 10,
        records: vec![alloc_record(3, 10)],
    };
    let err = ExtentAllocator::load_partitioned(
        f.path(),
        0,
        total,
        0,
        4,
        part(2, 0),
        &[0, 0],
        std::slice::from_ref(&foreign),
    )
    .await
    .expect_err("a foreign allocator delta must refuse loud");
    let msg = format!("{err}");
    assert!(
        msg.contains("extent 3") && msg.contains("writer 1"),
        "the refusal must name the extent and the appender: {msg}"
    );

    // The same record from its rightful owner loads fine, and parks its
    // free under the OWNER's gate.
    let own = MergedEntry {
        writer_id: 0,
        seq: 10,
        records: vec![alloc_record(3, 10)],
    };
    let freed = MergedEntry {
        writer_id: 1,
        seq: 20,
        records: vec![free_record(ALLOC_PAGE_BITS + 1, 4, 20)],
    };
    let alloc = ExtentAllocator::load_partitioned(
        f.path(),
        0,
        total,
        0,
        4,
        part(2, 0),
        &[0, 0],
        &[own, freed],
    )
    .await
    .expect("owner records load");
    assert!(alloc.is_allocated(3));
    assert!(
        alloc.is_allocated(ALLOC_PAGE_BITS + 1),
        "a replayed in-window free stays parked (the §2-A mount gate)"
    );
    assert_eq!(alloc.pending_count(), 1);
    // Writer 0's tail cannot release writer 1's parked extent.
    assert_eq!(alloc.advance_durable_in(0, 1_000), 0);
    assert_eq!(alloc.advance_durable_in(1, 20), 1);
    assert!(!alloc.is_allocated(ALLOC_PAGE_BITS + 1));
}

// ===========================================================================
// Ruling D9 — the compatibility matrix: BUILT, NOT STAMPED.
// ===========================================================================

/// The bit exists and is understood (so a Phase-8-stamped volume mounts),
/// but **nothing in this binary sets it**: not `format`, not `mount`.
/// The compatibility matrix:
///
/// | volume | this binary | an older binary |
/// |---|---|---|
/// | un-stamped (every existing volume, and every volume this binary formats) | mounts, solo structures byte-identical | mounts (bit absent) |
/// | Phase-8 stamped | mounts, partitioned forms available | **refuses loud** — the bit is outside its `FEATURES_INCOMPAT_KNOWN` |
#[test]
fn test_partitioned_append_bit_is_built_but_not_stamped() {
    use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
    use squeezefs::meta_backend::kv::superblock::{
        SuperblockV3, FEATURES_INCOMPAT_KNOWN, FEATURE_INCOMPAT_KV_PARTITIONED_APPEND,
    };

    assert_eq!(
        FEATURE_INCOMPAT_KV_PARTITIONED_APPEND,
        1 << 8,
        "bit 8 — the next free incompat bit after S2's durable term (bit 7)"
    );
    assert_ne!(
        FEATURES_INCOMPAT_KNOWN & FEATURE_INCOMPAT_KV_PARTITIONED_APPEND,
        0,
        "this binary must UNDERSTAND the bit, so a stamped volume mounts"
    );
    // The bit intersects no prior mask ⇒ every older binary refuses a
    // stamped volume at its own feature gate.
    assert_eq!(
        (FEATURES_INCOMPAT_KNOWN & !FEATURE_INCOMPAT_KV_PARTITIONED_APPEND)
            & FEATURE_INCOMPAT_KV_PARTITIONED_APPEND,
        0
    );

    let sb = SuperblockV3::plan(
        64 * 1024 * 1024,
        DEFAULT_NODE_SIZE,
        None,
        [0x11; 16],
        0x5EED,
    )
    .expect("plan");
    assert_eq!(
        sb.features_incompat & FEATURE_INCOMPAT_KV_PARTITIONED_APPEND,
        0,
        "ruling D9: BUILT but NOT STAMPED — format must not set the bit \
         (it stamps in the Phase-8 batched reformat window)"
    );
}

/// The un-stamped path end to end: a volume this binary formats and mounts
/// comes back with a **byte-identical superblock sector** — no bit 8, no
/// partitioned structures, nothing new written. This is the ruling-D9
/// guarantee ("an existing volume must mount and behave EXACTLY as today")
/// pinned at the only place it can be observed.
#[tokio::test]
async fn test_unstamped_volume_mount_leaves_the_superblock_unchanged() {
    use squeezefs::meta_backend::kv::backend::KvMetaBackend;
    use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
    use squeezefs::meta_backend::kv::superblock::{
        classify_sector0, classify_volume, VolumeFormat, FEATURE_INCOMPAT_KV_PARTITIONED_APPEND,
    };
    use squeezefs::meta_backend::Metadata;

    const VOL_LEN: u64 = 64 * 1024 * 1024;
    let f = NamedTempFile::new().unwrap();
    f.as_file().set_len(VOL_LEN).unwrap();
    format_v3(
        f.path(),
        VOL_LEN,
        &FormatV3Options {
            node_size: 64 * 1024,
            journal_len_override: Some(1024 * 1024),
            force: false,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format");

    let before = uring_fs::read_at(f.path(), 0, 4096)
        .await
        .expect("sector 0");

    let be = KvMetaBackend::open(f.path()).await.expect("mount");
    let created = be
        .create(1, "partition_probe", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create");
    assert!(created.ino >= 2, "the mount must be functional");
    be.shutdown().await.expect("clean unmount");
    drop(be);

    let after = uring_fs::read_at(f.path(), 0, 4096)
        .await
        .expect("sector 0");
    // Feature word at the superblock's `OFF_FEAT_INCOMPAT` (16).
    let feat_before = u64::from_le_bytes(before[16..24].try_into().unwrap());
    let feat_after = u64::from_le_bytes(after[16..24].try_into().unwrap());
    assert_eq!(
        feat_after & FEATURE_INCOMPAT_KV_PARTITIONED_APPEND,
        0,
        "mount must never stamp the partitioned-append bit"
    );
    assert_eq!(
        feat_before, feat_after,
        "an un-stamped volume's feature word must survive a mount cycle unchanged"
    );
    // …and the whole self-describing geometry is untouched (the DUR-5
    // sector generation is the only field a mount may move).
    let (VolumeFormat::V3(sb_before), VolumeFormat::V3(sb_after)) = (
        classify_sector0(&before).expect("classify before"),
        classify_volume(f.path()).await.expect("classify after"),
    ) else {
        panic!("expected a v3 volume on both sides");
    };
    assert_eq!(
        sb_before, sb_after,
        "an un-stamped volume's superblock must be unchanged after a mount cycle"
    );
}
