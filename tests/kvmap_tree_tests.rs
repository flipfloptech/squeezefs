//! **The `TREE_BLOCK_MAP` KV tree — PR 1 (`feat/kvmap-tree-core`)** of the
//! PB-class file ladder (`docs/design-kvmap-block-map-tree.md`, Rev 1):
//! tree 7 + the key/value/head-sentinel codecs + incompat bit 16 + KvTx
//! staging + `get_block_mapping`/`block_map_range` — **dark infrastructure**:
//! nothing stamps the bit, no production path stages a record, and the
//! crossing/spill switch is PR 2's.
//!
//! Contracts pinned here:
//!
//! 1. **Codecs.** 12-byte BE keys sort memcmp == `(ino, index)` order
//!    (§2); v1 POINT/STRING values round-trip; the head sentinel carries
//!    the A2 sweep cursor from day one.
//! 2. **Refusals.** Index `u32::MAX` (A5 — reserved by the refs MAP_BLOB
//!    sentinel), unknown value versions/kinds (RUN is reserved for PR 6),
//!    unknown `kvmap:N` majors, and truncated buffers refuse loud
//!    (`KvError::Corrupt`-class Results) — never a panic, never a
//!    truncated guess.
//! 3. **One transaction.** Map Puts + Deletes ride the SAME `KvTx` as the
//!    layout/inode records — a publish costs the same journal entries
//!    with map records as without them (the
//!    `accounting_rides_the_publish` law, §3). And unlike block_refs
//!    (accounting the derived walk can rebuild), map records ARE the
//!    mapping: an un-engaged volume REFUSES non-empty map ops loud
//!    rather than silently dropping them.
//! 4. **Resolution.** `get_block_mapping` + `block_map_range` round-trip,
//!    absent keys answer `None`, one ino's range never bleeds into its
//!    neighbour's, and records survive a crash remount through the
//!    journal alone (§4.10 whole-tx atomicity, the block_refs precedent).
//! 5. **Compatibility.** A fresh format does NOT carry bit 16 and its
//!    sector 0 is byte-identical across a mount (PR 2 owns the stamp);
//!    stamping engages the tree on the next mount, which mints the
//!    missing root itself.

use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::block_map::{
    block_map_key, decode_block_map_key, decode_block_map_value, index_range_from,
    parse_kvmap_head, BlockMapOp, KvmapHead, MapEntry, BLOCK_MAP_KEY_LEN, BLOCK_MAP_KIND_POINT,
    BLOCK_MAP_KIND_RUN, BLOCK_MAP_KIND_STRING, BLOCK_MAP_POINT_VALUE_LEN, BLOCK_MAP_VALUE_VERSION,
};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::journal::{tag_for, untag};
use squeezefs::meta_backend::kv::record::{TREE_BLOCK_MAP, TREE_BLOCK_REFS, TREE_ID_MAX};
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, write_superblock_v3, VolumeFormat, FEATURES_INCOMPAT_KNOWN,
    FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE, MULTI_WRITER_FORMAT_BITS,
};
use squeezefs::meta_backend::kv::{
    META_KV_BLOCK_MAP_DELETES, META_KV_BLOCK_MAP_LOOKUP_EXACT, META_KV_BLOCK_MAP_LOOKUP_RANGE,
    META_KV_BLOCK_MAP_PUTS, META_KV_JOURNAL_ENTRIES,
};
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::NamedTempFile;

const META_LEN: u64 = 256 * 1024 * 1024;

fn opts() -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: None,
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// Format a v3 meta volume the way `squeezefs format` does today —
/// **without** incompat bit 16 (PR 1 stamps nothing; PR 2 owns the
/// bit-before-first-record act).
async fn format_meta(path: &std::path::Path) {
    format_v3(path, META_LEN, &opts())
        .await
        .expect("format v3 meta volume");
}

/// [`format_meta`] plus the bit-16 stamp — written OFFLINE through the
/// same superblock primitives PR 2's stamping path will use. Asserts the
/// fresh format did not already carry the bit (the D9 posture).
async fn format_meta_stamped(path: &std::path::Path) {
    format_meta(path).await;
    let VolumeFormat::V3(mut sb) = classify_volume(path).await.expect("classify") else {
        panic!("expected v3");
    };
    assert_eq!(
        sb.features_incompat & FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE,
        0,
        "a fresh format must NOT carry bit 16 — PR 2's crossing owns the stamp"
    );
    assert!(!sb.block_map_tree_stamped());
    sb.features_incompat |= FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE;
    assert!(sb.block_map_tree_stamped());
    write_superblock_v3(path, &sb).await.expect("stamp bit 16");
}

/// One mounted harness: a real v3 meta volume behind the routed layer
/// (single volume ⇒ the W ≤ 1 identity routing — global ino == local ino,
/// so the backend's map methods take the created ino verbatim).
struct Rig {
    routed: Arc<RoutedMetaBackend>,
}

async fn mount(meta: &std::path::Path) -> Rig {
    let kv = KvMetaBackend::open(meta)
        .await
        .expect("open v3 meta volume");
    Rig {
        routed: Arc::new(RoutedMetaBackend::new(vec![kv])),
    }
}

impl Rig {
    fn kv(&self) -> &Arc<KvMetaBackend> {
        &self.routed.volumes[0]
    }

    async fn mk_file(&self, name: &str) -> u64 {
        self.routed
            .create(1, name, libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .expect("create")
            .ino
    }

    async fn shutdown(self) {
        self.routed.volumes[0]
            .shutdown()
            .await
            .expect("clean shutdown");
    }
}

fn point(vol_tag: u64, offset: u64) -> MapEntry {
    MapEntry::Point { vol_tag, offset }
}

// ---------------------------------------------------------------------------
// 1. Codecs (design §2).
// ---------------------------------------------------------------------------

/// Keys are 12-byte big-endian composites: raw memcmp order equals
/// `(owner_ino, block_index)` order — what makes the per-ino prefix scans
/// exact — and both halves round-trip.
#[test]
fn key_codec_round_trips_and_sorts_memcmp() {
    let k = block_map_key(7, 3).expect("legal index");
    assert_eq!(k.len(), BLOCK_MAP_KEY_LEN);
    assert_eq!(decode_block_map_key(&k).expect("roundtrip"), (7, 3));

    // Every dimension, most significant first.
    let a = block_map_key(5, 9).unwrap();
    assert!(block_map_key(4, u32::MAX - 1).unwrap() < a);
    assert!(block_map_key(5, 8).unwrap() < a);
    assert!(a < block_map_key(5, 10).unwrap());
    assert!(a < block_map_key(6, 0).unwrap());

    // Random pairs: encoded sort == tuple sort (the BE ordering law).
    use rand::{Rng, SeedableRng};
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(42);
    let mut pairs: Vec<(u64, u32)> = (0..512)
        .map(|_| (rng.gen(), rng.gen_range(0..u32::MAX)))
        .collect();
    let mut keys: Vec<[u8; BLOCK_MAP_KEY_LEN]> = pairs
        .iter()
        .map(|&(ino, idx)| block_map_key(ino, idx).expect("legal index"))
        .collect();
    pairs.sort_unstable();
    keys.sort_unstable();
    let sorted_pairs: Vec<(u64, u32)> = keys
        .iter()
        .map(|k| decode_block_map_key(k).expect("decode"))
        .collect();
    assert_eq!(
        sorted_pairs, pairs,
        "memcmp order over encoded keys must equal (ino, index) order"
    );
}

/// v1 POINT and STRING values round-trip; the decorated-key STRING form
/// is verbatim bytes (design §2: `damaged:` markers etc.).
#[test]
fn value_codec_round_trips_point_and_string() {
    let p = point(0xDEAD_BEEF_CAFE_F00D, 42 * 4 * 1024 * 1024);
    let enc = p.encode();
    assert_eq!(enc.len(), BLOCK_MAP_POINT_VALUE_LEN);
    assert_eq!(enc[0], BLOCK_MAP_VALUE_VERSION);
    assert_eq!(enc[1], BLOCK_MAP_KIND_POINT);
    assert_eq!(decode_block_map_value(&enc).expect("point roundtrip"), p);

    let s = MapEntry::String(b"damaged:167772160".to_vec());
    let enc = s.encode();
    assert_eq!(enc[1], BLOCK_MAP_KIND_STRING);
    assert_eq!(decode_block_map_value(&enc).expect("string roundtrip"), s);

    // Verbatim law: arbitrary decorated bytes survive untouched.
    let raw = MapEntry::String(vec![0xFF, 0x00, b':', 0x7F]);
    assert_eq!(
        decode_block_map_value(&raw.encode()).expect("verbatim roundtrip"),
        raw
    );
}

/// The head sentinel round-trips both forms — `kvmap:1` and
/// `kvmap:1;sweep:K` — because A2's truncate design needs PR 1's encoding
/// to carry the sweep cursor even though the sweep itself lands later.
#[test]
fn head_sentinel_round_trips_and_carries_the_sweep_cursor() {
    let plain = KvmapHead { sweep_cursor: None };
    assert_eq!(plain.encode(), "kvmap:1");
    assert_eq!(parse_kvmap_head("kvmap:1").expect("plain head"), plain);

    let swept = KvmapHead {
        sweep_cursor: Some(4096),
    };
    assert_eq!(swept.encode(), "kvmap:1;sweep:4096");
    assert_eq!(
        parse_kvmap_head("kvmap:1;sweep:4096").expect("swept head"),
        swept
    );

    // The cursor's full u32 domain.
    let top = KvmapHead {
        sweep_cursor: Some(u32::MAX),
    };
    assert_eq!(
        parse_kvmap_head(&top.encode()).expect("u32::MAX cursor"),
        top
    );
}

// ---------------------------------------------------------------------------
// 2. Refusals (design §6 A5 + the always-forward posture) — loud typed
//    errors, never panics.
// ---------------------------------------------------------------------------

/// Index `u32::MAX` is reserved (A5 — the refs MAP_BLOB sentinel), so the
/// key encoder refuses it as a Result; malformed key buffers refuse too.
#[test]
fn reserved_index_and_malformed_keys_refuse_loud() {
    let err = block_map_key(7, u32::MAX).expect_err("reserved index");
    assert!(
        format!("{err}").contains("reserved"),
        "the refusal must name the reservation: {err}"
    );

    assert!(decode_block_map_key(&[]).is_err(), "empty");
    let k = block_map_key(7, 3).unwrap();
    assert!(
        decode_block_map_key(&k[..BLOCK_MAP_KEY_LEN - 1]).is_err(),
        "short"
    );
    let mut long = k.to_vec();
    long.push(0);
    assert!(decode_block_map_key(&long).is_err(), "long");
    // A key CARRYING the reserved index cannot have been legitimately
    // encoded — decoding one is corruption, not a mapping.
    let mut reserved = [0u8; BLOCK_MAP_KEY_LEN];
    reserved[..8].copy_from_slice(&7u64.to_be_bytes());
    reserved[8..].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(decode_block_map_key(&reserved).is_err(), "reserved in key");
}

/// Unknown value versions and kinds refuse loud — including the RUN kind,
/// whose discriminant is reserved here and decoded only from PR 6 on —
/// and truncation/trailing bytes are corruption, never a guess.
#[test]
fn unknown_value_version_kind_and_truncation_refuse_loud() {
    assert!(decode_block_map_value(&[]).is_err(), "empty");
    assert!(
        decode_block_map_value(&[BLOCK_MAP_VALUE_VERSION]).is_err(),
        "kindless"
    );
    assert!(
        decode_block_map_value(&[2, BLOCK_MAP_KIND_POINT, 0, 0]).is_err(),
        "unknown version"
    );
    assert!(
        decode_block_map_value(&[BLOCK_MAP_VALUE_VERSION, 9, 0, 0]).is_err(),
        "unknown kind"
    );
    let run = [BLOCK_MAP_VALUE_VERSION, BLOCK_MAP_KIND_RUN, 0, 0];
    let err = decode_block_map_value(&run).expect_err("RUN is PR 6's");
    assert!(
        format!("{err}").contains("run"),
        "the RUN refusal must name the reserved kind: {err}"
    );

    let p = point(1, 2).encode();
    assert!(
        decode_block_map_value(&p[..BLOCK_MAP_POINT_VALUE_LEN - 1]).is_err(),
        "truncated POINT"
    );
    let mut trailing = p.clone();
    trailing.push(0);
    assert!(decode_block_map_value(&trailing).is_err(), "trailing byte");
}

/// An unknown `kvmap:N` major and every malformed head refuse loud
/// (always-forward: a newer head grammar means a newer binary's volume).
#[test]
fn unknown_kvmap_major_and_malformed_heads_refuse_loud() {
    for bad in [
        "kvmap:2",                  // unknown major — forward-only refusal
        "kvmap:",                   // no major
        "kvmap:one",                // non-numeric major
        "kvmap:1;sweep:",           // empty cursor
        "kvmap:1;sweep:x",          // non-numeric cursor
        "kvmap:1;sweep:4294967296", // cursor past u32
        "kvmap:1;bogus:3",          // unknown segment
        "kvmap:1;sweep:5;more",     // trailing segment
        "indirect:blk-42",          // not a kvmap head at all
        "",
    ] {
        assert!(
            parse_kvmap_head(bad).is_err(),
            "head {bad:?} must refuse loud"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. One transaction (design §3 publish law).
// ---------------------------------------------------------------------------

/// Map records ride the SAME KvTx as the layout xattr + inode records: a
/// publish costs the same number of journal entries with map ops as
/// without them (the `accounting_rides_the_publish` pattern), and the
/// staging counters account for every op.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn map_records_ride_the_layout_transaction_and_add_no_commit() {
    const PUBLISHES: u32 = 8;

    // Leg A: an UN-stamped volume — the pre-kvmap entry cost.
    let meta_a = NamedTempFile::new().unwrap();
    format_meta(meta_a.path()).await;
    let rig = mount(meta_a.path()).await;
    let ino = rig.mk_file("entries_plain").await;
    let before = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    for b in 0..PUBLISHES {
        rig.kv()
            .set_layout_and_size(ino, b"layout-bytes", (b as u64 + 1) * 4096, &[])
            .await
            .expect("plain publish");
    }
    let plain_entries = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed) - before;
    rig.shutdown().await;

    // Leg B: the same op sequence WITH map records riding each tx —
    // two Puts and one Delete per publish.
    let meta_b = NamedTempFile::new().unwrap();
    format_meta_stamped(meta_b.path()).await;
    let rig = mount(meta_b.path()).await;
    assert!(rig.kv().block_map_tree_engaged());
    let ino = rig.mk_file("entries_mapped").await;
    let before = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    let puts_before = META_KV_BLOCK_MAP_PUTS.load(Ordering::Relaxed);
    let dels_before = META_KV_BLOCK_MAP_DELETES.load(Ordering::Relaxed);
    for b in 0..PUBLISHES {
        let ops = [
            BlockMapOp::Put {
                owner_ino: ino,
                block_index: b,
                entry: point(0xA1, (b as u64) * 4 * 1024 * 1024),
            },
            BlockMapOp::Put {
                owner_ino: ino,
                block_index: 1000 + b,
                entry: MapEntry::String(b"damaged:blk".to_vec()),
            },
            BlockMapOp::Delete {
                owner_ino: ino,
                block_index: 500 + b,
            },
        ];
        rig.kv()
            .set_layout_and_size_with_map(ino, b"layout-bytes", (b as u64 + 1) * 4096, &[], &ops)
            .await
            .expect("mapped publish");
    }
    let mapped_entries = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed) - before;
    assert_eq!(
        META_KV_BLOCK_MAP_PUTS.load(Ordering::Relaxed) - puts_before,
        2 * PUBLISHES as u64,
        "two map Puts staged per publish (the engagement instrument)"
    );
    assert_eq!(
        META_KV_BLOCK_MAP_DELETES.load(Ordering::Relaxed) - dels_before,
        PUBLISHES as u64,
        "one map Delete staged per publish"
    );
    assert_eq!(
        mapped_entries,
        plain_entries,
        "map records must ride the layout commit — {} extra journal entr(ies) \
         would re-split the write-commit-economy collapse",
        mapped_entries as i64 - plain_entries as i64
    );

    // …and what rode the tx resolves.
    assert_eq!(
        rig.kv().get_block_mapping(ino, 0).await.expect("lookup"),
        Some(point(0xA1, 0))
    );
    rig.shutdown().await;
}

/// Map records ARE the mapping — unlike block_refs (accounting the
/// derived walk can rebuild), silently dropping them on an un-engaged
/// volume would lose data. Non-empty ops on an un-stamped volume refuse
/// LOUD, naming the bit; empty ops keep the plain-publish behavior.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn map_ops_on_an_unstamped_volume_refuse_loud_never_silently_drop() {
    let meta = NamedTempFile::new().unwrap();
    format_meta(meta.path()).await;
    let rig = mount(meta.path()).await;
    assert!(!rig.kv().block_map_tree_engaged());
    let ino = rig.mk_file("unstamped").await;

    // Empty ops: exactly the plain publish (the delegation identity).
    rig.kv()
        .set_layout_and_size_with_map(ino, b"layout-bytes", 4096, &[], &[])
        .await
        .expect("empty map ops behave as the plain publish");

    let ops = [BlockMapOp::Put {
        owner_ino: ino,
        block_index: 0,
        entry: point(1, 0),
    }];
    let err = rig
        .kv()
        .set_layout_and_size_with_map(ino, b"layout-bytes", 4096, &[], &ops)
        .await
        .expect_err("non-empty map ops on an un-stamped volume must refuse");
    let msg = format!("{err}");
    assert!(
        msg.contains("16") || msg.contains("KV_BLOCK_MAP_TREE"),
        "the refusal must name incompat bit 16: {msg}"
    );

    // Nothing resolved, nothing engaged, nothing dropped silently.
    assert_eq!(
        rig.kv().get_block_mapping(ino, 0).await.expect("lookup"),
        None,
        "an un-engaged volume answers None, never a fabricated mapping"
    );
    assert!(rig
        .kv()
        .block_map_range(ino, 0, 16)
        .await
        .expect("range")
        .is_empty());
    rig.shutdown().await;
}

// ---------------------------------------------------------------------------
// 4. Resolution (design §3 read law, PR 1's exact-key half).
// ---------------------------------------------------------------------------

/// `get_block_mapping` + `block_map_range` round-trip: exact hits, absent
/// keys, index order, `max` bounding, Deletes, and cross-ino isolation —
/// ino N's range never bleeds into N+1's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_block_mapping_and_range_round_trip_with_cross_ino_isolation() {
    let meta = NamedTempFile::new().unwrap();
    format_meta_stamped(meta.path()).await;
    let rig = mount(meta.path()).await;
    let ino_a = rig.mk_file("a").await;
    let ino_b = rig.mk_file("b").await;

    let a0 = point(0xA1, 0);
    let a5 = MapEntry::String(b"damaged:blk-5".to_vec());
    let a7 = point(0xA1, 7 * 4 * 1024 * 1024);
    let b0 = point(0xB2, 0);
    let b3 = point(0xB2, 3 * 4 * 1024 * 1024);
    rig.kv()
        .set_layout_and_size_with_map(
            ino_a,
            b"layout-a",
            8 * 4 * 1024 * 1024,
            &[],
            &[
                BlockMapOp::Put {
                    owner_ino: ino_a,
                    block_index: 0,
                    entry: a0.clone(),
                },
                BlockMapOp::Put {
                    owner_ino: ino_a,
                    block_index: 5,
                    entry: a5.clone(),
                },
                BlockMapOp::Put {
                    owner_ino: ino_a,
                    block_index: 7,
                    entry: a7.clone(),
                },
            ],
        )
        .await
        .expect("publish a");
    rig.kv()
        .set_layout_and_size_with_map(
            ino_b,
            b"layout-b",
            4 * 4 * 1024 * 1024,
            &[],
            &[
                BlockMapOp::Put {
                    owner_ino: ino_b,
                    block_index: 0,
                    entry: b0.clone(),
                },
                BlockMapOp::Put {
                    owner_ino: ino_b,
                    block_index: 3,
                    entry: b3.clone(),
                },
            ],
        )
        .await
        .expect("publish b");

    // Exact hits and absent keys (PR 3 split the merged PR-1 counter:
    // exact lookups and range reads attribute separately).
    let exact_before = META_KV_BLOCK_MAP_LOOKUP_EXACT.load(Ordering::Relaxed);
    let range_before = META_KV_BLOCK_MAP_LOOKUP_RANGE.load(Ordering::Relaxed);
    assert_eq!(
        rig.kv().get_block_mapping(ino_a, 0).await.unwrap(),
        Some(a0.clone())
    );
    assert_eq!(
        rig.kv().get_block_mapping(ino_a, 5).await.unwrap(),
        Some(a5.clone())
    );
    assert_eq!(
        rig.kv().get_block_mapping(ino_a, 7).await.unwrap(),
        Some(a7.clone())
    );
    assert_eq!(rig.kv().get_block_mapping(ino_a, 1).await.unwrap(), None);
    assert_eq!(
        rig.kv().get_block_mapping(ino_b, 3).await.unwrap(),
        Some(b3.clone())
    );
    assert_eq!(
        rig.kv().get_block_mapping(ino_b, 5).await.unwrap(),
        None,
        "ino b must not see ino a's index-5 mapping"
    );
    assert!(
        META_KV_BLOCK_MAP_LOOKUP_EXACT.load(Ordering::Relaxed) > exact_before,
        "the exact-lookup counter must account for the resolution traffic"
    );
    assert_eq!(
        META_KV_BLOCK_MAP_LOOKUP_RANGE.load(Ordering::Relaxed),
        range_before,
        "exact lookups never count as range reads (the §8 #5 split)"
    );
    // The reserved index refuses at the lookup seam too (A5).
    assert!(rig.kv().get_block_mapping(ino_a, u32::MAX).await.is_err());

    // Ranges: exact membership, index order, from-cursor resume, max cap.
    let all_a = rig.kv().block_map_range(ino_a, 0, 100).await.unwrap();
    assert_eq!(
        all_a,
        vec![(0, a0.clone()), (5, a5.clone()), (7, a7.clone())],
        "ino a's range must be exactly its records, in index order"
    );
    assert_eq!(
        rig.kv().block_map_range(ino_a, 1, 100).await.unwrap(),
        vec![(5, a5.clone()), (7, a7.clone())]
    );
    assert_eq!(
        rig.kv().block_map_range(ino_a, 6, 100).await.unwrap(),
        vec![(7, a7.clone())]
    );
    assert_eq!(
        rig.kv().block_map_range(ino_a, 0, 2).await.unwrap().len(),
        2,
        "max bounds the page"
    );
    assert_eq!(
        rig.kv().block_map_range(ino_b, 0, 100).await.unwrap(),
        vec![(0, b0.clone()), (3, b3.clone())],
        "ino b's range must not bleed into ino a's records"
    );

    // A Delete shadows: index 5 disappears from both read paths.
    rig.kv()
        .set_layout_and_size_with_map(
            ino_a,
            b"layout-a",
            8 * 4 * 1024 * 1024,
            &[],
            &[BlockMapOp::Delete {
                owner_ino: ino_a,
                block_index: 5,
            }],
        )
        .await
        .expect("delete index 5");
    assert_eq!(rig.kv().get_block_mapping(ino_a, 5).await.unwrap(), None);
    assert_eq!(
        rig.kv().block_map_range(ino_a, 0, 100).await.unwrap(),
        vec![(0, a0.clone()), (7, a7.clone())]
    );
    rig.shutdown().await;
}

/// The range bounds are exact at the key level too: an ino's inclusive
/// scan window covers every legal index of that ino and nothing of its
/// neighbours (the `block_range` bounds law, keyed the map way).
#[test]
fn index_range_bounds_exactly_one_ino() {
    let (lo, hi) = index_range_from(5, 0);
    let inside = |k: [u8; BLOCK_MAP_KEY_LEN]| k[..] >= lo[..] && k[..] <= hi[..];
    assert!(inside(block_map_key(5, 0).unwrap()));
    assert!(inside(block_map_key(5, u32::MAX - 1).unwrap()));
    assert!(!inside(block_map_key(4, u32::MAX - 1).unwrap()));
    assert!(!inside(block_map_key(6, 0).unwrap()));
    // From a cursor: the window starts exactly there.
    let (lo, _hi) = index_range_from(5, 9);
    assert!(block_map_key(5, 8).unwrap()[..] < lo[..]);
    assert!(block_map_key(5, 9).unwrap()[..] >= lo[..]);
}

/// Map records survive a crash + remount through the journal alone (no
/// checkpoint ran — the §4.10 whole-transaction-atomicity claim), and the
/// post-crash mount's root mint is idempotent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn map_records_survive_a_crash_remount_through_the_journal() {
    let meta = NamedTempFile::new().unwrap();
    format_meta_stamped(meta.path()).await;

    let (ino, entry) = {
        let rig = mount(meta.path()).await;
        let ino = rig.mk_file("crash_survivor").await;
        let entry = point(0xA1, 12 * 4 * 1024 * 1024);
        rig.kv()
            .set_layout_and_size_with_map(
                ino,
                b"layout-bytes",
                4 * 1024 * 1024,
                &[],
                &[BlockMapOp::Put {
                    owner_ino: ino,
                    block_index: 12,
                    entry: entry.clone(),
                }],
            )
            .await
            .expect("publish");
        // CRASH: no `shutdown()`, so nothing is checkpointed on purpose —
        // the record survives only through the journal.
        drop(rig);
        (ino, entry)
    };

    let rig = mount(meta.path()).await;
    assert!(rig.kv().block_map_tree_engaged());
    assert_eq!(
        rig.kv().get_block_mapping(ino, 12).await.expect("lookup"),
        Some(entry),
        "the map record must replay out of the journal"
    );
    rig.shutdown().await;
}

// ---------------------------------------------------------------------------
// 5. Compatibility (the bit-9 identity discipline).
// ---------------------------------------------------------------------------

/// A fresh format must NOT carry bit 16, its sector 0 must be
/// byte-identical across a mount (mount never stamps), and the tree-id /
/// bit-table pins hold: tree 7 beside tree 6, inside the journal tag
/// nibble, bit 16 known but outside the multi-writer nine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_format_does_not_carry_bit16_and_sector0_stays_byte_identical() {
    // The on-disk pins.
    assert_eq!(TREE_BLOCK_MAP, 7, "the §4.2 tree table pins the id");
    assert_eq!(TREE_ID_MAX, TREE_BLOCK_MAP);
    assert_eq!(TREE_BLOCK_REFS, 6, "tree 7 sits beside the refs tree");
    assert!(
        TREE_BLOCK_MAP <= 0x0F,
        "the journal tag nibble bounds tree ids at 15"
    );
    assert_eq!(
        untag(tag_for(TREE_BLOCK_MAP, 3)),
        (TREE_BLOCK_MAP, 3),
        "tree 7 round-trips the journal tag byte at every level"
    );
    assert_eq!(
        FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE,
        1 << 16,
        "the kvmap tree bit is 16 (0..=15 are claimed)"
    );
    assert_ne!(
        FEATURES_INCOMPAT_KNOWN & FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE,
        0,
        "this binary must understand bit 16"
    );
    assert_eq!(
        MULTI_WRITER_FORMAT_BITS & FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE,
        0,
        "bit 16 is not part of the one-act multi-writer stamp set"
    );

    let meta = NamedTempFile::new().unwrap();
    format_meta(meta.path()).await;
    let VolumeFormat::V3(sb) = classify_volume(meta.path()).await.unwrap() else {
        panic!("expected v3");
    };
    assert_eq!(
        sb.features_incompat & FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE,
        0,
        "format must not stamp bit 16 — PR 2's crossing owns the \
         bit-before-first-record act"
    );
    assert!(!sb.block_map_tree_stamped());

    let before = std::fs::read(meta.path()).unwrap()[..4096].to_vec();
    let rig = mount(meta.path()).await;
    assert!(
        !rig.kv().block_map_tree_engaged(),
        "an un-stamped volume must not engage the map tree"
    );
    rig.shutdown().await;
    let after = std::fs::read(meta.path()).unwrap()[..4096].to_vec();
    assert_eq!(
        before, after,
        "mounting an un-stamped volume must leave sector 0 byte-identical"
    );
}

/// Stamping the bit engages the tree on the next mount, which mints the
/// missing root itself; an empty engaged volume answers empty, not an
/// error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stamping_bit16_engages_the_tree_on_the_next_mount() {
    let meta = NamedTempFile::new().unwrap();
    format_meta_stamped(meta.path()).await;
    let VolumeFormat::V3(sb) = classify_volume(meta.path()).await.unwrap() else {
        panic!("expected v3");
    };
    assert!(sb.block_map_tree_stamped());

    let rig = mount(meta.path()).await;
    assert!(
        rig.kv().block_map_tree_engaged(),
        "the stamp engages the tree (the ledger names no root — the mount mints one)"
    );
    let ino = rig.mk_file("empty").await;
    assert_eq!(rig.kv().get_block_mapping(ino, 0).await.unwrap(), None);
    assert!(rig
        .kv()
        .block_map_range(ino, 0, 16)
        .await
        .unwrap()
        .is_empty());
    rig.shutdown().await;
}
