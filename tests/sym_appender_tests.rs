//! Symmetric metadata program, PR 2 — **the appender region**
//! (`docs/design-symmetric-metadata.md` §5.3 B(ii), §5.2.3, §5.7.3, §5.9,
//! §6.1/§6.2/§6.4, §7.1, §11; KD-SYM-3/4/10).
//!
//! Under incompat bit 17 every metadata writer is an APPENDER with its
//! own 4 KiB page (identity, term, state, ring segments, tail, checkpoint
//! seq, extent grant, leased slots), its own journal ring (a segment
//! table), and — from PR 3 — its own extent grant. Appender 0's page and
//! ring live with the format-time fixed ring; appenders ≥ 1 live in the
//! appender DIRECTORY, an extent chain the superblock names.
//!
//! **Bit 17 stays DARK**: the only stamp is the PR-1 seam
//! `SQUEEZEFS_TEST_STAMP_SYMMETRIC=1`, and a bit-17-absent volume is
//! byte-for-byte the shipped format — the negative contract every
//! positive one here rides beside.
//!
//! Contracts pinned (the PR-2 row of the design's PR plan):
//! - the page codec round-trips at every bound and its decode is TOTAL;
//! - `SLOT_PAGE_BUDGET` derives from the page's own fixed part;
//! - newest-valid-wins over the four page slots; a torn newest page falls
//!   back to its predecessor;
//! - the directory header codec; pairs per extent derive from the node
//!   size; the chain grows past one extent;
//! - the ring derivation: floor 512 KiB (reserve + one max entry, rounded
//!   to a power of two), ceiling = the solo ring, the knob wins verbatim;
//!   `appenders_capacity = heap/16 ÷ ring`;
//! - routing slot ↔ forest slot around the native slot;
//! - `appender_dir` rides sector 0 ONLY under bit 17 (byte-identical
//!   sector 0 otherwise); format under the seam writes appender 0's page
//!   pair + the directory's first extent.

mod common;

use squeezefs::meta_backend::kv::appender::{
    appender0_page_offsets, appender0_ring_extent, appender_ring_bytes_derived, appenders_capacity,
    classify_page, dir_pair_offsets, dir_pairs_per_extent, forest_slot_of_page_slot, newest_valid,
    page_slot_for, page_slot_of_forest_slot, read_directory, ring_budget_bytes,
    sym_ring_ceiling_bytes, AppenderIdentity, AppenderPage, AppenderState, DirHeader, GrantRun,
    PageRead, SlotEntry, SlotEntryState, APPENDER0_RESERVED_PAGES, APPENDER_PAGE_FIXED_LEN,
    APPENDER_PAGE_LEN, APPENDER_PAGE_SLOTS, GRANT_RUNS_MAX, RING_SEGMENTS_MAX, SLOT_ENTRY_LEN,
    SLOT_PAGE_BUDGET, SYM_RING_FLOOR_BYTES,
};
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder, ROOT_INO};
use squeezefs::meta_backend::kv::checkpoint::CHECKPOINT_MAX_AGE_MS;
use squeezefs::meta_backend::kv::journal::{
    checkpoint_reserve_bytes, detect_appender_violations, entry_len_for, tag_for,
    AppenderViolation, JournalRecovery, JournalRing, ReplayedEntry, RingSegment, JOURNAL_PAGE_LEN,
    MAX_ENTRY_LEN,
};
use squeezefs::meta_backend::kv::journal_core::AdmissionClass;
use squeezefs::meta_backend::kv::record::{
    forest_key, guest_forest_slot, inode_key, Record, KIND_INTERIOR, NATIVE_FOREST_SLOT,
    TREE_CONTROL, TREE_INODES,
};
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, journal_ring_len, ExtentRef, SuperblockV3, VolumeFormat,
    FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST, SUPERBLOCK_V3_LEN,
};
use squeezefs::meta_backend::kv::tree::RootPtr;
use squeezefs::meta_backend::kv::META_KV_CHECKPOINTS;
use std::collections::HashMap;
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// Harness (the sym_forest_tests shape: 64 KiB nodes, a 1 MiB fixed ring).
// ---------------------------------------------------------------------------

const VOL_LEN: u64 = 64 * 1024 * 1024;
const NODE_SIZE: usize = 64 * 1024;
const RING_LEN: u64 = 1024 * 1024;
const TEST_SEED: u64 = 0x5EED_A99E_0DE7_0002;
const TEST_UUID: [u8; 16] = *b"sym-appender-tst";

/// The seam is process-global; format calls serialize through it.
static SEAM: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn builder_config() -> BuilderConfig {
    BuilderConfig {
        node_size: NODE_SIZE,
        journal_len_override: Some(RING_LEN),
        hash_seed: TEST_SEED,
        uuid: TEST_UUID,
    }
}

fn describe() -> (ImageBuilder, HashMap<&'static str, u64>) {
    let mut b = ImageBuilder::new(builder_config()).unwrap();
    let mut inos = HashMap::new();
    let docs = b.add_dir(ROOT_INO, "docs", 0o750, 1000, 1000).unwrap();
    inos.insert("docs", docs);
    let readme = b
        .add_file(docs, "readme.txt", 0o644, 1000, 1000, 4096)
        .unwrap();
    inos.insert("readme.txt", readme);
    b.set_xattr(readme, "user.color", b"blue").unwrap();
    (b, inos)
}

async fn build_image(file: &NamedTempFile, symmetric: bool) -> HashMap<&'static str, u64> {
    file.as_file().set_len(VOL_LEN).unwrap();
    let (b, inos) = describe();
    let _g = SEAM.lock().await;
    if symmetric {
        std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
    } else {
        std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    }
    let built = b.build(file.path(), VOL_LEN).await;
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    built.expect("build image");
    inos
}

async fn superblock_of(file: &NamedTempFile) -> SuperblockV3 {
    match classify_volume(file.path()).await.expect("classify") {
        VolumeFormat::V3(sb) => sb,
        other => panic!("expected a v3 superblock, got {other:?}"),
    }
}

fn full_page() -> AppenderPage {
    AppenderPage {
        appender_id: 7,
        generation: 42,
        identity: AppenderIdentity {
            node_token: 0x0123_4567_89AB_CDEF,
            mount_slot: 0xDEAD_BEEF,
            writer_id: 0x0011_2233_4455_6677_8899_AABB_CCDD_EEFF,
        },
        term: 3,
        state: AppenderState::Live,
        recovered_by_term: 0,
        is_manager: true,
        home_volume: 2,
        segments: (0..RING_SEGMENTS_MAX as u64)
            .map(|i| ExtentRef {
                start: 0x10_0000 + i * 0x4_0000,
                len: 0x4_0000,
            })
            .collect(),
        head_hint: 123_456,
        ledger_tail_seq: 100_000,
        ckpt_seq: 17,
        seq_offset: 0x0102_0304,
        grant: (0..GRANT_RUNS_MAX as u64)
            .map(|i| GrantRun {
                start: 1000 + i * 8,
                len: 8,
            })
            .collect(),
        slots: (0..SLOT_PAGE_BUDGET as u16)
            .map(|i| SlotEntry {
                slot: i * 3,
                state: if i % 2 == 0 {
                    SlotEntryState::Live
                } else {
                    SlotEntryState::Releasing
                },
                g: u32::from(i) + 1,
                slot_tree_extents: u32::from(i) * 2,
                root: RootPtr {
                    addr: 0x20_0000 + u64::from(i) * 0x1_0000,
                    seq: 5000 + u64::from(i),
                },
                cursor: 77 + u64::from(i),
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// §5.3.2 — the page codec.
// ---------------------------------------------------------------------------

#[test]
fn slot_page_budget_derives_from_the_pages_fixed_part() {
    // The budget is what the page's own arithmetic leaves, never a free
    // constant: fixed part + budget × entry ≤ one page, and one more
    // entry would not fit.
    assert_eq!(APPENDER_PAGE_LEN, 4096);
    assert_eq!(
        SLOT_PAGE_BUDGET,
        (APPENDER_PAGE_LEN - APPENDER_PAGE_FIXED_LEN) / SLOT_ENTRY_LEN
    );
    let budget = std::hint::black_box(SLOT_PAGE_BUDGET);
    assert!(APPENDER_PAGE_FIXED_LEN + budget * SLOT_ENTRY_LEN <= APPENDER_PAGE_LEN);
    assert!(APPENDER_PAGE_FIXED_LEN + (budget + 1) * SLOT_ENTRY_LEN > APPENDER_PAGE_LEN);
    // The two VALUES are on-disk format (the `TREE_ALLOC_RESERVED == 4`
    // precedent): a wider fixed part or a wider entry recomputes both
    // and would silently re-lay every page under the same bit — a change
    // here ships under a NEW incompat bit (design §7.1), so the numbers
    // are pinned, not only the formula.
    assert_eq!(
        APPENDER_PAGE_FIXED_LEN, 314,
        "the §5.3.2 fixed part at natural widths (offset of n_slots + 2 + the ring's u64 \
         seq_offset — PR 4's seq-space law)"
    );
    assert_eq!(
        SLOT_PAGE_BUDGET, 108,
        "(4096 − 314) / 35 — the design's 122 predates `slot_tree_extents`"
    );
    // The §5.3.2 field list at natural widths: slot u16 + state u8 + g u32
    // + slot_tree_extents u32 + root (u64, u64) + cursor u64.
    assert_eq!(SLOT_ENTRY_LEN, 2 + 1 + 4 + 4 + 8 + 8 + 8);
    assert_eq!(
        RING_SEGMENTS_MAX, 8,
        "2 MiB per growth step at 256 KiB segments"
    );
    assert_eq!(
        GRANT_RUNS_MAX, 4,
        "current + successor + two returns in flight"
    );
    assert_eq!(
        APPENDER_PAGE_SLOTS, 4,
        "the A/B pair + the two ring-side pages"
    );
    assert_eq!(APPENDER0_RESERVED_PAGES, 4);
}

#[test]
fn appender_page_round_trips_at_every_bound_and_refuses_past_them() {
    let page = full_page();
    let img = page.encode().expect("encode");
    assert_eq!(img.len(), APPENDER_PAGE_LEN);
    assert_eq!(AppenderPage::decode(&img).expect("decode"), page);
    assert_eq!(page.ring_bytes(), RING_SEGMENTS_MAX as u64 * 0x4_0000);

    let mut too_many_segments = page.clone();
    too_many_segments
        .segments
        .push(ExtentRef { start: 1, len: 1 });
    assert!(too_many_segments.encode().is_err(), "9 segments refuse");
    let mut too_many_runs = page.clone();
    too_many_runs.grant.push(GrantRun { start: 1, len: 1 });
    assert!(too_many_runs.encode().is_err(), "5 grant runs refuse");
    let mut too_many_slots = page.clone();
    too_many_slots.slots.push(SlotEntry {
        slot: u16::MAX,
        state: SlotEntryState::Live,
        g: 0,
        slot_tree_extents: 0,
        root: RootPtr { addr: 0, seq: 0 },
        cursor: 0,
    });
    assert!(
        too_many_slots.encode().is_err(),
        "SLOT_PAGE_BUDGET + 1 slots refuse — the budget is a hard bound, never a truncation"
    );

    // A Free page names nothing.
    let free = AppenderPage::free(3, 1);
    let free_img = free.encode().unwrap();
    let back = AppenderPage::decode(&free_img).unwrap();
    assert_eq!(back.state, AppenderState::Free);
    assert!(back.segments.is_empty() && back.slots.is_empty() && back.grant.is_empty());
}

#[test]
fn appender_page_decode_is_total_and_canonical() {
    let page = full_page();
    let img = page.encode().unwrap();
    // Short / long images refuse by length.
    assert!(AppenderPage::decode(&img[..img.len() - 1]).is_err());
    assert!(AppenderPage::decode(&[0u8; 4097]).is_err());
    // Blank classifies as Blank, not Corrupt.
    assert_eq!(classify_page(&[0u8; APPENDER_PAGE_LEN]), PageRead::Blank);
    // Bad magic.
    let mut bad_magic = img.clone();
    bad_magic[0] ^= 0xFF;
    assert!(matches!(classify_page(&bad_magic), PageRead::Corrupt(_)));
    // Any flipped byte fails the checksum (the whole image is covered).
    for off in [5usize, 30, 60, 100, 300, 2000, 4095] {
        let mut torn = img.clone();
        torn[off] ^= 0x01;
        assert!(
            matches!(classify_page(&torn), PageRead::Corrupt(_)),
            "flip at {off} must not verify"
        );
    }
    // A structurally impossible image with a VALID checksum refuses too:
    // an unknown state, a non-ascending slot list, a count past its bound.
    let mut state9 = page.clone();
    state9.slots.clear();
    let mut s_img = state9.encode().unwrap();
    s_img[52] = 9;
    let sum = recompute_checksum(&s_img);
    s_img[16..24].copy_from_slice(&sum.to_le_bytes());
    assert!(
        AppenderPage::decode(&s_img).is_err(),
        "unknown state refuses"
    );
    let mut unsorted = page.clone();
    unsorted.slots.truncate(2);
    unsorted.slots.swap(0, 1);
    let u_img = unsorted.encode().unwrap();
    assert!(
        AppenderPage::decode(&u_img).is_err(),
        "slot entries must be slot-ascending"
    );
    let mut count = page.clone();
    count.slots.clear();
    let mut c_img = count.encode().unwrap();
    c_img[304..306].copy_from_slice(&((SLOT_PAGE_BUDGET as u16) + 1).to_le_bytes());
    // (offset 304 is `n_slots`; the u64 `seq_offset` follows it at 306.)
    let sum = recompute_checksum(&c_img);
    c_img[16..24].copy_from_slice(&sum.to_le_bytes());
    assert!(
        AppenderPage::decode(&c_img).is_err(),
        "a slot count past the budget is a claim, never an allocation authority"
    );
}

/// The page's own checksum law (xxh3_64 with the field zeroed) — used to
/// build checksum-valid but structurally wrong images.
fn recompute_checksum(img: &[u8]) -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(&img[..16]);
    h.update(&[0u8; 8]);
    h.update(&img[24..]);
    h.digest()
}

#[test]
fn newest_valid_page_wins_and_a_torn_newest_falls_back_to_its_predecessor() {
    let mut images: Vec<Vec<u8>> = Vec::new();
    for g in 1..=4u64 {
        let mut p = AppenderPage::free(1, g);
        p.term = g;
        images.push(p.encode().unwrap());
    }
    let (slot, newest) = newest_valid(&images).unwrap().expect("a valid page");
    assert_eq!((slot, newest.generation), (3, 4));
    // Tear the newest: its predecessor wins.
    images[3][100] ^= 0xFF;
    let (slot, newest) = newest_valid(&images).unwrap().expect("a predecessor");
    assert_eq!((slot, newest.generation), (2, 3));
    // Tear that too: two of slack (§5.3.2).
    images[2][100] ^= 0xFF;
    let (slot, newest) = newest_valid(&images)
        .unwrap()
        .expect("a second predecessor");
    assert_eq!((slot, newest.generation), (1, 2));
    // Everything torn / blank ⇒ nothing.
    images[1][100] ^= 0xFF;
    images[0] = vec![0u8; APPENDER_PAGE_LEN];
    assert!(newest_valid(&images).unwrap().is_none());
    // Generation order, not slot order, decides.
    let mut p9 = AppenderPage::free(1, 9);
    p9.term = 9;
    let imgs = [
        p9.encode().unwrap(),
        AppenderPage::free(1, 2).encode().unwrap(),
    ];
    assert_eq!(newest_valid(&imgs).unwrap().unwrap().0, 0);
}

#[test]
fn page_slot_rotation_covers_the_four_slots() {
    let slots: Vec<usize> = (1..=8u64).map(page_slot_for).collect();
    assert_eq!(slots, vec![1, 2, 3, 0, 1, 2, 3, 0]);
}

// ---------------------------------------------------------------------------
// §5.3.1 — the directory chain.
// ---------------------------------------------------------------------------

#[test]
fn directory_header_round_trips_and_pairs_per_extent_derive_from_the_node_size() {
    // Every page but the header page, in pairs.
    assert_eq!(dir_pairs_per_extent(64 * 1024), 7);
    assert_eq!(dir_pairs_per_extent(256 * 1024), 31);
    assert_eq!(dir_pairs_per_extent(1024 * 1024), 127);
    let hdr = DirHeader {
        chain_index: 3,
        next: ExtentRef {
            start: 0x40_0000,
            len: 0x4_0000,
        },
        pairs: 31,
    };
    let img = hdr.encode();
    assert_eq!(img.len(), APPENDER_PAGE_LEN);
    assert_eq!(DirHeader::decode(&img).unwrap(), hdr);
    let mut torn = img.clone();
    torn[20] ^= 1;
    assert!(DirHeader::decode(&torn).is_err());
    assert!(DirHeader::decode(&img[..100]).is_err());
    assert!(DirHeader::decode(&[0u8; APPENDER_PAGE_LEN]).is_err());
}

// ---------------------------------------------------------------------------
// §1.6 / §6.1 — the ring derivation and the capacity.
// ---------------------------------------------------------------------------

#[test]
fn ring_size_derivation_floor_ceiling_and_capacity_tie() {
    // Floor: the checkpoint carve-out + one max entry, rounded to a power
    // of two — 512 KiB, derived, not declared.
    assert_eq!(
        SYM_RING_FLOOR_BYTES,
        (checkpoint_reserve_bytes(512 * 1024) + MAX_ENTRY_LEN).next_power_of_two()
    );
    assert_eq!(SYM_RING_FLOOR_BYTES, 512 * 1024);
    // Ceiling: the solo ring's derivation.
    assert_eq!(sym_ring_ceiling_bytes(VOL_LEN), journal_ring_len(VOL_LEN));
    let tib = 1u64 << 40;
    assert_eq!(sym_ring_ceiling_bytes(tib), journal_ring_len(tib));
    // A fresh join (no EWMA) takes the floor; a runaway rate the ceiling.
    assert_eq!(
        appender_ring_bytes_derived(0, VOL_LEN),
        SYM_RING_FLOOR_BYTES
    );
    assert_eq!(
        appender_ring_bytes_derived(u64::MAX / 4, VOL_LEN),
        journal_ring_len(VOL_LEN)
    );
    // The middle: 2 × rate × CHECKPOINT_MAX_AGE — 1 MiB/s ⇒ 2 MiB.
    let rate = 1024 * 1024;
    assert_eq!(
        appender_ring_bytes_derived(rate, tib),
        2 * rate * CHECKPOINT_MAX_AGE_MS as u64 / 1000
    );
    // Capacity: the ring budget (heap/16) over the ring size.
    let heap = 4 * tib;
    assert_eq!(ring_budget_bytes(heap), heap / 16);
    assert_eq!(
        appenders_capacity(heap, SYM_RING_FLOOR_BYTES),
        heap / 16 / SYM_RING_FLOOR_BYTES
    );
    assert_eq!(appenders_capacity(heap, 0), 0);
}

#[test]
fn routing_slot_and_forest_slot_convert_both_ways_around_the_native_slot() {
    let native = 5u16;
    assert_eq!(forest_slot_of_page_slot(native, native), NATIVE_FOREST_SLOT);
    assert_eq!(forest_slot_of_page_slot(7, native), guest_forest_slot(7));
    assert_eq!(
        page_slot_of_forest_slot(NATIVE_FOREST_SLOT, native).unwrap(),
        native
    );
    assert_eq!(
        page_slot_of_forest_slot(guest_forest_slot(7), native).unwrap(),
        7
    );
    assert!(
        page_slot_of_forest_slot(guest_forest_slot(native), native).is_err(),
        "the native slot's guest keyspace is never minted"
    );
    assert!(page_slot_of_forest_slot(guest_forest_slot(u16::MAX) + 1, native).is_err());
    for s in [0u16, 1, 100, u16::MAX] {
        if s == native {
            continue;
        }
        assert_eq!(
            page_slot_of_forest_slot(forest_slot_of_page_slot(s, native), native).unwrap(),
            s
        );
    }
}

// ---------------------------------------------------------------------------
// §6.4 / §7.1 — the superblock and the format-time image.
// ---------------------------------------------------------------------------

#[test]
fn appender_dir_rides_sector_zero_only_under_bit_17() {
    let mut sb =
        SuperblockV3::plan(VOL_LEN, NODE_SIZE, Some(RING_LEN), TEST_UUID, TEST_SEED).expect("plan");
    assert_eq!(
        sb.appender_dir,
        ExtentRef { start: 0, len: 0 },
        "plan never names a directory"
    );
    let flat = sb.encode_sector().unwrap();
    assert!(
        flat[136..152].iter().all(|b| *b == 0),
        "sector 0 bytes [136, 152) are zero on a bit-17-absent volume"
    );
    // Under bit 17 the field round-trips.
    sb.features_incompat |= FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST;
    sb.appender_dir = ExtentRef {
        start: sb.heap.start,
        len: NODE_SIZE as u64,
    };
    let img = sb.encode_sector().unwrap();
    assert_eq!(img.len(), SUPERBLOCK_V3_LEN);
    let back = SuperblockV3::decode_sector(&img).unwrap();
    assert_eq!(back.appender_dir, sb.appender_dir);
    // Without the bit a named directory is not the shipped image: the
    // DECODER refuses it (the mount gate — the byte-identity law's tooth;
    // the encoder writes what it is handed so the harnesses that strip
    // bits off a stamped superblock reach the gate they aim at).
    sb.features_incompat &= !FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST;
    let stripped = sb.encode_sector().unwrap();
    assert!(SuperblockV3::decode_sector(&stripped).is_err());
    let mut forged = flat.clone();
    forged[136..144].copy_from_slice(&sb.heap.start.to_le_bytes());
    forged[144..152].copy_from_slice(&(NODE_SIZE as u64).to_le_bytes());
    // Re-checksum so only the gate can refuse it.
    let sum = xxhash_rust::xxh3::xxh3_64(&{
        let mut z = forged.clone();
        z[120..128].fill(0);
        z
    });
    forged[120..128].copy_from_slice(&sum.to_le_bytes());
    assert!(SuperblockV3::decode_sector(&forged).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn format_without_the_seam_leaves_the_directory_and_the_fixed_ring_untouched() {
    let file = NamedTempFile::new().unwrap();
    build_image(&file, false).await;
    let sb = superblock_of(&file).await;
    assert_eq!(sb.appender_dir, ExtentRef { start: 0, len: 0 });
    // The journal extent's first four pages are ring pages on a flat
    // volume — zero after format (nothing to replay).
    let head = squeezefs::uring_fs::read_at(
        file.path(),
        sb.journal.start,
        (APPENDER0_RESERVED_PAGES * APPENDER_PAGE_LEN as u64) as usize,
    )
    .await
    .unwrap();
    assert!(head.iter().all(|b| *b == 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn format_under_the_seam_writes_appender_zeros_page_pair_and_the_first_directory_extent() {
    let file = NamedTempFile::new().unwrap();
    build_image(&file, true).await;
    let sb = superblock_of(&file).await;
    assert!(sb.symmetric_forest_stamped());
    assert_eq!(sb.appender_dir.len, NODE_SIZE as u64);
    assert!(sb.appender_dir.start >= sb.heap.start && sb.appender_dir.end() <= sb.heap.end());
    let entries = read_directory(file.path(), &sb).await.expect("directory");
    // Appender 0 + every pair of the first extent (7 at 64 KiB nodes).
    assert_eq!(
        entries.len() as u64,
        1 + dir_pairs_per_extent(NODE_SIZE as u64)
    );
    let zero = entries[0].page.as_ref().expect("appender 0's page");
    assert_eq!(zero.appender_id, 0);
    assert_eq!(
        zero.state,
        AppenderState::Free,
        "nobody has joined at format"
    );
    assert_eq!(zero.segments, vec![appender0_ring_extent(&sb.journal)]);
    assert_eq!(
        entries[0].dir_offsets,
        [sb.journal.start, sb.journal.start + 4096]
    );
    assert_eq!(
        appender0_page_offsets(&sb.journal)[3],
        sb.journal.start + 3 * 4096
    );
    for e in &entries[1..] {
        assert!(e.page.is_none(), "an unallocated id has no page");
    }
}

// ---------------------------------------------------------------------------
// §5.3.1 / §6.4 — segmented rings (kv/journal.rs).
// ---------------------------------------------------------------------------

/// One inode Put padding its entry to exactly `entry_len` bytes.
fn sized_records(i: u64, entry_len: u64, marker: u8, seq: u64) -> Vec<(u8, Record)> {
    let overhead = squeezefs::meta_backend::kv::journal::ENTRY_HDR_LEN
        + 1
        + squeezefs::meta_backend::kv::record::RECORD_HEADER_LEN as u64
        + squeezefs::meta_backend::kv::record::INODE_KEY_LEN as u64;
    let value = vec![marker; (entry_len - overhead) as usize];
    vec![(TREE_INODES, Record::put(inode_key(i).to_vec(), seq, value))]
}

async fn append_sized(ring: &JournalRing, i: u64, entry_len: u64, marker: u8) -> u64 {
    let probe = sized_records(i, entry_len, marker, 0);
    let need = entry_len_for(&probe).expect("under cap");
    let adm = ring
        .core()
        .try_admit(need, AdmissionClass::User)
        .expect("room");
    let (res, seq_base) = ring.reserve_registered(adm);
    let records = sized_records(i, entry_len, marker, seq_base);
    ring.commit_entry(&res, &records).await.expect("write");
    res.end()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_segmented_ring_maps_pages_through_its_segment_table_and_replays_across_segments() {
    // Two NON-contiguous segments of 4 pages each, with a 16-page gap.
    let f = NamedTempFile::new().unwrap();
    f.as_file().set_len(64 * JOURNAL_PAGE_LEN).unwrap();
    let segs = vec![
        RingSegment {
            base: 4 * JOURNAL_PAGE_LEN,
            pages: 4,
        },
        RingSegment {
            base: 24 * JOURNAL_PAGE_LEN,
            pages: 4,
        },
    ];
    let ring = JournalRing::new_segments(f.path(), segs.clone(), 0);
    assert_eq!(
        ring.segments(),
        vec![
            ExtentRef {
                start: 4 * JOURNAL_PAGE_LEN,
                len: 4 * JOURNAL_PAGE_LEN
            },
            ExtentRef {
                start: 24 * JOURNAL_PAGE_LEN,
                len: 4 * JOURNAL_PAGE_LEN
            }
        ]
    );
    assert_eq!(ring.ring_bytes(), 8 * JOURNAL_PAGE_LEN);
    // Logical page 5 is the second segment's page 1.
    assert_eq!(
        ring.page_offset(5),
        24 * JOURNAL_PAGE_LEN + JOURNAL_PAGE_LEN
    );
    // Six 3000-byte entries: the chain crosses the segment boundary
    // (page 4 starts at logical 4 × 4072 = 16,288; entry 6 ends past it).
    let mut end = 0;
    for i in 1..=6u64 {
        end = append_sized(&ring, i, 3000, i as u8).await;
    }
    assert!(end > 4 * 4072, "the chain crossed into the second segment");
    let (rec_ring, recovery) = JournalRing::recover_segments(f.path(), segs, 0, 0)
        .await
        .expect("replay");
    assert_eq!(recovery.entries.len(), 6);
    assert_eq!(recovery.dropped_torn, 0);
    assert_eq!(rec_ring.core().head(), end);
    for (i, e) in recovery.entries.iter().enumerate() {
        assert_eq!(
            e.records,
            sized_records(i as u64 + 1, 3000, i as u8 + 1, e.seq)
        );
    }
    // The gap between the segments was never written.
    let gap = squeezefs::uring_fs::read_at(f.path(), 8 * JOURNAL_PAGE_LEN, 16 * 4096)
        .await
        .unwrap();
    assert!(gap.iter().all(|b| *b == 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_solo_ring_is_one_segment_and_growth_needs_a_drained_ring() {
    let f = NamedTempFile::new().unwrap();
    f.as_file().set_len(32 * JOURNAL_PAGE_LEN).unwrap();
    let ring = JournalRing::new(f.path(), 4096, 4, 0);
    assert_eq!(
        ring.segments(),
        vec![ExtentRef {
            start: 4096,
            len: 4 * JOURNAL_PAGE_LEN
        }],
        "the shipped ring is one segment over its extent"
    );
    let extra = RingSegment {
        base: 16 * JOURNAL_PAGE_LEN,
        pages: 4,
    };
    // Drained (head == reusable_upto, nothing in flight): grows, head kept.
    let end = append_sized(&ring, 1, 500, 0xA1).await;
    ring.advance_reusable_upto(end);
    let grown = ring.grown_with(extra).expect("a drained ring grows");
    assert_eq!(grown.segments().len(), 2);
    assert_eq!(grown.core().head(), end, "the logical head is kept");
    assert_eq!(grown.core().reusable_upto(), end);
    assert_eq!(grown.core().geometry().pages, 8);
    assert_eq!(grown.written_entries(), 1, "the counters carry over");
    // Not drained: in-window records exist ⇒ refused.
    let ring2 = JournalRing::new(f.path(), 4096, 4, 0);
    append_sized(&ring2, 1, 500, 0xA2).await;
    assert!(
        ring2.grown_with(extra).is_err(),
        "an undrained ring refuses to grow"
    );
    // An open reservation ⇒ refused.
    let ring3 = JournalRing::new(f.path(), 4096, 4, 0);
    let adm = ring3.core().try_admit(500, AdmissionClass::User).unwrap();
    let (res, _) = ring3.reserve_registered(adm);
    ring3.advance_reusable_upto(res.end());
    assert!(
        ring3.grown_with(extra).is_err(),
        "an open reservation refuses growth"
    );
    ring3.complete(&res);
    // Past the segment bound ⇒ refused.
    let mut many = JournalRing::new(f.path(), 4096, 1, 0);
    for k in 0..RING_SEGMENTS_MAX as u64 - 1 {
        many = many
            .grown_with(RingSegment {
                base: (8 + k) * JOURNAL_PAGE_LEN,
                pages: 1,
            })
            .expect("under the bound");
    }
    assert_eq!(many.segments().len(), RING_SEGMENTS_MAX);
    assert!(
        many.grown_with(RingSegment {
            base: 20 * JOURNAL_PAGE_LEN,
            pages: 1
        })
        .is_err(),
        "a ninth segment refuses"
    );
}

#[test]
fn sym_ring_kb_is_a_registered_int_knob_with_the_derived_range() {
    let knob = squeezefs::env_knobs::lookup("SQUEEZEFS_SYM_RING_KB")
        .expect("ENG-10: every knob a site reads is registered");
    match knob.kind {
        squeezefs::env_knobs::Kind::Int { lo, hi } => {
            assert_eq!(
                lo,
                (SYM_RING_FLOOR_BYTES / 1024) as i128,
                "the floor in KiB"
            );
            // The ceiling is per volume (the solo ring's derivation); the
            // registry's bound is the derivation's own ceiling — 32 MiB.
            assert_eq!(hi, (journal_ring_len(u64::MAX) / 1024) as i128);
        }
        other => panic!("SQUEEZEFS_SYM_RING_KB must be an Int knob, got {other:?}"),
    }
    assert_eq!(knob.default, "derived");
}

// ---------------------------------------------------------------------------
// §5.3.4 — the three per-ring violation classes.
// ---------------------------------------------------------------------------

fn recovery(entries: Vec<(u64, Vec<(u8, Record)>)>) -> JournalRecovery {
    let head_pos = entries.iter().map(|(s, _)| *s + 1).max().unwrap_or(0);
    JournalRecovery {
        entries: entries
            .into_iter()
            .map(|(seq, records)| ReplayedEntry { seq, records })
            .collect(),
        head_pos,
        dropped_torn: 0,
        foreign_pages: 0,
    }
}

fn content(slot: u32, ino_local: u64) -> (u8, Record) {
    let ino = if slot == NATIVE_FOREST_SLOT {
        ino_local
    } else {
        squeezefs::meta_backend::guest_local_ino((slot - 1) as u16, ino_local)
    };
    let key = forest_key(TREE_INODES, &inode_key(ino)).unwrap();
    (tag_for(TREE_INODES, 0), Record::put(key, 0, vec![1]))
}

#[test]
fn appender_violations_key_lease_extent_are_each_detected() {
    let s5 = guest_forest_slot(5);
    let s6 = guest_forest_slot(6);
    let mut leases: std::collections::BTreeMap<u32, std::collections::BTreeSet<u32>> =
        std::collections::BTreeMap::new();
    leases.insert(1, [s5].into_iter().collect());

    // A clean partition: ring 0 writes native + slot 6, ring 1 writes slot 5.
    let clean = vec![
        (
            0u32,
            recovery(vec![
                (0, vec![content(NATIVE_FOREST_SLOT, 9)]),
                (100, vec![content(s6, 1)]),
            ]),
        ),
        (1u32, recovery(vec![(0, vec![content(s5, 1)])])),
    ];
    let no_grant = |_: u32, _: u64| false;
    let none: std::collections::BTreeSet<u32> = Default::default();
    assert!(detect_appender_violations(&clean, &leases, &no_grant, &none).is_empty());

    // Key: the same key in two rings.
    let key_dup = vec![
        (0u32, recovery(vec![(0, vec![content(s6, 1)])])),
        (1u32, recovery(vec![(0, vec![content(s6, 1)])])),
    ];
    let v = detect_appender_violations(&key_dup, &leases, &no_grant, &none);
    assert!(
        v.iter().any(|x| matches!(
            x,
            AppenderViolation::Key {
                appenders: (0, 1),
                ..
            }
        )),
        "{v:?}"
    );
    // Lease: ring 1 writes a slot it does not lease; ring 0 writes a slot
    // appender 1 leases; ring 1 carries a tree-0 record (the manager's
    // structure) and an interior record for slot 7, which it does NOT
    // lease. Its interior record for slot 6 — ITS slot's own SMO (PR 3,
    // §5.2.3) — is legal and counts nothing.
    let lease_bad = vec![
        (0u32, recovery(vec![(0, vec![content(s5, 2)])])),
        (
            1u32,
            recovery(vec![
                (0, vec![content(s6, 2)]),
                (
                    50,
                    vec![(
                        tag_for(TREE_CONTROL, 0),
                        Record::put(b"slot_state:xxxx".to_vec(), 0, vec![]),
                    )],
                ),
                (
                    60,
                    vec![(
                        tag_for(KIND_INTERIOR, 1),
                        Record::put(vec![0, 0, 0, 7, 0xFF], 0, vec![]),
                    )],
                ),
                (
                    70,
                    vec![(
                        tag_for(KIND_INTERIOR, 1),
                        Record::put(vec![0, 0, 0, 6, 0xFF], 0, vec![]),
                    )],
                ),
            ]),
        ),
    ];
    let v = detect_appender_violations(&lease_bad, &leases, &no_grant, &none);
    let lease_hits = v
        .iter()
        .filter(|x| matches!(x, AppenderViolation::Lease { .. }))
        .count();
    assert_eq!(lease_hits, 4, "{v:?}");
    assert!(
        v.iter().any(|x| matches!(
            x,
            AppenderViolation::Lease {
                appender_id: 1,
                slot: Some(7),
                ..
            }
        )),
        "the foreign-slot interior record is the violation, not the own-slot one: {v:?}"
    );
    assert!(v
        .iter()
        .any(|x| matches!(x, AppenderViolation::Lease { appender_id: 0, .. })));
    assert!(v
        .iter()
        .any(|x| matches!(x, AppenderViolation::Lease { appender_id: 1, .. })));
    // Extent: an allocator delta in a ring whose appender holds no grant
    // covering it — and, granted, the same delta is the appender's own
    // (§5.3.3: an alloc record is its own only INSIDE a grant).
    let alloc = squeezefs::meta_backend::kv::alloc_ext::alloc_record(77, 0);
    let extent_bad = vec![
        (0u32, recovery(vec![(0, vec![alloc.clone()])])),
        (1u32, recovery(vec![(0, vec![alloc])])),
    ];
    let v = detect_appender_violations(&extent_bad, &leases, &no_grant, &none);
    assert_eq!(
        v.len(),
        1,
        "the manager's delta is legal, appender 1's is not: {v:?}"
    );
    assert!(matches!(
        v[0],
        AppenderViolation::Extent {
            appender_id: 1,
            extent: 77,
            ..
        }
    ));
    let granted = |appender: u32, extent: u64| appender == 1 && (72..80).contains(&extent);
    assert!(
        detect_appender_violations(&extent_bad, &leases, &granted, &none).is_empty(),
        "inside its grant the delta is appender 1's own"
    );
}

/// **PR 10, review round 4, Issue 31 — the `recovering` exemption keys on
/// the PAGE STATE of the slot's tree-0 lessee, never on the writer of the
/// record.** Appender 1 leases slot 5. The MANAGER's interior record for
/// slot 5 in ring 0 is the `Lease` class while appender 1's page is `Live`
/// (the set is empty); with appender 1's page `Recovering` (slot 5 in the
/// set — a recovery in flight, whose step-6 flush compacted the slot's
/// leaves and journaled the flips into ring 0) the SAME record is legal.
/// Nothing else moves: the manager's CONTENT record for slot 5 stays the
/// violation (the door refuses the manager's commits into a leased slot,
/// dead lessee or not), appender 1's own interior record for slot 5 in ITS
/// ring stays legal (its own-residue replay at a rejoin), appender 2's
/// interior record for slot 5 in ITS ring stays the violation (a foreign
/// ring is judged by the lease map alone, whatever the set says), and the
/// manager's interior record for slot 6 (appender 1's other lease, NOT in
/// the set) stays the violation.
#[test]
fn the_recovering_exemption_admits_the_managers_structure_alone_and_only_for_the_named_slots() {
    let s5 = guest_forest_slot(5);
    let s6 = guest_forest_slot(6);
    let mut leases: std::collections::BTreeMap<u32, std::collections::BTreeSet<u32>> =
        std::collections::BTreeMap::new();
    leases.insert(1, [s5, s6].into_iter().collect());
    let no_grant = |_: u32, _: u64| false;
    let interior = |slot: u32| {
        (
            tag_for(KIND_INTERIOR, 1),
            Record::put([slot.to_be_bytes().as_slice(), &[0xFF]].concat(), 0, vec![]),
        )
    };
    let none: std::collections::BTreeSet<u32> = Default::default();
    let recovering: std::collections::BTreeSet<u32> = [s5].into_iter().collect();

    // The manager's flip of slot 5: a violation under a Live lessee, legal
    // under a Recovering one.
    let managers_flip = vec![(0u32, recovery(vec![(0, vec![interior(s5)])]))];
    let v = detect_appender_violations(&managers_flip, &leases, &no_grant, &none);
    assert!(
        matches!(
            v.as_slice(),
            [AppenderViolation::Lease {
                appender_id: 0,
                slot: Some(s),
                ..
            }] if *s == s5
        ),
        "a live lessee's slot written into ring 0 is the Lease class: {v:?}"
    );
    assert!(
        detect_appender_violations(&managers_flip, &leases, &no_grant, &recovering).is_empty(),
        "the recovery in flight owns the slot's structure"
    );
    // The manager's CONTENT for slot 5: never exempt.
    let managers_content = vec![(0u32, recovery(vec![(0, vec![content(s5, 3)])]))];
    let v = detect_appender_violations(&managers_content, &leases, &no_grant, &recovering);
    assert_eq!(
        v.len(),
        1,
        "content into a leased slot stays the Lease class: {v:?}"
    );
    // Slot 6 (the lessee's other lease, not recovering): never exempt.
    let managers_other_flip = vec![(0u32, recovery(vec![(0, vec![interior(s6)])]))];
    let v = detect_appender_violations(&managers_other_flip, &leases, &no_grant, &recovering);
    assert_eq!(v.len(), 1, "only the NAMED slots are exempt: {v:?}");
    // The lessee's own flip in ITS ring: legal either way (its rejoin's
    // own-residue replay).
    let lessees_flip = vec![(1u32, recovery(vec![(0, vec![interior(s5)])]))];
    assert!(detect_appender_violations(&lessees_flip, &leases, &no_grant, &none).is_empty());
    assert!(detect_appender_violations(&lessees_flip, &leases, &no_grant, &recovering).is_empty());
    // A THIRD appender's flip of slot 5 in its ring: the violation either
    // way — the exemption is the manager's, never a foreign writer's.
    let strangers_flip = vec![(2u32, recovery(vec![(0, vec![interior(s5)])]))];
    let v = detect_appender_violations(&strangers_flip, &leases, &no_grant, &recovering);
    assert!(
        matches!(
            v.as_slice(),
            [AppenderViolation::Lease { appender_id: 2, .. }]
        ),
        "{v:?}"
    );
}

// ---------------------------------------------------------------------------
// The mounted region: join, the page per checkpoint, own-residue recovery,
// the foreign refusal, two appenders, growth, the flush ceiling.
// ---------------------------------------------------------------------------

use squeezefs::meta_backend::kv::appender::{
    appender_flush_ceiling_ms, appender_flush_ceiling_service_cap_ms, write_page, AppenderStats,
    TEST_APPENDER_SLOTS_ENV,
};
use squeezefs::meta_backend::kv::backend::{
    test_conveyor_hold_release, TEST_CONVEYOR_HOLD_PRE_ROLLBACK, TEST_CONVEYOR_HOLD_STAGE,
};
use squeezefs::meta_backend::kv::block_refs::{volume_tag, BlockRef, BlockRefOp};
use squeezefs::meta_backend::kv::builder::{digest_backend, format_v3_stamped, FormatV3Options};
use squeezefs::meta_backend::kv::checkpoint::{
    checkpoint_landing_ceiling_ms, checkpoint_tick_period_ms, read_newest_ledger,
};
use squeezefs::meta_backend::kv::node::{write_node, NodeLayout, NodeWriteParams, MIN_NODE_SIZE};
use squeezefs::meta_backend::kv::node_cache::{
    NodeCache, NodeCacheConfig, OwnedRec, DEFAULT_WRITEBACK_DELTA_BYTES,
};
use squeezefs::meta_backend::kv::record::RecordKind;
use squeezefs::meta_backend::kv::{
    META_KV_BITMAP_GENERATION_RAISES, META_KV_REPLAY_EXTENT_VIOLATIONS,
    META_KV_REPLAY_KEY_VIOLATIONS, META_KV_REPLAY_LEASE_VIOLATIONS,
};
use squeezefs::meta_backend::{
    guest_local_ino, open_routed_meta_set, open_volume_for_mount, plan_meta_slot_set, Metadata,
    RoutedMetaBackend,
};
use std::sync::atomic::Ordering;
use std::sync::Arc;

fn set_opts() -> FormatV3Options {
    FormatV3Options {
        node_size: NODE_SIZE,
        journal_len_override: Some(RING_LEN),
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// Format a one-member stamped set (the default format's bits + bit 17)
/// at `dir/name`; the seam guard is the caller's.
async fn format_stamped_member(dir: &std::path::Path, name: &str) -> String {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
    let r = format_v3_stamped(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone()).await;
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    r.expect("format stamped member");
    p.display().to_string()
}

/// Open the set with the declared partition `partition` in force for the
/// open (the env is read at the writer's open; cleared after). A declared
/// partition ARMS the symmetric plane, and a file-backed volume is a
/// non-PR substrate — KD-SYM-13 (PR 3) needs the loud opt-in here.
async fn open_with_partition(uris: &[String], partition: Option<&str>) -> Arc<RoutedMetaBackend> {
    match partition {
        Some(p) => {
            std::env::set_var(TEST_APPENDER_SLOTS_ENV, p);
            std::env::set_var("SQUEEZEFS_SYM_ALLOW_NON_PR", "1");
        }
        None => std::env::remove_var(TEST_APPENDER_SLOTS_ENV),
    }
    let r = open_routed_meta_set(uris).await;
    std::env::remove_var(TEST_APPENDER_SLOTS_ENV);
    std::env::remove_var("SQUEEZEFS_SYM_ALLOW_NON_PR");
    r.expect("open routed set")
}

fn stats(vol: &squeezefs::meta_backend::kv::backend::KvMetaBackend) -> AppenderStats {
    vol.appender_stats()
        .expect("a forest volume has an appender set")
}

/// `n` block references of `owner` on volume tag `tag`, blocks
/// `base..base+n`.
fn refs(tag: u64, owner: u64, base: u64, n: u64) -> Vec<BlockRefOp> {
    (0..n)
        .map(|i| {
            BlockRefOp::taken(BlockRef {
                vol_tag: tag,
                block_idx: base + i,
                owner_ino: owner,
                block_index: i as u32,
            })
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flat_mount_has_no_appender_set() {
    let file = NamedTempFile::new().unwrap();
    build_image(&file, false).await;
    let b = open_volume_for_mount(file.path().to_str().unwrap())
        .await
        .unwrap();
    assert!(
        b.appender_stats().is_none(),
        "a bit-17-absent mount has no appender region — every Appender gauge is absent"
    );
    b.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_writer_joins_appender_zero_and_writes_its_page_per_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_with_partition(&uris, None).await;
    let vol = &routed.volumes[0];
    let s = stats(vol);
    assert_eq!(s.appender_id, 0);
    assert_eq!((s.joins, s.leaves, s.self_recoveries, s.live), (1, 0, 0, 1));
    let sb = vol.superblock().clone();
    assert_eq!(
        s.capacity,
        appenders_capacity(
            sb.heap.len,
            squeezefs::meta_backend::kv::appender::resolve_sym_ring_bytes(0, VOL_LEN)
        )
    );
    assert_eq!(
        s.ring_segments, 1,
        "appender 0's ring is the fixed extent, one segment"
    );
    assert_eq!(s.ring_bytes, appender0_ring_extent(&sb.journal).len);
    let path = std::path::Path::new(&uris[0]);
    let entries = read_directory(path, &sb).await.unwrap();
    let page = entries[0].page.clone().expect("page 0");
    assert_eq!(page.state, AppenderState::Live);
    assert_eq!(page.term, 1, "the first join of a Free page is term 1");
    assert!(
        page.is_manager,
        "the solo writer plays the manager (KD-SYM-3)"
    );
    let gen_before = page.generation;

    // A checkpoint writes the page: tail + ckpt_seq mirror the fixed
    // ledger; the slot vector names the native slot tree's root; tree 0's
    // root and the stamp are NOT on the page.
    Metadata::create(vol.as_ref(), ROOT_INO, "d", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    vol.checkpoint_now().await.unwrap();
    let ledger = read_newest_ledger(path, sb.root_ledger.start)
        .await
        .unwrap()
        .expect("ledger");
    let page = read_directory(path, &sb).await.unwrap()[0]
        .page
        .clone()
        .unwrap();
    assert!(
        page.generation > gen_before,
        "one page write per checkpoint"
    );
    assert_eq!(page.ledger_tail_seq, ledger.journal_tail_seq);
    assert_eq!(page.ckpt_seq, ledger.seq);
    let native_root = ledger
        .tree_roots
        .iter()
        .find(|r| r.tree_id == KIND_INTERIOR)
        .unwrap();
    let native_entry = page
        .slots
        .iter()
        .find(|e| e.slot == s.native_slot)
        .expect("the page names the native slot it leases");
    assert_eq!(
        (native_entry.root.addr, native_entry.root.seq),
        (native_root.node_addr, native_root.node_seq)
    );
    // KD-SYM-3: no page names tree 0's root — checked against the ROOT
    // ADDRESS the ledger publishes for tree 0, not a slot value no code
    // path produces.
    let tree0_root = ledger
        .tree_roots
        .iter()
        .find(|r| r.tree_id == TREE_CONTROL)
        .expect("the ledger names tree 0");
    assert_ne!(tree0_root.node_addr, 0);
    assert!(
        page.slots
            .iter()
            .all(|e| e.root.addr != tree0_root.node_addr),
        "no entry of the manager's page names tree 0's root {:#x}",
        tree0_root.node_addr
    );

    // A clean shutdown releases the region: the page goes Free.
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
    let page = read_directory(path, &sb).await.unwrap()[0]
        .page
        .clone()
        .unwrap();
    assert_eq!(page.state, AppenderState::Free);
    assert_eq!(page.term, 1, "the id and its term survive the release");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn own_residue_is_recovered_at_rejoin_with_a_bumped_term() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_with_partition(&uris, None).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let d = routed
        .create(ROOT_INO, "storm", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    for i in 0..24u32 {
        routed
            .create(d, &format!("f{i:03}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
    }
    let live = digest_backend(&vol).await.unwrap();
    // Barrier, then die without a checkpoint: page 0 stays Live with a
    // non-empty window behind it.
    vol.sync_device().await.unwrap();
    drop(vol);
    drop(routed);
    let again = open_with_partition(&uris, None).await;
    let vol = &again.volumes[0];
    let s = stats(vol);
    assert_eq!(
        s.self_recoveries, 1,
        "a Live page of our own ⇒ recover our own ring first"
    );
    assert_eq!(s.joins, 1);
    assert_eq!(s.regions[0].term, 2, "re-adopted with a bumped term");
    assert!(vol.replay_stats().entries > 0);
    assert_eq!(digest_backend(vol).await.unwrap(), live);
    assert_eq!(again.readdir(d, 0, usize::MAX).await.unwrap().len(), 24);
    // A clean shutdown then a rejoin: nothing to recover, term bumps again.
    for v in &again.volumes {
        v.shutdown().await.unwrap();
    }
    drop(again);
    let third = open_with_partition(&uris, None).await;
    let s = stats(&third.volumes[0]);
    assert_eq!(s.self_recoveries, 0);
    assert_eq!(s.regions[0].term, 3);
    for v in &third.volumes {
        v.shutdown().await.unwrap();
    }
}

/// The kill-9 successor at ANOTHER mount point of the same node (the
/// shape `inline_raise_tests` runs live: `kill9()` the promoter, remount
/// the volume at `mnt2`) — its mount slot is `xxh3(canonical mount
/// point)`, so it differs, but the writer flock it holds at this point of
/// the open IS the D0 same-host death proof: a same-NODE Live page can
/// only be a dead predecessor's residue, and refusing it would make the
/// forest volume the one layout a same-host crash cannot remount over.
/// Found stamped-only by the live-FUSE leg of the PR-2 verification.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_same_node_live_page_under_another_mount_slot_is_own_residue_under_d0() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let path = std::path::Path::new(&uris[0]);
    let sb = superblock_of_path(path).await;
    let routed = open_with_partition(&uris, None).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let d = routed
        .create(ROOT_INO, "promoter", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    for i in 0..8u32 {
        routed
            .create(d, &format!("f{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
    }
    let live = digest_backend(&vol).await.unwrap();
    vol.sync_device().await.unwrap();
    // Die without a checkpoint (page 0 Live, a window behind it) ...
    drop(vol);
    drop(routed);
    // ... and re-stamp the Live page with the slot another mount point of
    // THIS node derives: the same node token, a different mount slot.
    let offs = appender0_page_offsets(&sb.journal);
    let mut page = read_directory(path, &sb).await.unwrap()[0]
        .page
        .clone()
        .expect("the dead mount left a valid page");
    assert_eq!(page.state, AppenderState::Live);
    let our_slot = page.identity.mount_slot;
    page.identity.mount_slot = our_slot ^ 0x5A5A_0001;
    page.generation += 1;
    write_page(
        path,
        offs[page_slot_for(page.generation)],
        page.encode().unwrap(),
    )
    .await
    .unwrap();
    let again = open_with_partition(&uris, None).await;
    let vol = &again.volumes[0];
    let s = stats(vol);
    assert_eq!(
        s.self_recoveries, 1,
        "a same-node Live page is OUR residue under the D0 flock, whatever mount point \
         the dead predecessor used"
    );
    assert_eq!(s.regions[0].term, 2, "re-adopted with a bumped term");
    assert_eq!(digest_backend(vol).await.unwrap(), live);
    assert_eq!(again.readdir(d, 0, usize::MAX).await.unwrap().len(), 8);
    // The rejoined page is bound to THIS mount's slot again.
    let listed = read_directory(path, &sb).await.unwrap();
    let bound = listed[0].page.as_ref().unwrap();
    assert_eq!(bound.state, AppenderState::Live);
    assert_eq!(bound.identity.mount_slot, our_slot);
    for v in &again.volumes {
        v.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_foreign_live_page_refuses_the_writer_open_and_a_probe_lists_it() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let path = std::path::Path::new(&uris[0]);
    let sb = superblock_of_path(path).await;
    // Forge: appender 1's page (the directory's first pair) Live under an
    // identity that is not ours. Page 0 is the MANAGER's and is the one
    // page a writer that won the D0 ladder ADOPTS (PR 3's successor arm,
    // §5.9 — pinned in `sym_manager_tests`); every other foreign Live
    // page refuses the join until PR 10's recovery driver.
    let offs = dir_pair_offsets(&sb.appender_dir, 0);
    let mut page = AppenderPage::free(1, 0);
    page.generation = 1000;
    page.state = AppenderState::Live;
    page.term = 9;
    page.identity = AppenderIdentity {
        node_token: 0xF0E1_D2C3_B4A5_9687,
        mount_slot: 0x1234_5678,
        writer_id: 42,
    };
    write_page(path, offs[0], page.encode().unwrap())
        .await
        .unwrap();
    // The non-joining Writer opens are NOT blocked by the page: `claim
    // clear` — the attested D0 remedy for a dead foreign-host writer on a
    // non-PR substrate — opens Writer posture, joins nothing, and answers
    // (before the refused mount below leaves its own fresh claim, which
    // `claim clear` rightly refuses to touch — a possibly-live holder).
    let cleared = squeezefs::meta_backend::kv::backend::KvMetaBackend::claim_clear(path)
        .await
        .expect("`claim clear` is never blocked by an appender page it does not write");
    assert!(
        matches!(
            cleared,
            squeezefs::meta_backend::kv::backend::ClaimClearOutcome::NoClaim
        ),
        "a fresh volume carries no writer claim: {cleared:?}"
    );
    // A DECLARED region whose id a foreign Live page holds: the seam
    // would steal a joined appender's page — refused. (An undeclared
    // foreign Live page ≥ 1 is a JOINED appender, the directory's normal
    // state since PR 3's `JoinAppender`, and refuses nothing.)
    std::env::set_var(TEST_APPENDER_SLOTS_ENV, PARTITION);
    std::env::set_var("SQUEEZEFS_SYM_ALLOW_NON_PR", "1");
    let r = open_routed_meta_set(&uris).await;
    std::env::remove_var(TEST_APPENDER_SLOTS_ENV);
    std::env::remove_var("SQUEEZEFS_SYM_ALLOW_NON_PR");
    let err = match r {
        Ok(_) => panic!("a writer must not mount a declared region over a foreign Live page"),
        Err(e) => e.to_string(),
    };
    // The refusal names the remedy this binary HAS (PR 10: the death
    // ledger's driver recovers a recorded death; `squeezefs appender
    // clear` attests one the plane never recorded); it fires at the
    // JOIN, after the D0 claim gate — a live foreign holder gets D0's
    // own message first.
    assert!(
        err.contains("refusing to join")
            && err.contains("appender clear")
            && err.contains("death ledger"),
        "the refusal names the ledger's driver and the attestation verb: {err}"
    );
    // A probe never writes and lists the page as it stands.
    let probe = squeezefs::meta_backend::kv::backend::KvMetaBackend::open_probe(path)
        .await
        .expect("a probe opens beside a foreign Live page");
    let s = stats(&probe);
    assert_eq!(s.live_pages_at_mount, 1);
    assert_eq!(s.joins, 0, "a probe joins nothing");
    assert_eq!(
        s.live, 0,
        "a probe holds no Live region: the closure law holds on every posture"
    );
    let listed = read_directory(path, &sb).await.unwrap();
    assert_eq!(listed[1].page.as_ref().unwrap().term, 9);
    // The page is untouched by any of it.
    let listed = read_directory(path, &sb).await.unwrap();
    let p = listed[1].page.as_ref().unwrap();
    assert_eq!(
        (p.state, p.term, p.generation),
        (AppenderState::Live, 9, 1000)
    );
}

/// A `Recovering` page of OUR OWN node (§5.9, PR 10): our dead
/// predecessor's page, mid-recovery by a recoverer that died (or an
/// operator's `appender clear` attestation of it) — the D0 winner replays
/// the fixed ring as its own residue exactly as it does a `Live` one
/// (the partial recovery folded the same records, idempotent by seq) and
/// JOINS over it: the page is `Live` under us again and every record the
/// predecessor acked is served. Before PR 10 the state refused the join
/// of any identity (nothing wrote it); a FOREIGN node's `Recovering` page
/// is the mount-path gate's (recovered when the ledger names it, refused
/// naming the verb when nothing does — `sym_crash_matrix_tests`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_own_recovering_page_is_own_residue_and_the_join_takes_it_back() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let path = std::path::Path::new(&uris[0]);
    let sb = superblock_of_path(path).await;
    // Our OWN identity, mid-recovery: mount once (page Live under us),
    // create under it, then re-stamp the newest page Recovering without
    // a leave (the ring window stays).
    let routed = open_with_partition(&uris, None).await;
    let ino = routed
        .create(
            ROOT_INO,
            "acked_before_the_kill",
            libc::S_IFREG | 0o644,
            0,
            0,
        )
        .await
        .unwrap()
        .ino;
    drop(routed);
    let offs = appender0_page_offsets(&sb.journal);
    let mut page = read_directory(path, &sb).await.unwrap()[0]
        .page
        .clone()
        .unwrap();
    assert_eq!(page.state, AppenderState::Live, "no leave ran");
    page.state = AppenderState::Recovering;
    page.generation += 1;
    write_page(
        path,
        offs[page_slot_for(page.generation)],
        page.encode().unwrap(),
    )
    .await
    .unwrap();
    let again = open_routed_meta_set(&uris)
        .await
        .expect("the join takes our page back");
    let s = stats(&again.volumes[0]);
    assert_eq!(s.self_recoveries, 1, "the Recovering page was own residue");
    assert_eq!(
        again
            .lookup(ROOT_INO, "acked_before_the_kill")
            .await
            .unwrap()
            .ino,
        ino
    );
    let listed = read_directory(path, &sb).await.unwrap();
    assert_eq!(
        listed[0].page.as_ref().unwrap().state,
        AppenderState::Live,
        "the join wrote the page Live under us"
    );
    for v in &again.volumes {
        v.shutdown().await.unwrap();
    }
}

async fn superblock_of_path(path: &std::path::Path) -> SuperblockV3 {
    match classify_volume(path).await.expect("classify") {
        VolumeFormat::V3(sb) => sb,
        other => panic!("expected a v3 superblock, got {other:?}"),
    }
}

/// The two-appender shape: appender 1 leases forest slot `guest 3`; the
/// native slot stays the manager's. Every tx of `commit_block_refs` lives
/// in its owner's slot, so the partition is respected by construction.
const PARTITION: &str = "1:4";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_appenders_commit_into_two_rings_and_replay_to_the_union_digest() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let a = vec![format_stamped_member(dir.path(), "a").await];
    let b = vec![format_stamped_member(dir.path(), "b").await];
    let tag = volume_tag("vol-0011223344556677");
    let manager_owner = 1000u64; // native slot
    let guest_owner = guest_local_ino(3, 77); // forest slot 4 — appender 1's

    // Volume A: partitioned — two rings written concurrently.
    let ra = open_with_partition(&a, Some(PARTITION)).await;
    let va = Arc::clone(&ra.volumes[0]);
    let s = stats(&va);
    assert_eq!(s.live, 2, "two regions joined");
    assert_eq!(s.joins, 2);
    assert_eq!(s.regions.len(), 2);
    assert_eq!(s.regions[1].id, 1);
    let (v1, v2) = (Arc::clone(&va), Arc::clone(&va));
    let t1 = tokio::spawn(async move {
        for i in 0..16u64 {
            v1.commit_block_refs(manager_owner, &refs(tag, manager_owner, i * 10, 3))
                .await
                .unwrap();
        }
    });
    let t2 = tokio::spawn(async move {
        for i in 0..16u64 {
            v2.commit_block_refs(guest_owner, &refs(tag, guest_owner, 1000 + i * 10, 3))
                .await
                .unwrap();
        }
    });
    t1.await.unwrap();
    t2.await.unwrap();
    let s = stats(&va);
    assert!(s.regions[0].ring_entries >= 16, "{:?}", s.regions);
    assert_eq!(
        s.regions[1].ring_entries, 16,
        "appender 1's commits rode ITS ring"
    );
    let live_a = digest_backend(&va).await.unwrap();
    va.sync_device().await.unwrap();
    drop(va);
    drop(ra);

    // Volume B: one appender commits the union.
    let rb = open_with_partition(&b, None).await;
    let vb = Arc::clone(&rb.volumes[0]);
    for i in 0..16u64 {
        vb.commit_block_refs(manager_owner, &refs(tag, manager_owner, i * 10, 3))
            .await
            .unwrap();
        vb.commit_block_refs(guest_owner, &refs(tag, guest_owner, 1000 + i * 10, 3))
            .await
            .unwrap();
    }
    let live_b = digest_backend(&vb).await.unwrap();
    assert_eq!(
        live_a, live_b,
        "the same records fold to the same digest, one ring or two"
    );
    vb.sync_device().await.unwrap();
    drop(vb);
    drop(rb);

    // Both replay (own-residue for A's two rings) to the live digest; A
    // replays twice to the same digest.
    let ra = open_with_partition(&a, Some(PARTITION)).await;
    let va = &ra.volumes[0];
    assert_eq!(
        stats(va).self_recoveries,
        2,
        "both of our Live pages were recovered"
    );
    assert_eq!(digest_backend(va).await.unwrap(), live_a);
    assert_eq!(va.block_ref_count(tag, 1005).await.unwrap(), 0);
    assert_eq!(va.block_ref_count(tag, 1010).await.unwrap(), 1);
    assert_eq!(va.block_ref_count(tag, 10).await.unwrap(), 1);
    drop(ra);
    let ra2 = open_with_partition(&a, Some(PARTITION)).await;
    assert_eq!(digest_backend(&ra2.volumes[0]).await.unwrap(), live_a);
    let rb = open_with_partition(&b, None).await;
    assert_eq!(digest_backend(&rb.volumes[0]).await.unwrap(), live_b);
    for v in ra2.volumes.iter().chain(rb.volumes.iter()) {
        v.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_changed_lease_set_refuses_the_mount_as_a_lease_violation() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag("vol-0011223344556677");
    let guest_owner = guest_local_ino(3, 5);
    // The cadence parked (the two-suite "park the timer" idiom): the
    // records must still sit in ring 1's WINDOW at the crash, not in a
    // flushed leaf the page's tail already passed.
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let ra = open_with_partition(&uris, Some(PARTITION)).await;
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    let va = Arc::clone(&ra.volumes[0]);
    va.commit_block_refs(guest_owner, &refs(tag, guest_owner, 0, 2))
        .await
        .unwrap();
    va.sync_device().await.unwrap();
    assert!(
        stats(&va).regions[1].ring_entries >= 1,
        "the commit rode appender 1's ring"
    );
    drop(va);
    drop(ra);
    let before = META_KV_REPLAY_LEASE_VIOLATIONS.load(Ordering::Relaxed);
    std::env::set_var(TEST_APPENDER_SLOTS_ENV, "1:9");
    std::env::set_var("SQUEEZEFS_SYM_ALLOW_NON_PR", "1");
    let r = open_routed_meta_set(&uris).await;
    std::env::remove_var(TEST_APPENDER_SLOTS_ENV);
    std::env::remove_var("SQUEEZEFS_SYM_ALLOW_NON_PR");
    let err = match r {
        Ok(_) => panic!("ring 1 holds records for slot 4, which appender 1 no longer leases"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("does not lease"), "{err}");
    assert!(META_KV_REPLAY_LEASE_VIOLATIONS.load(Ordering::Relaxed) > before);
    assert_eq!(META_KV_REPLAY_KEY_VIOLATIONS.load(Ordering::Relaxed), 0);
    assert_eq!(META_KV_REPLAY_EXTENT_VIOLATIONS.load(Ordering::Relaxed), 0);
}

/// §4.6 pt 2's ring-pressure trigger, per REGION: a committer parked at a
/// full declared ring is drained by the NEXT cadence tick, never by the
/// 1 s ceiling. The tick read ring 0 alone (`be.journal_ring()`), so a
/// full region ring never made a cycle due — its committer sat parked for
/// `CHECKPOINT_MAX_AGE_MS` per drain — and the shipped law's `distance >
/// logical_len / 2` is unreachable on a floor-sized ring anyway (its
/// reserve IS half the ring): the region's law is half its ADMISSIBLE
/// window. Found attributing the growth contract's 1-in-20 stall race.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_declared_ring_kicks_the_next_cadence_tick_not_the_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag("vol-0011223344556677");
    let guest_owner = guest_local_ino(3, 11);
    std::env::set_var("SQUEEZEFS_SYM_RING_KB", "512");
    // The DEFAULT cadence (50 ms): the tick is the only drain here.
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    let ra = open_with_partition(&uris, Some(PARTITION)).await;
    std::env::remove_var("SQUEEZEFS_SYM_RING_KB");
    let va = Arc::clone(&ra.volumes[0]);
    assert_eq!(stats(&va).regions[1].ring_bytes, 512 * 1024);
    let t0 = std::time::Instant::now();
    let committer = {
        let v = Arc::clone(&va);
        tokio::spawn(async move {
            for i in 0..40u64 {
                v.commit_block_refs(guest_owner, &refs(tag, guest_owner, i * 1000, 500))
                    .await
                    .unwrap();
            }
        })
    };
    // ≈ 1.08 MB into a 256 KiB user window: ≥ 4 drains, each a park.
    tokio::time::timeout(std::time::Duration::from_secs(60), committer)
        .await
        .expect("the storm drains — a parked committer is never stranded")
        .unwrap();
    let wall = t0.elapsed();
    let s = stats(&va);
    assert!(
        s.regions[1].stalls > 0,
        "the small ring parked: {:?}",
        s.regions[1]
    );
    assert!(
        s.pressure_cycles > 0,
        "a full region ring made a cycle due (appender_pressure_cycles): {s:?}"
    );
    // Each drain rides the next 50 ms tick (+ one cycle); the ceiling-
    // driven shape costs ≥ CHECKPOINT_MAX_AGE_MS PER drain, so the whole
    // storm under two ceilings separates the two by ≥ 2×.
    let bound = std::time::Duration::from_millis(2 * CHECKPOINT_MAX_AGE_MS as u64);
    assert!(
        wall < bound,
        "the storm's {} drains took {wall:?} — parked committers waited for the ceiling, \
         not the tick: {:?}",
        s.pressure_cycles,
        s.regions[1]
    );
    for i in [0u64, 17, 39] {
        assert_eq!(va.block_ref_count(tag, i * 1000 + 7).await.unwrap(), 1);
    }
    drop(va);
    for v in &ra.volumes {
        v.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_appender_ring_grows_a_segment_and_its_content_survives() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag("vol-0011223344556677");
    let guest_owner = guest_local_ino(3, 9);
    std::env::set_var("SQUEEZEFS_SYM_RING_KB", "512");
    // The cadence parked: the test is the ONLY drain of ring 1 (see the
    // stall wait below).
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let ra = open_with_partition(&uris, Some(PARTITION)).await;
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    std::env::remove_var("SQUEEZEFS_SYM_RING_KB");
    let va = Arc::clone(&ra.volumes[0]);
    let s = stats(&va);
    assert_eq!(
        s.regions[1].ring_bytes,
        512 * 1024,
        "the knob sized appender 1's ring"
    );
    assert_eq!(s.regions[1].segments, 1);
    // 40 × ~27 KB entries ≈ 1 MiB into a 512 KiB ring whose user window
    // is 256 KiB (the reserve is the other half): with the cadence PARKED
    // (`SQUEEZEFS_META_FLUSH_INTERVAL_MS` above) nothing but this test
    // advances ring 1's tail, so the committer MUST park at the ring by
    // capacity arithmetic — the stall gauge moving is the certainty the
    // poller waits for BEFORE it starts draining. (The first shape raced
    // the poller's `checkpoint_now` loop against the committer's fill and
    // read `stalls == 0` on one stamped run in twenty — a legal schedule,
    // not a defect: the poller drained faster than 40 commits filled.)
    let committer = {
        let v = Arc::clone(&va);
        tokio::spawn(async move {
            for i in 0..40u64 {
                v.commit_block_refs(guest_owner, &refs(tag, guest_owner, i * 1000, 500))
                    .await
                    .unwrap();
            }
        })
    };
    let stall_deadline = std::time::Instant::now() + std::time::Duration::from_secs(20); // ≪ the 30 s D1.b park
    while stats(&va).regions[1].stalls == 0 {
        assert!(
            !committer.is_finished(),
            "40 × 27 KB cannot fit a 256 KiB user window without a drain: {:?}",
            stats(&va).regions[1]
        );
        assert!(
            std::time::Instant::now() < stall_deadline,
            "the committer never parked at the small ring: {:?}",
            stats(&va).regions[1]
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    while !committer.is_finished() {
        va.checkpoint_now().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    committer.await.unwrap();
    let s = stats(&va);
    assert!(
        s.regions[1].stalls > 0,
        "the small ring stalled: {:?}",
        s.regions[1]
    );
    // Two barriered cycles: the first covers the last entries (its tail
    // may still sit under a dying leaf floor — reclamation lags a cycle by
    // design); the second's barrier leaves the ring drained with stalls
    // behind it, and the growth decision at that cycle's end grows it.
    va.checkpoint_now().await.unwrap();
    va.checkpoint_now().await.unwrap();
    let s = stats(&va);
    assert!(s.ring_grows >= 1, "{:?}", s.regions[1]);
    assert!(s.regions[1].segments >= 2 && s.regions[1].segments <= RING_SEGMENTS_MAX as u64);
    assert!(s.regions[1].ring_bytes > 512 * 1024);
    // The page names the grown segment table.
    let path = std::path::Path::new(&uris[0]);
    let sb = va.superblock().clone();
    let page1 = read_directory(path, &sb).await.unwrap()[1]
        .page
        .clone()
        .expect("appender 1's page");
    assert_eq!(page1.segments.len() as u64, s.regions[1].segments);
    // Content intact, live and after a crash-replay through the grown ring.
    for i in [0u64, 17, 39] {
        assert_eq!(va.block_ref_count(tag, i * 1000 + 7).await.unwrap(), 1);
    }
    va.commit_block_refs(guest_owner, &refs(tag, guest_owner, 90_000, 4))
        .await
        .unwrap();
    va.sync_device().await.unwrap();
    let live = digest_backend(&va).await.unwrap();
    drop(va);
    drop(ra);
    let again = open_with_partition(&uris, Some(PARTITION)).await;
    let v = &again.volumes[0];
    assert_eq!(digest_backend(v).await.unwrap(), live);
    assert_eq!(v.block_ref_count(tag, 90_002).await.unwrap(), 1);
    for v in &again.volumes {
        v.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_flush_ceiling_is_the_checkpoint_age_and_a_parked_device_moves_the_overrun_counter() {
    // KD-SYM-10's "within CHECKPOINT_MAX_AGE_MS" names the cadence
    // TRIGGER; a leaf is durable one LANDING later (the trigger plus the
    // two tick-granularity terms the reader's qualify term already
    // derives), so the audit's ceiling IS that derivation — at the shipped
    // 50 ms flush, 1,100 ms; on a parked cadence, the parked tick's.
    for interval in [0u64, 50, 200, 60_000] {
        assert_eq!(
            appender_flush_ceiling_ms(interval),
            checkpoint_landing_ceiling_ms(interval),
            "the flush ceiling is the checkpoint LANDING ceiling of the cadence in force"
        );
        assert_eq!(
            appender_flush_ceiling_ms(interval),
            CHECKPOINT_MAX_AGE_MS as u64 + 2 * checkpoint_tick_period_ms(interval)
        );
    }
    assert_eq!(appender_flush_ceiling_ms(50), 1_100);
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let path = std::path::PathBuf::from(&uris[0]);
    let ra = open_with_partition(&uris, None).await;
    let va = Arc::clone(&ra.volumes[0]);
    for i in 0..8u32 {
        ra.create(ROOT_INO, &format!("n{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        va.checkpoint_now().await.unwrap();
    }
    assert_eq!(
        stats(&va).flush_ceiling_overruns,
        0,
        "a normal run never overruns"
    );
    // A parked device: the flush pass's covering barrier lands past the
    // ceiling while a dirty leaf waited on it (the ceiling in force is the
    // default cadence's; the gauge publishes it).
    let ceiling = stats(&va).flush_ceiling_ms;
    assert_eq!(
        ceiling,
        appender_flush_ceiling_ms(50),
        "the default cadence's landing ceiling"
    );
    let park = std::time::Duration::from_millis(ceiling + 400);
    squeezefs::uring_fs::arm_device_latency(&path, std::time::Duration::ZERO, park);
    ra.create(ROOT_INO, "late", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    va.checkpoint_now().await.unwrap();
    squeezefs::uring_fs::disarm_device_latency(&path);
    assert!(
        stats(&va).flush_ceiling_overruns >= 1,
        "the parked barrier is an overrun the counter must see"
    );
    for v in &ra.volumes {
        v.shutdown().await.unwrap();
    }
}

/// Symmetric PR 13c, F-B1 (`.benchmarks/2026-09-19-sym-acceptance.md`
/// §3.9.2): on the box `appender_flush_ceiling_overruns` tripped four
/// times in 12 minutes, 1–32 ms past the 1,100 ms landing ceiling, with
/// no recovery in flight — the manager's grant / release / ship SERVICE
/// holds the volume's SMO mutex 5–127 ms (PR 13 §4.5), the cadence tick
/// that covers a dirty leaf waits it out, and the ceiling's fixed 2-tick
/// margin is what it spent. ONE law now (defect 33's recovery extension
/// generalized): a leaf is judged on the time it aged with NO structural
/// hold on the mutex — the Σ of SERVICE holds (a wire appender's slot
/// grant / release, a slot transfer's adoption, a projection refresh, a
/// region's release — another actor's hold, never the pass's own wait on
/// a peer) OVERLAPPING its dirty window is excluded (a monotone Σ on the
/// node environment, stamped on the leaf at its clean → dirty transition
/// beside `dirty_since_ns`; an overlap-bounded exclusion whose over-excuse
/// is ≤ one cadence tick per hold), CAPPED at one landing ceiling
/// (`appender_flush_ceiling_service_cap_ms`, published — review round 1,
/// Issue 1c), a RECOVERY hold's overlap up to the published
/// `appender_recovery_bound_ms`; what remains past the ceiling is the
/// overrun. Each excused leaf counts on its class's gauge
/// (`appender_flush_ceiling_service_extensions` here), the excused Σ and
/// the largest exclusion are published. RED before: the leaf that aged
/// under a held mutex read `flush_ceiling_overruns == 1`.
/// Take the SMO mutex as the SERVICE would, waiting out a cadence pass
/// that holds it (the seam is try-only; the tick runs every 50 ms).
async fn hold_smo_as_service(
    va: &Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend>,
) -> (
    squeezefs::meta_backend::kv::backend::SmoHold<'_>,
    squeezefs::meta_backend::kv::backend::StructuralHold<'_>,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(h) = va.test_hold_smo_as_service() {
            return h;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the SMO mutex never freed between passes"
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_leaf_that_aged_under_a_service_hold_of_the_smo_mutex_is_an_extension_not_an_overrun() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let path = std::path::PathBuf::from(&uris[0]);
    let ra = open_with_partition(&uris, None).await;
    let va = Arc::clone(&ra.volumes[0]);
    ra.create(ROOT_INO, "warm", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    va.checkpoint_now().await.unwrap();
    let s0 = stats(&va);
    assert_eq!(s0.flush_ceiling_overruns, 0, "the premise");
    assert_eq!(s0.flush_ceiling_service_extensions, 0, "the premise");
    let ceiling = s0.flush_ceiling_ms;
    // The leaf goes dirty, then a SERVICE holds the mutex past the ceiling
    // (the box's shape: the tick parked behind the manager's grant service).
    ra.create(
        ROOT_INO,
        "under-a-service-hold",
        libc::S_IFREG | 0o644,
        0,
        0,
    )
    .await
    .unwrap();
    let hold = hold_smo_as_service(&va).await;
    tokio::time::sleep(std::time::Duration::from_millis(ceiling + 200)).await;
    drop(hold);
    va.checkpoint_now().await.unwrap();
    let s1 = stats(&va);
    assert_eq!(
        s1.flush_ceiling_overruns, 0,
        "a leaf that aged under a service hold is not an overrun (ceiling {ceiling} ms)"
    );
    assert!(
        s1.flush_ceiling_service_extensions >= 1,
        "the extension is counted on its class (got {})",
        s1.flush_ceiling_service_extensions
    );
    // The exclusion is PUBLISHED and CAPPED at one landing ceiling: this
    // hold ran 200 ms past the cap, so the Σ excused reads the cap (the
    // 200 ms + the pass are what was judged, under the ceiling).
    let cap = s1.flush_ceiling_service_cap_ms;
    assert_eq!(
        cap,
        appender_flush_ceiling_service_cap_ms(50),
        "the service cap in force is the derivation's (one landing ceiling)"
    );
    assert_eq!(cap, ceiling, "one landing ceiling");
    assert_eq!(
        s1.flush_ceiling_excused_ns,
        cap * 1_000_000,
        "the excused Σ is the capped overlap"
    );
    assert_eq!(
        s1.flush_ceiling_excused_max_ms, cap,
        "the largest exclusion is the cap"
    );
    // The exclusion is OVERLAP-bounded: a hold that ended BEFORE the leaf
    // went dirty excuses nothing — a parked device past the ceiling is
    // still the overrun the counter must see.
    let hold = hold_smo_as_service(&va).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    drop(hold);
    ra.create(ROOT_INO, "late", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let park = std::time::Duration::from_millis(ceiling + 400);
    squeezefs::uring_fs::arm_device_latency(&path, std::time::Duration::ZERO, park);
    va.checkpoint_now().await.unwrap();
    squeezefs::uring_fs::disarm_device_latency(&path);
    let s2 = stats(&va);
    assert_eq!(
        s2.flush_ceiling_overruns, 1,
        "a hold that ended before the leaf went dirty excuses nothing"
    );
    assert_eq!(
        s2.flush_ceiling_service_extensions, s1.flush_ceiling_service_extensions,
        "no extension counted for a hold outside the dirty window"
    );
    assert_eq!(
        s2.flush_ceiling_excused_ns, s1.flush_ceiling_excused_ns,
        "nothing excused for a leaf no hold overlapped"
    );
    // The exclusion is CAPPED (review round 1, Issue 1c): a service hold
    // longer than one landing ceiling excuses the cap and the EXCESS is
    // the overrun — the stall class the ceiling's consumers must see.
    // The leaf goes dirty UNDER the hold (PR 13e, F-B1): the parked
    // device above taught the cadence a term past the ceiling, so a leaf
    // dirtied before the hold is flushed inside one tick — the honest
    // response to a device that cannot land the promise, and not the
    // shape this arm judges.
    let hold = hold_smo_as_service(&va).await;
    ra.create(ROOT_INO, "under-a-long-hold", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(cap + ceiling + 300)).await;
    drop(hold);
    va.checkpoint_now().await.unwrap();
    let s3 = stats(&va);
    assert_eq!(
        s3.flush_ceiling_overruns,
        2,
        "a hold past the cap leaves its excess as the overrun (hold {} ms, cap {cap} ms, \
         ceiling {ceiling} ms)",
        cap + ceiling + 300
    );
    assert_eq!(
        s3.flush_ceiling_excused_max_ms, cap,
        "the largest exclusion is exactly the cap"
    );
    assert_eq!(
        s3.flush_ceiling_excused_ns - s2.flush_ceiling_excused_ns,
        cap * 1_000_000,
        "the long hold excused the cap and nothing more"
    );
    for v in &ra.volumes {
        v.shutdown().await.unwrap();
    }
}

/// The F-B1 fixture (the box's shape): a bit-17 forest at the shipped
/// node geometry (256 KiB nodes, an 8 MiB ring — wide enough that no
/// reserve drain forces a cycle mid-window), the ARMED plane with the
/// box's affinity order (a populated volume's `used_leaf_bytes / 64` is
/// MiBs; this 64 MiB fixture's derives to the one-extent floor, which a
/// one-leaf tree sits AT and spills to the 64-rotor — 60–68 dirty leaves
/// + 1–2 SMOs per cycle and 64 per-tree maintenance items ahead of every
/// decision), and ONE directory whose children mint into its slot by
/// affinity (one leaf per cycle) with four files already in it. Returns
/// the set, its volume, the device path and the directory.
async fn armed_one_leaf_fixture(
    dir: &std::path::Path,
) -> (
    Arc<RoutedMetaBackend>,
    Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend>,
    std::path::PathBuf,
    u64,
) {
    let p = dir.join("meta0");
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
    let r = format_v3_stamped(
        &p,
        VOL_LEN,
        &FormatV3Options {
            node_size: 256 * 1024,
            journal_len_override: Some(8 * 1024 * 1024),
            ..set_opts()
        },
        plan.stamps[0].clone(),
    )
    .await;
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    r.expect("format");
    let uris = vec![p.display().to_string()];
    let path = std::path::PathBuf::from(&uris[0]);
    let ra = common::sym::open_under(&uris, &common::sym::Knobs::armed().affinity_mb("16")).await;
    let va = Arc::clone(&ra.volumes[0]);
    assert_eq!(stats(&va).flush_ceiling_ms, appender_flush_ceiling_ms(50));
    let d = ra
        .create(ROOT_INO, "d", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    for i in 0..4u32 {
        ra.create(d, &format!("w{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
    }
    (ra, va, path, d)
}

/// The cadence's UPPER cycle bound over a window: one cycle per trigger
/// interval at the LOWEST trigger a bounded term admits, plus the two
/// warm cycles a window's edges can straddle. A trigger collapsed to 0
/// (a term the derivation read past the ceiling) runs a cycle per tick
/// and lands an order of magnitude above it.
fn max_cadence_cycles(window_ms: u64, term_ms: u64) -> u64 {
    let trigger = squeezefs::meta_backend::kv::checkpoint::checkpoint_trigger_ms(
        CHECKPOINT_MAX_AGE_MS as u64,
        term_ms,
    )
    .max(checkpoint_tick_period_ms(50));
    window_ms.div_ceil(trigger) + 2
}

/// **PR 13e review round 1, Issue 1 (F-B1's age law — the bug): an IDLE
/// forest volume's whole idle span became the next cycle's "term".** The
/// first build recorded the age decision's lateness on EVERY `due` tick —
/// the ticks that ran no cycle included (nothing dirty, the ring
/// covered) — while `checkpoint_collected_ns` never advanced, so the
/// first cycle after an idle span folded `wall + idle` into the term, the
/// 64-cycle maximum held it, the trigger saturated to 0, and every cycle's
/// own ledger record left `distance > 0` for the next tick: a
/// self-sustaining checkpoint-per-tick storm for the whole horizon after
/// EVERY idle → active transition (the reviewer reproduced it: 4 s idle →
/// 29 paced creates → 31 cycles in 1.5 s, term 2,489 ms, trigger 0 — the
/// box's "between the rows" → row shape on every writer). The law:
/// lateness is recorded ONLY by a decision that RUNS a cycle; an idle
/// `due` tick with nothing to cover ADVANCES the collection instant (an
/// empty collection is a collection — every leaf dirtied from here is
/// bounded from here); and a lateness past one landing ceiling is a stall
/// the audit counts on the cycle it happens, never a term to anticipate
/// (the belt). This contract: the fixture, a clean checkpoint, the
/// cadence given 1.5 s to cover itself, 4 s IDLE, then a paced storm for
/// 1.5 s — the cycles over the storm are bounded by the trigger and the
/// published term stays inside a tick. RED on the first build (31 cycles,
/// term 2,489 ms); GREEN on the fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_span_is_never_a_cycles_term_so_the_first_burst_after_it_runs_at_the_cadence() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let (ra, va, _path, d) = armed_one_leaf_fixture(dir.path()).await;
    va.checkpoint_now().await.unwrap();
    // The cadence covers its own ledger record (one cycle), then the
    // volume is IDLE for four seconds — every tick past the trigger is
    // `due` with nothing to cover.
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
    let idle_ms = 4_000u64;
    tokio::time::sleep(std::time::Duration::from_millis(idle_ms)).await;
    let checkpoints0 = META_KV_CHECKPOINTS.load(std::sync::atomic::Ordering::Relaxed);
    let term_before = va.checkpoint_term_ms();
    // The storm: one creator paced at a tick for 1.5 s.
    let window_ms = 1_500u64;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let creator = {
        let ra = Arc::clone(&ra);
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            let mut i = 0u32;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                ra.create(d, &format!("b{i}"), libc::S_IFREG | 0o644, 0, 0)
                    .await
                    .unwrap();
                i += 1;
                tokio::time::sleep(std::time::Duration::from_millis(checkpoint_tick_period_ms(
                    50,
                )))
                .await;
            }
            i
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(window_ms)).await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let created = creator.await.unwrap();
    assert!(created >= 20, "the storm ran ({created} creates)");
    let cycles = META_KV_CHECKPOINTS.load(std::sync::atomic::Ordering::Relaxed) - checkpoints0;
    let term = va.checkpoint_term_ms();
    let tick = checkpoint_tick_period_ms(50);
    assert!(
        term <= 2 * tick,
        "the published term is the storm's own (barrier-free device: a few ms), never the \
         {idle_ms} ms idle span before it (term {term} ms, before the storm {term_before} ms)"
    );
    let bound = max_cadence_cycles(window_ms, term);
    assert!(
        cycles <= bound,
        "the first burst after an idle span runs at the cadence: {cycles} cycles over a \
         {window_ms} ms storm against a bound of {bound} (term {term} ms, trigger {} ms) — a \
         trigger collapsed to 0 runs a cycle per tick (RED: 31 cycles in 1.5 s at a 2,489 ms \
         term)",
        va.checkpoint_trigger_ms(CHECKPOINT_MAX_AGE_MS as u64)
    );
    assert!(
        cycles >= 1,
        "the storm's leaf was checkpointed at least once ({cycles})"
    );
    assert_eq!(
        stats(&va).flush_ceiling_overruns,
        0,
        "no overrun (the storm's leaf lands inside the ceiling)"
    );
    for v in &ra.volumes {
        v.shutdown().await.unwrap();
    }
}

/// **PR 13e, F-B1 (record §3.9.4.6 / §7 item 3 — the box re-run's six
/// trips with NOTHING excused): the cadence anticipates the cycle's
/// MEASURED pre-barrier wall, so a slow barrier lands every leaf inside
/// the landing ceiling.** KD-SYM-10's ceiling is `trigger + 2 ticks`: the
/// tick wait and the maintenance drain — the cycle's own pre-barrier wall
/// (the flush pass, the bitmap pages, barrier #1; on the box 16–106 ms of
/// page writes and barriers under N regions' joins and an ingest) sat
/// OUTSIDE it, and the decision ran from the previous cycle's END (the
/// grant cadence, a growth, the merge sweep after the barrier ate the
/// margin too), so a leaf dirtied right after a collection aged
/// `trigger + ticks + wall` at its covering barrier and the audit —
/// correctly — read the wall as an overrun the exclusion of another
/// actor's hold could not touch (`excused_ns` 0). The box's shape here: the
/// shipped node size, ONE leaf (a directory's children by affinity — no
/// rotor round-robin, no SMO in the window: an SMO barriers its images, and
/// the fixture's 64 KiB / 64-rotor geometry ran 1–6 per cycle, walls past
/// the ceiling itself — a geometry × latency verdict, not a cadence's), a
/// device barrier parked at 60 % of the margin (60 ms at the shipped
/// flush; a flush pass's compaction barriers its images too, so the term
/// is 60–120 ms — the box's order, past the margin), a single creator
/// paced at one tick (the leaf dirty within a tick of every collection; a
/// STATIONARY storm — an unpaced one grows its flush passes' SMO count as
/// the leaf fills, a burst larger than every one before it, which the
/// tripwire is designed to catch), two cycles under the stream warming
/// the measurement, then five cadence intervals. RED on
/// `7f4b007e`: the covering barrier lands two walls past the trigger +
/// ticks. GREEN: the decision is taken against the LAST COLLECTION
/// instant and fires the anticipated wall early
/// (`checkpoint::checkpoint_trigger_ms` off the horizon maximum of
/// the measured TERM — the pre-barrier wall plus the decision's lateness
/// beyond one tick, the deferred-flush and maintenance barriers the tick
/// runs before it decides, its wait for the SMO mutex left out; a bound
/// anticipated by a bound, never a mean),
/// and the landing stays inside the published ceiling: 0 overruns, the
/// term and the trigger in force published per volume.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cadence_anticipates_the_measured_cycle_wall_so_a_slow_barrier_lands_inside_the_ceiling(
) {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let (ra, va, path, d) = armed_one_leaf_fixture(dir.path()).await;
    va.checkpoint_now().await.unwrap();
    // The barrier is 60 % of the 100 ms margin: a paced storm's term then
    // reads 60–120 ms (one barrier, or a compaction's second one) — the
    // box's 16–106 ms class, PAST the margin on the base cadence (RED 5/5
    // at 1,121–1,174 ms) and inside the anticipation on the fix. A 25 ms
    // barrier's 26 ms term sits INSIDE the margin on the base too and
    // pins nothing.
    let margin_ms = 2 * checkpoint_tick_period_ms(50);
    let barrier = std::time::Duration::from_millis(margin_ms * 3 / 5);
    squeezefs::uring_fs::arm_device_latency(&path, std::time::Duration::ZERO, barrier);
    // The stream: ONE creator PACED at a tick — the leaf is dirty within
    // one tick of every collection (the worst leaf the ceiling bounds),
    // and the storm is STATIONARY: an unpaced creator fills the leaf's log
    // faster every second and its flush passes run 1 → 2 → 3 SMOs, a burst
    // larger than every one before it, which the tripwire is DESIGNED to
    // catch (the derivation anticipates the measured term, never a growth
    // it has not seen).
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let creator = {
        let ra = Arc::clone(&ra);
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            let mut i = 0u32;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                ra.create(d, &format!("s{i}"), libc::S_IFREG | 0o644, 0, 0)
                    .await
                    .unwrap();
                i += 1;
                tokio::time::sleep(std::time::Duration::from_millis(checkpoint_tick_period_ms(
                    50,
                )))
                .await;
            }
            i
        })
    };
    // Two cycles under the stream warm the measurement (the coalesced
    // wall is what the window learns; the decision's lateness joins it
    // from the first cadence cycle).
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    va.checkpoint_now().await.unwrap();
    va.checkpoint_now().await.unwrap();
    let overruns0 = stats(&va).flush_ceiling_overruns;
    let checkpoints0 = META_KV_CHECKPOINTS.load(std::sync::atomic::Ordering::Relaxed);
    let window = std::time::Duration::from_millis(5 * CHECKPOINT_MAX_AGE_MS as u64 + 500);
    tokio::time::sleep(window).await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let created = creator.await.unwrap();
    assert!(created >= 40, "the stream ran ({created} creates)");
    squeezefs::uring_fs::disarm_device_latency(&path);
    let s = stats(&va);
    let cycles = META_KV_CHECKPOINTS.load(std::sync::atomic::Ordering::Relaxed) - checkpoints0;
    assert!(
        cycles >= 4,
        "the cadence ran through the window ({cycles} checkpoints)"
    );
    // The UPPER bound (review round 1, Issue 1): a trigger collapsed to 0
    // — a term the derivation read past the ceiling — passes the lower
    // bound and the overrun count both while running a cycle per tick.
    let bound = max_cadence_cycles(window.as_millis() as u64, va.checkpoint_term_ms());
    assert!(
        cycles <= bound,
        "the cadence ran at its trigger, never a cycle per tick ({cycles} cycles over {} ms, \
         bound {bound}, term {} ms)",
        window.as_millis(),
        va.checkpoint_term_ms()
    );
    assert_eq!(
        s.flush_ceiling_overruns - overruns0,
        0,
        "a {} ms barrier must land inside the {} ms ceiling — the cadence anticipates the \
         wall it measured (RED: {} overrun(s) in {cycles} cycles, the wall outside the 2-tick \
         margin, nothing excused: the box's F-B1)",
        barrier.as_millis(),
        s.flush_ceiling_ms,
        s.flush_ceiling_overruns - overruns0
    );
    // The derivation's published faces: the anticipated term carries the
    // parked barrier (every cycle's barrier #1 waits it), and the trigger
    // in force is the ceiling less that term (`checkpoint_trigger_ms`).
    let term = va.checkpoint_term_ms();
    assert!(
        term >= barrier.as_millis() as u64,
        "the anticipated cycle term carries the parked barrier ({term} ms)"
    );
    assert_eq!(
        va.checkpoint_trigger_ms(CHECKPOINT_MAX_AGE_MS as u64),
        squeezefs::meta_backend::kv::checkpoint::checkpoint_trigger_ms(
            CHECKPOINT_MAX_AGE_MS as u64,
            term
        ),
        "the trigger in force is the derivation's"
    );
    assert!(
        va.checkpoint_trigger_ms(CHECKPOINT_MAX_AGE_MS as u64)
            <= CHECKPOINT_MAX_AGE_MS as u64 - barrier.as_millis() as u64,
        "the cadence fires the wall early"
    );
    for v in &ra.volumes {
        v.shutdown().await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// Review round 1 — the three landed-behaviour bugs, pinned red first.
// ---------------------------------------------------------------------------

/// Set an env var for a scope; restore its prior value on drop (a panic
/// mid-contract must not leave the cadence parked for the next one).
struct EnvVarGuard {
    key: &'static str,
    prior: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, val: &str) -> Self {
        let prior = std::env::var(key).ok();
        std::env::set_var(key, val);
        Self { key, prior }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.prior {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

/// Disarm every `uring_fs` fault on drop (a panicked contract must not
/// leave a sector error armed for the next one).
struct FaultGuard;
impl Drop for FaultGuard {
    fn drop(&mut self) {
        squeezefs::uring_fs::clear_faults();
    }
}

/// Review round 1, Issue 1 (bug — the headline mechanism): a clean
/// unmount writes the region's `Free` page into the DIRECTORY pair; the
/// join that follows attaches a FRESH ring, and its `Live` page must be
/// discoverable from the directory pair ALONE — `read_directory` reaches
/// a ring-side slot only through a directory image that names that ring.
/// The four-slot rotation put the join's page into R0/R1 of the new ring
/// for two of the four generation residues, so a crash before the
/// next-but-one checkpoint read the region as `Free`: the ring abandoned,
/// its acked in-window records lost. Every residue is walked (k extra
/// checkpoints before the leave shift the join's generation by k).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_region_rejoined_after_a_clean_unmount_is_found_live_after_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    // The cadence parked: the join's checkpoint is the ONLY page write
    // between the rejoin and the crash — the exposure window as it is.
    let _cadence = EnvVarGuard::set("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let tag = volume_tag("vol-0011223344556677");
    let guest_owner = guest_local_ino(3, 21);
    for k in 0..4u32 {
        let uris = vec![format_stamped_member(dir.path(), &format!("k{k}")).await];
        let path = std::path::Path::new(&uris[0]);
        let sb = superblock_of_path(path).await;
        // Mount 1: join, k extra cycles, a clean unmount (the leave).
        let ra = open_with_partition(&uris, Some(PARTITION)).await;
        for _ in 0..k {
            ra.volumes[0].checkpoint_now().await.unwrap();
        }
        for v in &ra.volumes {
            v.shutdown().await.unwrap();
        }
        drop(ra);
        let left = read_directory(path, &sb).await.unwrap()[1]
            .page
            .clone()
            .expect("the leave wrote appender 1's Free page");
        assert_eq!(left.state, AppenderState::Free, "k={k}");
        // Mount 2: the rejoin (a fresh ring), one acked commit into the
        // region, then death before any further checkpoint.
        let rb = open_with_partition(&uris, Some(PARTITION)).await;
        let vb = Arc::clone(&rb.volumes[0]);
        vb.commit_block_refs(guest_owner, &refs(tag, guest_owner, 500, 2))
            .await
            .unwrap();
        vb.sync_device().await.unwrap();
        let live = digest_backend(&vb).await.unwrap();
        let joined = read_directory(path, &sb).await.unwrap()[1]
            .page
            .clone()
            .expect("appender 1 has a page");
        assert_eq!(
            joined.state,
            AppenderState::Live,
            "k={k}: the rejoined region's Live page is reachable from the directory (the leave \
             left generation {}, the join wrote generation {})",
            left.generation,
            joined.generation
        );
        drop(vb);
        drop(rb);
        // Mount 3: both regions are our own residue; the acked refs are
        // there.
        let rc = open_with_partition(&uris, Some(PARTITION)).await;
        let vc = &rc.volumes[0];
        let s = stats(vc);
        assert_eq!(
            s.self_recoveries, 2,
            "k={k}: both Live pages recovered (the leave left generation {}): {s:?}",
            left.generation
        );
        assert_eq!(vc.block_ref_count(tag, 500).await.unwrap(), 1, "k={k}");
        assert_eq!(vc.block_ref_count(tag, 501).await.unwrap(), 1, "k={k}");
        assert_eq!(digest_backend(vc).await.unwrap(), live, "k={k}");
        for v in &rc.volumes {
            v.shutdown().await.unwrap();
        }
    }
}

/// Review round 1, Issue 2 (bug): the leave released a declared region's
/// ring extents with a RAM-only allocator op AFTER the final checkpoint
/// had written the bitmap, and the process then exited — so every clean
/// unmount of a partitioned volume leaked the ring (the next mount reads
/// the bitmap, not a reachability census). The heap's free count must
/// come back to the unpartitioned baseline after every clean cycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clean_unmount_returns_a_declared_regions_ring_extents_to_the_heap() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let path = std::path::Path::new(&uris[0]);
    // The baseline: the heap as an unpartitioned mount leaves it.
    let r0 = open_with_partition(&uris, None).await;
    for v in &r0.volumes {
        v.shutdown().await.unwrap();
    }
    drop(r0);
    let baseline = squeezefs::meta_backend::kv::backend::KvMetaBackend::open_probe(path)
        .await
        .unwrap()
        .free_extents();
    for cycle in 0..3u32 {
        let ra = open_with_partition(&uris, Some(PARTITION)).await;
        let held = ra.volumes[0].free_extents();
        assert!(
            held < baseline,
            "cycle {cycle}: the region's ring is claimed while mounted ({held} < {baseline})"
        );
        for v in &ra.volumes {
            v.shutdown().await.unwrap();
        }
        drop(ra);
        // The durable bitmap, as the next mount reads it.
        let probe = squeezefs::meta_backend::kv::backend::KvMetaBackend::open_probe(path)
            .await
            .unwrap();
        assert_eq!(
            probe.free_extents(),
            baseline,
            "cycle {cycle}: the ring's extents returned to the heap durably (held {held} while \
             mounted)"
        );
    }
}

/// PR 3 review round 2 (found by the clean-remount grant pin's planted
/// orphan — a product defect on PR 2's ring carve): a region rejoined
/// after a clean unmount carves a FRESH ring from the heap, and the
/// heap's lowest-free-first hands it the extents the PREDECESSOR
/// incarnation's ring occupied — with that ring's entries still on the
/// device, checksummed, at lap 0 like the new ring's own. The new
/// incarnation's replay chain walks past its own head into them: a
/// predecessor's `+ref` at a higher position out-votes this mount's
/// acked release (per-key LWW by ring position — the record resurrects),
/// a predecessor's `free(extent)` is folded into the grant and RETURNED
/// (the extent's live image loses its bit). A carved ring must replay
/// exactly the entries its own incarnation wrote.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_re_carved_ring_never_replays_its_predecessor_incarnations_entries() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let _cadence = EnvVarGuard::set("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let tag = volume_tag("vol-0011223344556677");
    let guest_owner = guest_local_ino(3, 23);
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    // Mount 1: many entries into appender 1's ring (one commit = one
    // entry), durable, then a clean leave (the ring's extents return).
    let ra = open_with_partition(&uris, Some(PARTITION)).await;
    let va = Arc::clone(&ra.volumes[0]);
    for i in 0..24u64 {
        va.commit_block_refs(guest_owner, &refs(tag, guest_owner, 7000 + i * 10, 3))
            .await
            .unwrap();
    }
    va.checkpoint_now().await.unwrap();
    for v in &ra.volumes {
        v.shutdown().await.unwrap();
    }
    drop(va);
    drop(ra);
    // Mount 2: the rejoin carves a fresh ring (the predecessor's extents,
    // lowest-free-first); ONE acked entry — the RELEASE of two mount-1
    // references: the FIRST record mount 1 ever wrote (its seq is the
    // predecessor ring's position 0 — a fresh ring restarting at 0 gives
    // the release the SAME seq, the tie) and the LAST (position ≫ 0 — a
    // release at a fresh ring's low position sorts BELOW it, and per-key
    // LWW by seq resurrects the put at replay while the live RAM apply
    // read 0: the seq space of a region must be monotone across its
    // incarnations) — then death.
    let rb = open_with_partition(&uris, Some(PARTITION)).await;
    let vb = Arc::clone(&rb.volumes[0]);
    let victims: Vec<BlockRefOp> = [7000u64, 7230]
        .iter()
        .map(|b| {
            BlockRefOp::released(BlockRef {
                vol_tag: tag,
                block_idx: *b,
                owner_ino: guest_owner,
                block_index: 0,
            })
        })
        .collect();
    assert_eq!(vb.block_ref_count(tag, 7000).await.unwrap(), 1);
    assert_eq!(vb.block_ref_count(tag, 7230).await.unwrap(), 1);
    vb.commit_block_refs(guest_owner, &victims).await.unwrap();
    vb.sync_device().await.unwrap();
    assert_eq!(vb.block_ref_count(tag, 7000).await.unwrap(), 0);
    assert_eq!(vb.block_ref_count(tag, 7230).await.unwrap(), 0);
    let live = digest_backend(&vb).await.unwrap();
    let free_live = vb.free_extents();
    drop(vb);
    drop(rb);
    // Mount 3: the releases stand; the predecessor's entries are gone.
    let rc = open_with_partition(&uris, Some(PARTITION)).await;
    let vc = &rc.volumes[0];
    let s = stats(vc);
    assert_eq!(
        s.self_recoveries, 2,
        "both regions are our own residue at the crash remount: {s:?}"
    );
    for b in [7000u64, 7230] {
        assert_eq!(
            vc.block_ref_count(tag, b).await.unwrap(),
            0,
            "block {b}: the acked release survives the crash — a predecessor incarnation's \
             `+ref`, at a higher ring position or at the same seq, must never out-vote it"
        );
    }
    assert_eq!(digest_backend(vc).await.unwrap(), live);
    assert_eq!(
        vc.free_extents(),
        free_live,
        "no predecessor `free` was folded into the grant and returned"
    );
    for v in &rc.volumes {
        v.shutdown().await.unwrap();
    }
}

/// Review round 1, Issue 3 (bug): the §4.4 pt 4 rollback of a FAILED
/// window write journals its compensating records through the
/// checkpoint-class reserve — of the ring the failed window used, never
/// ring 0. Compensation is CONTENT of the window's region (KD-SYM-4: one
/// ring per key); in the manager's ring it is the `Lease` violation the
/// next mount refuses on. The phase-2 arm is forced exactly: the doomed
/// write is HELD, a checkpoint flushes its applied records into a durable
/// bset (the "freeze raced the failed write" shape), then the held write
/// fails on release.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_window_in_a_declared_region_compensates_into_its_own_ring() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let _faults = FaultGuard;
    let _cadence = EnvVarGuard::set("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let path = std::path::PathBuf::from(&uris[0]);
    let tag = volume_tag("vol-0011223344556677");
    let guest_owner = guest_local_ino(3, 31);
    let ra = open_with_partition(&uris, Some(PARTITION)).await;
    let va = Arc::clone(&ra.volumes[0]);
    // The leaf exists and is flushed before the doomed write.
    va.commit_block_refs(guest_owner, &refs(tag, guest_owner, 0, 2))
        .await
        .unwrap();
    va.checkpoint_now().await.unwrap();
    let ring0 = va.journal_ring();
    let ring1 = va.ring_of_region(1);
    let (e0, e1) = (ring0.written_entries(), ring1.written_entries());
    let phys = ring1.physical_offset_of(ring1.core().head());
    let mut arrived = squeezefs::uring_fs::arm_write_stall(&path, phys, 8);
    let doomed = {
        let v = Arc::clone(&va);
        tokio::spawn(async move {
            v.commit_block_refs(guest_owner, &refs(tag, guest_owner, 100, 2))
                .await
        })
    };
    arrived
        .recv()
        .await
        .expect("the doomed write reached the device shim");
    // Applied at stage A; its records now leave the open overlay for a
    // durable bset while the write is still in flight.
    va.checkpoint_now().await.unwrap();
    squeezefs::uring_fs::arm_sector_write_error(phys);
    squeezefs::uring_fs::release_write_stall(&path);
    let out = doomed.await.unwrap();
    assert!(out.is_err(), "the failed window fails its member: {out:?}");
    squeezefs::uring_fs::clear_faults();
    assert_eq!(
        va.block_ref_count(tag, 100).await.unwrap(),
        0,
        "rolled back in RAM"
    );
    assert_eq!(
        ring0.written_entries(),
        e0,
        "no compensating entry rode the MANAGER's ring for appender 1's content"
    );
    assert_eq!(
        ring1.written_entries(),
        e1 + 1,
        "the compensating entry rode appender 1's OWN ring"
    );
    // The remount is clean: no partition violation, the rolled-back refs
    // absent, the earlier ones present.
    let before = META_KV_REPLAY_LEASE_VIOLATIONS.load(Ordering::Relaxed);
    let live = digest_backend(&va).await.unwrap();
    drop(va);
    drop(ra);
    let rb = open_with_partition(&uris, Some(PARTITION)).await;
    let vb = &rb.volumes[0];
    assert_eq!(
        META_KV_REPLAY_LEASE_VIOLATIONS.load(Ordering::Relaxed),
        before
    );
    assert_eq!(META_KV_REPLAY_KEY_VIOLATIONS.load(Ordering::Relaxed), 0);
    assert_eq!(vb.block_ref_count(tag, 100).await.unwrap(), 0);
    assert_eq!(vb.block_ref_count(tag, 0).await.unwrap(), 1);
    assert_eq!(digest_backend(vb).await.unwrap(), live);
    for v in &rb.volumes {
        v.shutdown().await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// Review round 2 — Issues 22, 18, 23, 24, pinned red first.
// ---------------------------------------------------------------------------

/// Review round 2, Issue 22 (bug): the audit compared the leaf's age
/// against the cadence TRIGGER (1,000 ms) — but the tick makes a cycle due
/// AT the trigger, so a leaf dirtied ε after a checkpoint is `1000 + pass −
/// ε` old at its covering barrier and the must-stay-0 gauge fired on a
/// healthy stamped mount under any steady stream at the DEFAULT cadence
/// (the reviewer read 3 overruns in 3.5 s, ages 1,013–1,060 ms). The
/// ceiling is the LANDING one (`checkpoint_landing_ceiling_ms`); a steady
/// stream into BOTH regions for ≥ 5 s at the default cadence moves the
/// gauge by nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steady_write_stream_at_the_default_cadence_never_overruns_the_flush_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    // The DEFAULT cadence — the shipped shape the tripwire must be silent on.
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag("vol-0011223344556677");
    // Region 0 through the native slot's owner, region 1 through guest
    // slot 3's (the two-appender contract's shape). A routed CREATE is not
    // the vehicle under the declared partition: the mint spread lands ~1
    // in 64 child inos in the declared slot, and a tx spanning the parent's
    // slot and the child's is the §5.6 cross-owner refusal as the seam
    // models it (PR 6's intents) — the seam's known shape, not this
    // contract's subject.
    let manager_owner = 1000u64;
    let guest_owner = guest_local_ino(3, 41);
    let ra = open_with_partition(&uris, Some(PARTITION)).await;
    let va = Arc::clone(&ra.volumes[0]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut i = 0u64;
    while std::time::Instant::now() < deadline {
        va.commit_block_refs(manager_owner, &refs(tag, manager_owner, i * 4, 2))
            .await
            .unwrap();
        va.commit_block_refs(guest_owner, &refs(tag, guest_owner, i * 4, 2))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        i += 1;
    }
    let s = stats(&va);
    assert!(i >= 500, "the stream ran: {i} rounds");
    assert!(s.regions[0].ring_entries > 0 && s.regions[1].ring_entries > 0);
    assert_eq!(
        s.flush_ceiling_overruns, 0,
        "a healthy stamped mount at the default cadence never overruns the flush ceiling \
         ({} ms in force): {s:?}",
        s.flush_ceiling_ms
    );
    for v in &ra.volumes {
        v.shutdown().await.unwrap();
    }
}

/// Review round 2, Issue 18 (nit, reproduced): the leave's bitmap write
/// consumed a checkpoint seq that no ledger record carried, and the mount
/// resumed `checkpoint_seq` from the ledger alone — so the next mount's
/// first bitmap write TIED the leave's copy and took DUR-4's loud raise
/// ("a checkpoint retry after a failed cycle") on every clean partitioned
/// unmount → remount. The mount resumes above `max(ledger.seq, the
/// bitmap's newest page generation)` — what DUR-4's own doc always said —
/// and the raise stays the failed-cycle signal it was written for (its
/// tripwire `meta_kv_bitmap_generation_raises` reads 0 across the cycles).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_partitioned_remounts_never_take_the_bitmap_generation_raise() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag("vol-0011223344556677");
    let guest_owner = guest_local_ino(3, 51);
    let before = META_KV_BITMAP_GENERATION_RAISES.load(Ordering::Relaxed);
    for cycle in 0..3u64 {
        let ra = open_with_partition(&uris, Some(PARTITION)).await;
        let va = Arc::clone(&ra.volumes[0]);
        va.commit_block_refs(guest_owner, &refs(tag, guest_owner, cycle * 10, 2))
            .await
            .unwrap();
        va.checkpoint_now().await.unwrap();
        drop(va);
        for v in &ra.volumes {
            v.shutdown().await.unwrap();
        }
        drop(ra);
        assert_eq!(
            META_KV_BITMAP_GENERATION_RAISES.load(Ordering::Relaxed),
            before,
            "cycle {cycle}: a clean partitioned unmount → remount is not a failed-cycle retry \
             — the bitmap generation is never raised"
        );
    }
}

/// Review round 2, Issue 23 (suggestion): the Dekker pair guards the PASS
/// (stage A); the durability lane (stage B) holds only the ring `Arc` it
/// was handed. Between a failed window's `complete` (its reservation
/// closed — `min_inflight_start` clear) and its §4.4 pt 4 compensation,
/// growth's `drained` predicate could hold and swap the ring, and the
/// compensation would then reserve on the OLD ring object — a second core
/// over the same extents, the compensating record lost at replay. A
/// stage-B window counts against growth from its creation to its terminal
/// outcome (`windows_inflight`), so growth never swaps a ring a window
/// still names. The exact schedule is built: the doomed write held, its
/// records frozen, the write failed, the lane PARKED before the rollback
/// (`TEST_CONVEYOR_HOLD_PRE_ROLLBACK`) while two barriered cycles find the
/// ring drained with a stall on record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn growth_never_swaps_a_ring_with_a_stage_b_window_in_flight() {
    struct HoldGuard;
    impl Drop for HoldGuard {
        fn drop(&mut self) {
            TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
            test_conveyor_hold_release();
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let _faults = FaultGuard;
    let _hold = HoldGuard;
    let _cadence = EnvVarGuard::set("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let _ring = EnvVarGuard::set("SQUEEZEFS_SYM_RING_KB", "512");
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let path = std::path::PathBuf::from(&uris[0]);
    let tag = volume_tag("vol-0011223344556677");
    let guest_owner = guest_local_ino(3, 61);
    let ra = open_with_partition(&uris, Some(PARTITION)).await;
    let va = Arc::clone(&ra.volumes[0]);
    assert_eq!(stats(&va).regions[1].ring_bytes, 512 * 1024);
    // A stall on record (the growth contract's storm; the test is the
    // only drain), the ring NOT yet grown.
    let committer = {
        let v = Arc::clone(&va);
        tokio::spawn(async move {
            for i in 0..40u64 {
                v.commit_block_refs(guest_owner, &refs(tag, guest_owner, i * 1000, 500))
                    .await
                    .unwrap();
            }
        })
    };
    let stall_deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while stats(&va).regions[1].stalls == 0 {
        assert!(!committer.is_finished() && std::time::Instant::now() < stall_deadline);
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    while !committer.is_finished() {
        va.checkpoint_now().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    committer.await.unwrap();
    let s = stats(&va);
    assert!(s.regions[1].stalls > 0);
    assert_eq!(s.ring_grows, 0, "not grown yet: {:?}", s.regions[1]);
    // The doomed write: held at the device, its stage-A records frozen
    // into a bset by a checkpoint, then failed on release with the lane
    // parked BEFORE its rollback — a stage-B window in flight, its
    // reservation completed, its compensation not yet issued.
    let ring1 = va.ring_of_region(1);
    let phys = ring1.physical_offset_of(ring1.core().head());
    let mut arrived = squeezefs::uring_fs::arm_write_stall(&path, phys, 8);
    let doomed = {
        let v = Arc::clone(&va);
        tokio::spawn(async move {
            v.commit_block_refs(guest_owner, &refs(tag, guest_owner, 90_000, 2))
                .await
        })
    };
    arrived
        .recv()
        .await
        .expect("the doomed write reached the device shim");
    va.checkpoint_now().await.unwrap();
    TEST_CONVEYOR_HOLD_STAGE.store(TEST_CONVEYOR_HOLD_PRE_ROLLBACK, Ordering::SeqCst);
    squeezefs::uring_fs::arm_sector_write_error(phys);
    squeezefs::uring_fs::release_write_stall(&path);
    // The lane parks pre-rollback once the failed write's outcome is known.
    let park_deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while squeezefs::meta_backend::kv::backend::test_conveyor_hold_parked() == 0 {
        assert!(
            std::time::Instant::now() < park_deadline,
            "the lane never parked"
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    // Two barriered cycles: the ring drains (the failed tx's leaves are
    // flushed, the hole's tail lands) with the stall on record — the
    // growth decision at each cycle's end must DECLINE while the window
    // is in flight.
    va.checkpoint_now().await.unwrap();
    va.checkpoint_now().await.unwrap();
    let s = stats(&va);
    assert_eq!(
        s.ring_grows, 0,
        "growth never swaps a ring a stage-B window still names: {:?}",
        s.regions[1]
    );
    // Release: the rollback compensates into the ring it holds — the
    // CURRENT one — and the member fails.
    TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
    test_conveyor_hold_release();
    let out = doomed.await.unwrap();
    assert!(out.is_err(), "the failed window fails its member: {out:?}");
    squeezefs::uring_fs::clear_faults();
    // With the window settled, growth proceeds on the next drained cycle.
    va.checkpoint_now().await.unwrap();
    va.checkpoint_now().await.unwrap();
    let s = stats(&va);
    assert!(s.ring_grows >= 1, "{:?}", s.regions[1]);
    assert_eq!(va.block_ref_count(tag, 90_000).await.unwrap(), 0);
    assert_eq!(va.block_ref_count(tag, 17_007).await.unwrap(), 1);
    let live = digest_backend(&va).await.unwrap();
    drop(va);
    drop(ra);
    let rb = open_with_partition(&uris, Some(PARTITION)).await;
    assert_eq!(digest_backend(&rb.volumes[0]).await.unwrap(), live);
    assert_eq!(rb.volumes[0].block_ref_count(tag, 90_000).await.unwrap(), 0);
    for v in &rb.volumes {
        v.shutdown().await.unwrap();
    }
}

/// Review round 2, Issue 24 (nit): the leaf-age stamp is a forest-only
/// instrument, so a node that carries no forest-slot stamp — every node of
/// a bit-17-absent volume — takes no clock read at its clean → dirty
/// transition: the flat path pays nothing (nothing before PR 14 changes a
/// flat mount).
#[tokio::test]
async fn the_dirty_since_stamp_is_taken_only_on_forest_slot_nodes() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let node_size = MIN_NODE_SIZE;
    file.as_file().set_len(4 * node_size as u64).unwrap();
    let cache = NodeCache::new(NodeCacheConfig {
        path: file.path().to_path_buf(),
        layout: NodeLayout::new(node_size).unwrap(),
        heap_base: 0,
        budget_bytes: 4 * node_size as u64,
        writeback_delta_bytes: DEFAULT_WRITEBACK_DELTA_BYTES,
    });
    let mut nodes = Vec::new();
    for e in 0..2u64 {
        let addr = cache.extent_addr(e);
        write_node(
            cache.config().path.clone(),
            &cache.config().layout,
            &NodeWriteParams {
                node_addr: addr,
                node_seq: e + 1,
                tree_id: TREE_INODES,
                level: 0,
                min_key: b"",
                max_key: &[0xff; 8],
            },
            &[],
            0,
        )
        .await
        .unwrap();
        nodes.push(cache.load(addr).await.unwrap().unwrap());
    }
    let rec = || {
        vec![OwnedRec::new(
            bytes::Bytes::from_static(b"k"),
            1,
            RecordKind::Put,
            bytes::Bytes::from_static(b"v"),
        )]
    };
    // A flat node: dirty, unstamped.
    {
        let mut g = nodes[0].lock().write().await;
        nodes[0].apply_locked(&mut g, rec(), 1).unwrap();
    }
    assert_ne!(nodes[0].dirty_floor(), u64::MAX);
    assert_eq!(
        nodes[0].dirty_since_ns(),
        0,
        "a node of no forest slot takes no age stamp — the flat path's clean → dirty \
         transition reads no clock"
    );
    // A slot-tree node: dirty, stamped.
    nodes[1].stamp_forest_slot(guest_forest_slot(3));
    {
        let mut g = nodes[1].lock().write().await;
        nodes[1].apply_locked(&mut g, rec(), 1).unwrap();
    }
    assert_ne!(nodes[1].dirty_floor(), u64::MAX);
    assert_ne!(
        nodes[1].dirty_since_ns(),
        0,
        "a forest-slot leaf is stamped"
    );
}
