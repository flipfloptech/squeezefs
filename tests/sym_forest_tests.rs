//! Symmetric metadata program, PR 1 — **the slot-tree forest**
//! (`docs/design-symmetric-metadata.md` §5.2 B(i), §5.4 C, §7.1; KD-SYM-2).
//!
//! Under incompat bit 17 (`KV_SYMMETRIC_FOREST`) a metadata volume holds
//! ONE mixed-kind KV tree per routing slot instead of one tree per record
//! kind. Every content key gains a kind byte at offset 8 (the ino-major
//! family) or as a prefix (the by-block family), so a file's inode, its
//! `layout` xattr, its block map and — for a directory — its entries are
//! adjacent in one leaf; the record's slot is read from its key; the
//! control tree (`TREE_CONTROL = 8`, "tree 0") records the roots of every
//! non-native slot tree; the fixed ledger names tree 0 and the native
//! slot tree.
//!
//! **Bit 17 lands DARK**: nothing stamps it but the test seam
//! `SQUEEZEFS_TEST_STAMP_SYMMETRIC=1` (the `SQUEEZEFS_TEST_STAMP_BLOCK_REFS`
//! precedent) — `format --symmetric` is PR 11's and the default flip PR
//! 14's. The most important contract here is therefore the negative one:
//! a volume WITHOUT the bit is byte-for-byte the shipped format and takes
//! the shipped code path.
//!
//! Contracts pinned (the PR-1 row of the design's PR plan):
//! - the kind bytes ARE the tree ids; `TREE_CONTROL = 8`,
//!   `TREE_SHARED_INDEX = 9`, `TREE_ID_MAX = 9`, interior kind 0;
//! - key lengths inode 9 / dentry 17 / xattr 17 / block-map 13 / refs 29;
//! - forest keys round-trip and preserve memcmp order within a kind;
//! - "a kind byte is never another tree's id" (round-3 Issue 6);
//! - a record routes to the slot its key names (native = 0, guest s = s+1;
//!   refs by their OWNER ino);
//! - `stat + layout` is one leaf: ino-major adjacency;
//! - `tag_for(0, level ≥ 1)` for interior records, refused at level 0;
//! - bit 17's constant, its place in the known set, the seam's registry
//!   entry (`Kind::Harness`);
//! - tree-0 `slot_state` records round-trip and refuse malformed images;
//! - format without the seam stamps nothing and names the shipped roots;
//!   format under the seam names tree 0 + the native slot tree and NO
//!   per-kind root;
//! - the forest MOUNTS and serves the shipped conformance population
//!   (lookup / getattr / readdir / xattr) exactly like the un-stamped
//!   volume; mutations commit, checkpoint and REPLAY to the same digest
//!   (replay-twice); an empty slot owns no extent; a root swap of a slot
//!   tree pins the checkpoint floor until the ledger's tree-0 root names
//!   the new root (the dying-floor law, one tree at a time).

use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::block_map::block_map_key;
use squeezefs::meta_backend::kv::block_refs::{block_ref_key, BlockRef, BlockRefOp};
use squeezefs::meta_backend::kv::builder::{
    digest_backend, format_v3_stamped, BuilderConfig, FormatV3Options, ImageBuilder, ROOT_INO,
};
use squeezefs::meta_backend::kv::checkpoint::read_newest_ledger;
use squeezefs::meta_backend::kv::journal::{
    decode_entry_payload, encode_entry_payload, tag_for, untag,
};
use squeezefs::meta_backend::kv::record::{
    dentry_key, forest_key, forest_key_slot, guest_forest_slot, inode_key, is_slot_tree_kind,
    split_forest_key, xattr_key, Record, FOREST_BLOCK_MAP_KEY_LEN, FOREST_BLOCK_REF_KEY_LEN,
    FOREST_DENTRY_KEY_LEN, FOREST_INODE_KEY_LEN, FOREST_SLOT_MAX, FOREST_XATTR_KEY_LEN,
    KIND_INTERIOR, NATIVE_FOREST_SLOT, TREE_BLOCK_MAP, TREE_BLOCK_REFS, TREE_CONTROL,
    TREE_DENTRIES, TREE_ID_MAX, TREE_INODES, TREE_SHARED_INDEX, TREE_XATTRS,
};
use squeezefs::meta_backend::kv::revalidate::RevalidationPoller;
use squeezefs::meta_backend::kv::slot_state::{slot_state_key, SlotState};
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, VolumeFormat, FEATURES_INCOMPAT_KNOWN, FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE,
    FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST,
};
use squeezefs::meta_backend::kv::tree::RootPtr;
use squeezefs::meta_backend::kv::{
    META_KV_FOREST_READER_WINDOW_SKIPS, META_KV_FOREST_SLOT_TREES_MINTED, META_KV_NODE_APPENDS,
    META_KV_NODE_REWRITE_BYTES,
};
use squeezefs::meta_backend::{
    guest_local_ino, open_routed_meta_set, open_routed_meta_set_read_only, open_volume_for_mount,
    plan_meta_slot_set, plan_meta_slot_set_with_width, Metadata, GUEST_NS_SHIFT, MINT_SPREAD,
};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Small volumes: 64 KiB nodes, a 1 MiB ring (the kv_backend_tests shape).
const VOL_LEN: u64 = 64 * 1024 * 1024;
const NODE_SIZE: usize = 64 * 1024;
const RING_LEN: u64 = 1024 * 1024;
const TEST_SEED: u64 = 0x5EED_F0E5_7000_0001;
const TEST_UUID: [u8; 16] = *b"sym-forest-test!";

/// The seam is process-global; suites in this binary serialize their
/// format calls through it so a parallel un-stamped format never
/// observes a sibling's stamp (async-aware: the guard spans the build).
static SEAM: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn builder_config() -> BuilderConfig {
    BuilderConfig {
        node_size: NODE_SIZE,
        journal_len_override: Some(RING_LEN),
        hash_seed: TEST_SEED,
        uuid: TEST_UUID,
    }
}

/// The kv_backend_tests population, verbatim: /docs (0750 1000:1000),
/// /docs/readme.txt (0644, 4096 B, two xattrs), /hello.bin, /empty,
/// /hard.lnk → hello.bin.
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
    b.set_xattr(readme, "user.big", &vec![0xAB; 6000]).unwrap();
    let hello = b.add_file(ROOT_INO, "hello.bin", 0o600, 0, 0, 0).unwrap();
    inos.insert("hello.bin", hello);
    let empty = b.add_dir(ROOT_INO, "empty", 0o755, 0, 0).unwrap();
    inos.insert("empty", empty);
    b.add_link(hello, ROOT_INO, "hard.lnk").unwrap();
    (b, inos)
}

/// Build the population image at `file`, stamped iff `symmetric`.
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

async fn superblock_of(
    file: &NamedTempFile,
) -> squeezefs::meta_backend::kv::superblock::SuperblockV3 {
    match classify_volume(file.path()).await.expect("classify") {
        VolumeFormat::V3(sb) => sb,
        other => panic!("expected a v3 superblock, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// §5.2.1 — kinds, tags, key codecs.
// ---------------------------------------------------------------------------

#[test]
fn kind_bytes_are_the_tree_ids_and_the_id_space_extends_to_the_control_and_shared_trees() {
    // The kind bytes ARE today's tree ids, so the journal tag nibble is
    // unchanged for content records.
    assert_eq!(TREE_INODES, 1);
    assert_eq!(TREE_DENTRIES, 2);
    assert_eq!(TREE_XATTRS, 3);
    assert_eq!(TREE_BLOCK_REFS, 6);
    assert_eq!(TREE_BLOCK_MAP, 7);
    assert_eq!(
        KIND_INTERIOR, 0,
        "interior records of a slot tree carry kind 0"
    );
    assert_eq!(TREE_CONTROL, 8, "tree 0 is TREE_CONTROL = 8");
    assert_eq!(TREE_SHARED_INDEX, 9, "the shared index is kind 9");
    assert_eq!(
        TREE_ID_MAX, 9,
        "TREE_ID_MAX 7 → 9 — inside the tag nibble's 15"
    );
    for kind in [
        TREE_INODES,
        TREE_DENTRIES,
        TREE_XATTRS,
        TREE_BLOCK_REFS,
        TREE_BLOCK_MAP,
    ] {
        assert!(
            is_slot_tree_kind(kind),
            "kind {kind} lives inside a slot tree"
        );
    }
    for not in [
        KIND_INTERIOR,
        4u8,
        5,
        TREE_CONTROL,
        TREE_SHARED_INDEX,
        10,
        0xFF,
    ] {
        assert!(
            !is_slot_tree_kind(not),
            "kind {not} is not a slot-tree content kind"
        );
    }
}

#[test]
fn forest_key_lengths_match_the_design() {
    assert_eq!(FOREST_INODE_KEY_LEN, 9);
    assert_eq!(FOREST_DENTRY_KEY_LEN, 17);
    assert_eq!(FOREST_XATTR_KEY_LEN, 17);
    assert_eq!(FOREST_BLOCK_MAP_KEY_LEN, 13);
    assert_eq!(FOREST_BLOCK_REF_KEY_LEN, 29);
    let ino = guest_local_ino(7, 42);
    assert_eq!(
        forest_key(TREE_INODES, &inode_key(ino)).unwrap().len(),
        FOREST_INODE_KEY_LEN
    );
    assert_eq!(
        forest_key(TREE_DENTRIES, &dentry_key(ino, 0x1234, 1))
            .unwrap()
            .len(),
        FOREST_DENTRY_KEY_LEN
    );
    assert_eq!(
        forest_key(TREE_XATTRS, &xattr_key(ino, 0x5678, 0))
            .unwrap()
            .len(),
        FOREST_XATTR_KEY_LEN
    );
    assert_eq!(
        forest_key(TREE_BLOCK_MAP, &block_map_key(ino, 3).unwrap())
            .unwrap()
            .len(),
        FOREST_BLOCK_MAP_KEY_LEN
    );
    assert_eq!(
        forest_key(TREE_BLOCK_REFS, &block_ref_key(9, 11, ino, 0))
            .unwrap()
            .len(),
        FOREST_BLOCK_REF_KEY_LEN
    );
}

#[test]
fn forest_keys_round_trip_every_kind_and_refuse_a_wrong_length() {
    let ino = guest_local_ino(3, 0xBEEF);
    let legacy: Vec<(u8, Vec<u8>)> = vec![
        (TREE_INODES, inode_key(ino).to_vec()),
        (TREE_DENTRIES, dentry_key(ino, 0xABC, 7).to_vec()),
        (TREE_XATTRS, xattr_key(ino, 0xDEF, 2).to_vec()),
        (TREE_BLOCK_MAP, block_map_key(ino, 99).unwrap().to_vec()),
        (TREE_BLOCK_REFS, block_ref_key(1, 2, ino, 4).to_vec()),
    ];
    for (kind, key) in &legacy {
        let f = forest_key(*kind, key).expect("encode");
        let (k2, back) = split_forest_key(&f).expect("decode");
        assert_eq!(k2, *kind, "kind byte survives the round trip");
        assert_eq!(
            &back[..],
            &key[..],
            "the legacy key survives the round trip"
        );
        // A wrong-length legacy key for the kind is refused, never
        // silently framed.
        let mut short = key.clone();
        short.pop();
        assert!(
            forest_key(*kind, &short).is_err(),
            "kind {kind}: a {}-byte legacy key must be refused",
            short.len()
        );
    }
    // The ino-major key bytes: ino ‖ kind ‖ rest.
    let f = forest_key(TREE_INODES, &inode_key(ino)).unwrap();
    assert_eq!(&f[..8], &ino.to_be_bytes());
    assert_eq!(f[8], TREE_INODES);
    // The by-block key bytes: kind ‖ legacy.
    let f = forest_key(TREE_BLOCK_REFS, &block_ref_key(1, 2, ino, 4)).unwrap();
    assert_eq!(f[0], TREE_BLOCK_REFS);
    assert_eq!(&f[1..], &block_ref_key(1, 2, ino, 4));
}

/// xorshift64* — deterministic key population for the order law.
fn rng(seed: &mut u64) -> u64 {
    *seed ^= *seed >> 12;
    *seed ^= *seed << 25;
    *seed ^= *seed >> 27;
    seed.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

#[test]
fn forest_keys_preserve_memcmp_order_within_a_kind() {
    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    let mut inos: Vec<u64> = (0..400)
        .map(|i| {
            if i % 3 == 0 {
                rng(&mut s) & ((1 << 40) - 1) // native
            } else {
                guest_local_ino((rng(&mut s) % 65536) as u16, rng(&mut s) & ((1 << 40) - 1))
            }
        })
        .collect();
    inos.sort_unstable();
    inos.dedup();
    for kind in [
        TREE_INODES,
        TREE_DENTRIES,
        TREE_XATTRS,
        TREE_BLOCK_MAP,
        TREE_BLOCK_REFS,
    ] {
        let mut legacy: Vec<Vec<u8>> = inos
            .iter()
            .map(|&ino| match kind {
                TREE_INODES => inode_key(ino).to_vec(),
                TREE_DENTRIES => {
                    dentry_key(ino, rng(&mut s) >> 10, (rng(&mut s) & 0xFF) as u8).to_vec()
                }
                TREE_XATTRS => {
                    xattr_key(ino, rng(&mut s) >> 8, (rng(&mut s) & 0xFF) as u8).to_vec()
                }
                TREE_BLOCK_MAP => block_map_key(ino, (rng(&mut s) as u32) & 0x7FFF_FFFF)
                    .unwrap()
                    .to_vec(),
                _ => block_ref_key(rng(&mut s), rng(&mut s), ino, rng(&mut s) as u32).to_vec(),
            })
            .collect();
        legacy.sort();
        let forest: Vec<Vec<u8>> = legacy
            .iter()
            .map(|k| forest_key(kind, k).unwrap())
            .collect();
        for w in forest.windows(2) {
            assert!(
                w[0] < w[1],
                "kind {kind}: forest keys must sort exactly like their legacy keys"
            );
        }
    }
}

#[test]
fn a_kind_byte_is_never_another_trees_id() {
    // Round-3 Issue 6: content records are journaled `tag_for(kind, 0)`
    // and routed by tag + key slot, so a kind `0x08` inside a slot tree
    // would be indistinguishable from a tree-0 record. The codec refuses
    // it on both sides.
    let ino = guest_local_ino(1, 1);
    for kind in [KIND_INTERIOR, 4u8, 5, TREE_CONTROL, TREE_SHARED_INDEX, 10] {
        assert!(
            forest_key(kind, &inode_key(ino)).is_err(),
            "kind {kind} must never be framed as a slot-tree key"
        );
    }
    let mut forged = forest_key(TREE_INODES, &inode_key(ino)).unwrap();
    forged[8] = TREE_CONTROL;
    assert!(
        split_forest_key(&forged).is_err(),
        "kind byte 8 inside a slot tree is corruption"
    );
    forged[8] = KIND_INTERIOR;
    assert!(
        split_forest_key(&forged).is_err(),
        "kind byte 0 inside a slot tree is corruption"
    );
    // A by-block prefix that is not the refs kind is refused too.
    let mut refs = forest_key(TREE_BLOCK_REFS, &block_ref_key(1, 1, ino, 0)).unwrap();
    refs[0] = TREE_CONTROL;
    assert!(split_forest_key(&refs).is_err());
    assert!(split_forest_key(&[]).is_err(), "an empty key is corruption");
    assert!(
        split_forest_key(&[0u8; 8]).is_err(),
        "an 8-byte key has no kind byte"
    );
}

#[test]
fn a_record_routes_to_the_slot_its_key_names() {
    // Native locals (< 2^40) → slot 0; guest slot s → s + 1 (the key ino's
    // top 24 bits, `(s+1) << 40`).
    assert_eq!(NATIVE_FOREST_SLOT, 0);
    assert_eq!(guest_forest_slot(0), 1);
    assert_eq!(guest_forest_slot(u16::MAX), 65_536);
    let native = 1u64;
    let guest = guest_local_ino(5, 7);
    assert_eq!(
        forest_key_slot(&forest_key(TREE_INODES, &inode_key(native)).unwrap()).unwrap(),
        NATIVE_FOREST_SLOT
    );
    assert_eq!(
        forest_key_slot(&forest_key(TREE_INODES, &inode_key(guest)).unwrap()).unwrap(),
        guest_forest_slot(5)
    );
    // A dentry belongs to its PARENT's slot.
    assert_eq!(
        forest_key_slot(&forest_key(TREE_DENTRIES, &dentry_key(guest, 1, 0)).unwrap()).unwrap(),
        guest_forest_slot(5)
    );
    assert_eq!(
        forest_key_slot(&forest_key(TREE_XATTRS, &xattr_key(guest, 1, 0)).unwrap()).unwrap(),
        guest_forest_slot(5)
    );
    assert_eq!(
        forest_key_slot(&forest_key(TREE_BLOCK_MAP, &block_map_key(guest, 1).unwrap()).unwrap())
            .unwrap(),
        guest_forest_slot(5)
    );
    // A block reference belongs to the REFERENCING ino's slot — the owner
    // at offset 17, never the volume tag or block index at the front.
    let owner = guest_local_ino(3, 1);
    let refs = forest_key(
        TREE_BLOCK_REFS,
        &block_ref_key(0xFFFF_FFFF, 0xFFFF_FFFF, owner, 0),
    )
    .unwrap();
    assert_eq!(forest_key_slot(&refs).unwrap(), guest_forest_slot(3));
    // Interior separators are real keys of the tree and route the same
    // way (the design's "routed to its slot tree by the separator key").
    let sep = forest_key(TREE_XATTRS, &xattr_key(guest, u64::MAX >> 8, 0xFF)).unwrap();
    assert_eq!(forest_key_slot(&sep).unwrap(), guest_forest_slot(5));
}

#[test]
fn stat_and_layout_are_adjacent_in_ino_major_order() {
    // Within one ino: inode (01) < dentries (02) < xattrs (03) < block
    // map (07); the next ino's inode sorts after all of them; every
    // by-block key sorts after every ino-major key.
    let ino = guest_local_ino(9, 1000);
    let next = ino + 1;
    let inode = forest_key(TREE_INODES, &inode_key(ino)).unwrap();
    let dentry_lo = forest_key(TREE_DENTRIES, &dentry_key(ino, 0, 0)).unwrap();
    let dentry_hi = forest_key(TREE_DENTRIES, &dentry_key(ino, (1 << 54) - 1, 0xFF)).unwrap();
    let layout = forest_key(TREE_XATTRS, &xattr_key(ino, 0x0001_2345_6789_ABCD, 0)).unwrap();
    let xattr_hi = forest_key(TREE_XATTRS, &xattr_key(ino, (1 << 56) - 1, 0xFF)).unwrap();
    let map_lo = forest_key(TREE_BLOCK_MAP, &block_map_key(ino, 0).unwrap()).unwrap();
    let map_hi = forest_key(TREE_BLOCK_MAP, &block_map_key(ino, u32::MAX - 1).unwrap()).unwrap();
    let next_inode = forest_key(TREE_INODES, &inode_key(next)).unwrap();
    let refs = forest_key(TREE_BLOCK_REFS, &block_ref_key(0, 0, 1, 0)).unwrap();
    let chain = [
        &inode,
        &dentry_lo,
        &dentry_hi,
        &layout,
        &xattr_hi,
        &map_lo,
        &map_hi,
        &next_inode,
    ];
    for w in chain.windows(2) {
        assert!(w[0] < w[1], "ino-major order: {:x?} < {:x?}", w[0], w[1]);
    }
    // The largest ino any slot names: the last local ino of the last
    // guest slot (the codec refuses anything above — no slot owns it).
    let last_ino = (u64::from(FOREST_SLOT_MAX) << GUEST_NS_SHIFT) | ((1u64 << GUEST_NS_SHIFT) - 1);
    let last_ino_major = forest_key(
        TREE_BLOCK_MAP,
        &block_map_key(last_ino, u32::MAX - 1).unwrap(),
    )
    .unwrap();
    assert!(
        last_ino_major < refs,
        "the by-block family (prefix 0x06) sorts after every ino-major key"
    );
    let above_ino = last_ino + 1;
    let above: [(u8, Vec<u8>); 4] = [
        (TREE_INODES, inode_key(above_ino).to_vec()),
        (TREE_DENTRIES, dentry_key(above_ino, 0, 0).to_vec()),
        (TREE_XATTRS, xattr_key(above_ino, 0, 0).to_vec()),
        (
            TREE_BLOCK_MAP,
            block_map_key(above_ino, 0).unwrap().to_vec(),
        ),
    ];
    for (kind, legacy) in &above {
        assert!(
            forest_key(*kind, legacy).is_err(),
            "kind {kind}: an ino above the slot namespace is refused at the encoder"
        );
    }
    assert!(
        forest_key(TREE_BLOCK_REFS, &block_ref_key(1, 1, above_ino, 0)).is_err(),
        "a reference OWNED by an ino above the namespace is refused"
    );
    let mut forged = forest_key(TREE_INODES, &inode_key(last_ino)).unwrap();
    forged[..8].copy_from_slice(&above_ino.to_be_bytes());
    assert!(
        forest_key_slot(&forged).is_err(),
        "the decoder refuses the same ino — one bound, both directions"
    );
}

#[test]
fn interior_records_carry_kind_zero_and_the_control_tree_tags_decode() {
    // `tag_for(0, level)` is legal iff level ≥ 1 (an interior record of a
    // slot tree); tree 0 is tagged by its own id; kind 9 is admissible.
    assert_eq!(tag_for(KIND_INTERIOR, 1), 0x10);
    assert_eq!(untag(0x10), (KIND_INTERIOR, 1));
    assert_eq!(tag_for(TREE_CONTROL, 0), TREE_CONTROL);
    assert_eq!(tag_for(TREE_CONTROL, 2), TREE_CONTROL | 0x20);
    assert_eq!(tag_for(TREE_SHARED_INDEX, 0), TREE_SHARED_INDEX);

    let rec = |key: &[u8]| Record::put(key.to_vec(), 5, b"v".to_vec());
    let ino = guest_local_ino(2, 2);
    let sep = forest_key(TREE_INODES, &inode_key(ino)).unwrap();
    let ok = encode_entry_payload(&[
        (tag_for(KIND_INTERIOR, 1), rec(&sep)),
        (
            tag_for(TREE_CONTROL, 0),
            rec(&slot_state_key(guest_forest_slot(2))),
        ),
        (tag_for(TREE_SHARED_INDEX, 0), rec(&[TREE_SHARED_INDEX; 29])),
        (tag_for(TREE_INODES, 0), rec(&sep)),
    ]);
    let decoded = decode_entry_payload(&ok).expect("the new tags decode");
    assert_eq!(decoded.len(), 4);
    assert_eq!(untag(decoded[0].0), (KIND_INTERIOR, 1));
    assert_eq!(untag(decoded[1].0), (TREE_CONTROL, 0));
    assert_eq!(untag(decoded[2].0), (TREE_SHARED_INDEX, 0));

    // Kind 0 at LEVEL 0 is not a record any writer produces: refused.
    let mut bad = encode_entry_payload(&[(tag_for(TREE_INODES, 0), rec(&sep))]);
    bad[0] = 0x00;
    assert!(
        decode_entry_payload(&bad).is_err(),
        "tag (kind 0, level 0) is structural corruption"
    );
    // Tree id 10 is past TREE_ID_MAX.
    bad[0] = 10;
    assert!(decode_entry_payload(&bad).is_err());
}

// ---------------------------------------------------------------------------
// §7.1 — bit 17 and the test seam.
// ---------------------------------------------------------------------------

#[test]
fn bit_17_is_the_symmetric_forest_and_is_known_to_this_binary() {
    assert_eq!(FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST, 1 << 17);
    assert_ne!(
        FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST, FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE,
        "bit 16 is the block-map tree"
    );
    assert_eq!(
        FEATURES_INCOMPAT_KNOWN & FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST,
        FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST,
        "a stamped volume must mount on this binary (and refuse on a pre-bit one)"
    );
}

#[test]
fn the_test_seam_is_a_registered_bool_knob() {
    // Its siblings (`TEST_STAMP_BLOCK_REFS` / `TEST_STAMP_WRITER_SCOPE`)
    // are `Kind::Bool`, so a malformed value refuses the process at
    // startup instead of silently keeping the default.
    let knob = squeezefs::env_knobs::lookup("SQUEEZEFS_TEST_STAMP_SYMMETRIC")
        .expect("ENG-10: every knob a site reads is registered");
    assert_eq!(knob.kind, squeezefs::env_knobs::Kind::Bool);
    assert_eq!(knob.default, "0");
}

// ---------------------------------------------------------------------------
// Tree 0 — `slot_state` records (kv/slot_state.rs).
// ---------------------------------------------------------------------------

#[test]
fn slot_state_records_round_trip_and_refuse_malformed_images() {
    let unleased = SlotState::Unleased {
        root: RootPtr {
            addr: 0xDEAD_0000,
            seq: 77,
        },
        cursor: 4242,
        g: 3,
        slot_tree_extents: 5,
        last_written: 9_001,
        seq_floor: 123_456,
    };
    let leased = SlotState::Leased {
        appender_id: 9,
        g: 4,
        page_addr: 0xF00D,
        root: RootPtr {
            addr: 0xBEEF_0000,
            seq: 78,
        },
        cursor: 4243,
        slot_tree_extents: 1,
        seq_floor: 123_457,
    };
    for st in [&unleased, &leased] {
        let img = st.encode();
        assert_eq!(&SlotState::decode(&img).expect("decode"), st);
        // Truncation is corruption, never a default.
        assert!(SlotState::decode(&img[..img.len() - 1]).is_err());
        // A future version refuses loud (forward-only format).
        let mut future = img.clone();
        future[0] = 0xFF;
        assert!(SlotState::decode(&future).is_err());
    }
    // An unknown variant byte refuses.
    let mut img = leased.encode();
    img[1] = 0x7F;
    assert!(SlotState::decode(&img).is_err());
    assert!(SlotState::decode(&[]).is_err());
}

/// The tails record (`slot_tails:{s}`, review round 3 Issue 21): inline
/// and spilled images round-trip, the inline cap derives from the value
/// cap, a count at the spill sentinel refuses the inline encoder, and
/// truncation / a future version refuse.
#[test]
fn slot_tails_records_round_trip_inline_and_spilled() {
    use squeezefs::meta_backend::kv::slot_state::{
        inline_tails_cap, tails_per_spill_extent, SlotTails, SlotTailsRecord, TailsSpill,
        SLOT_TAILS_FIXED_LEN, TAILS_SPILLED, TAIL_ENTRY_LEN,
    };
    let inline = SlotTailsRecord {
        g: 7,
        tails: SlotTails::Inline(vec![(0x1000, 12), (0x2000, 0)]),
    };
    let spilled = SlotTailsRecord {
        g: 8,
        tails: SlotTails::Spilled(TailsSpill {
            runs: vec![(0x40000, 21_845), (0x80000, 155)],
            checksum: 0xC0FFEE,
        }),
    };
    assert_eq!(inline.tails.count(), 2);
    assert_eq!(spilled.tails.count(), 22_000);
    assert_eq!(spilled.tails.spill_addrs(), vec![0x40000, 0x80000]);
    for rec in [&inline, &spilled] {
        let img = rec.encode().expect("encode");
        assert_eq!(&SlotTailsRecord::decode(&img).expect("decode"), rec);
        assert!(SlotTailsRecord::decode(&img[..img.len() - 1]).is_err());
        let mut future = img.clone();
        future[0] = 0xFF;
        assert!(SlotTailsRecord::decode(&future).is_err());
    }
    // The inline cap: a 64 KiB value cap carries ≈ 5,400 entries; the
    // 256 KiB spill extent 21,845.
    let cap = inline_tails_cap(65_536 + 512);
    assert_eq!(cap, (65_536 + 512 - SLOT_TAILS_FIXED_LEN) / TAIL_ENTRY_LEN);
    assert!(SlotTails::fits_inline(cap, 65_536 + 512));
    assert!(!SlotTails::fits_inline(cap + 1, 65_536 + 512));
    assert_eq!(tails_per_spill_extent(256 * 1024), 21_845);
    // A count at the sentinel is the spill's, never an inline image.
    let at_sentinel = SlotTailsRecord {
        g: 1,
        tails: SlotTails::Inline(vec![(0, 0); usize::from(TAILS_SPILLED)]),
    };
    assert!(at_sentinel.encode().is_err());
    assert!(SlotTailsRecord::decode(&[]).is_err());
}

#[test]
fn slot_state_keys_sort_by_slot_index_and_decode() {
    let k0 = slot_state_key(NATIVE_FOREST_SLOT);
    let k1 = slot_state_key(guest_forest_slot(0));
    let k2 = slot_state_key(guest_forest_slot(255));
    let k3 = slot_state_key(guest_forest_slot(256));
    let k4 = slot_state_key(guest_forest_slot(u16::MAX));
    assert!(
        k0 < k1 && k1 < k2 && k2 < k3 && k3 < k4,
        "memcmp order == slot order"
    );
    for (k, s) in [
        (&k0, NATIVE_FOREST_SLOT),
        (&k3, guest_forest_slot(256)),
        (&k4, guest_forest_slot(u16::MAX)),
    ] {
        assert_eq!(
            squeezefs::meta_backend::kv::slot_state::decode_slot_state_key(k).unwrap(),
            s
        );
    }
    assert!(
        squeezefs::meta_backend::kv::slot_state::decode_slot_state_key(b"slot_state:").is_err()
    );
}

// ---------------------------------------------------------------------------
// Format: the negative pin first, then the forest.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn format_without_the_seam_stamps_nothing_and_names_the_shipped_roots() {
    let file = NamedTempFile::new().unwrap();
    build_image(&file, false).await;
    let sb = superblock_of(&file).await;
    assert_eq!(
        sb.features_incompat & FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST,
        0,
        "bit 17 lands DARK: nothing but the seam stamps it (PR 11 is format --symmetric)"
    );
    let ledger = read_newest_ledger(file.path(), sb.root_ledger.start)
        .await
        .unwrap()
        .expect("bootstrap ledger record");
    let mut ids: Vec<u8> = ledger.tree_roots.iter().map(|r| r.tree_id).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![TREE_INODES, TREE_DENTRIES, TREE_XATTRS],
        "the un-stamped builder image names exactly the three §4.2 roots"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn format_under_the_seam_stamps_bit_17_and_names_tree_zero_and_the_native_slot_root() {
    let file = NamedTempFile::new().unwrap();
    build_image(&file, true).await;
    let sb = superblock_of(&file).await;
    assert_ne!(
        sb.features_incompat & FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST,
        0
    );
    assert!(sb.symmetric_forest_stamped());
    let ledger = read_newest_ledger(file.path(), sb.root_ledger.start)
        .await
        .unwrap()
        .expect("bootstrap ledger record");
    let mut ids: Vec<u8> = ledger.tree_roots.iter().map(|r| r.tree_id).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![KIND_INTERIOR, TREE_CONTROL],
        "the forest ledger names tree 0 (id 8) and the native slot tree (id 0) — no per-kind root"
    );
}

// ---------------------------------------------------------------------------
// Mount: the shipped conformance population, served from the forest.
// ---------------------------------------------------------------------------

async fn assert_population_served(b: &KvMetaBackend, inos: &HashMap<&'static str, u64>) {
    let root = b.getattr(ROOT_INO).await.expect("root getattr");
    assert_eq!(root.mode & libc::S_IFMT, libc::S_IFDIR);
    let docs = b.lookup(ROOT_INO, "docs").await.expect("lookup docs");
    assert_eq!(docs.ino, inos["docs"]);
    assert_eq!(docs.mode, libc::S_IFDIR | 0o750);
    let readme = b
        .lookup(docs.ino, "readme.txt")
        .await
        .expect("lookup readme");
    assert_eq!(readme.ino, inos["readme.txt"]);
    assert_eq!(readme.size, 4096);
    assert!(b.lookup(ROOT_INO, "no-such-entry").await.is_err());
    let mut names: Vec<String> = b
        .readdir(ROOT_INO, 0, usize::MAX)
        .await
        .expect("readdir root")
        .into_iter()
        .map(|d| d.name)
        .collect();
    names.sort();
    assert_eq!(names, vec!["docs", "empty", "hard.lnk", "hello.bin"]);
    assert_eq!(
        b.getxattr(readme.ino, "user.color")
            .await
            .expect("getxattr"),
        Some(b"blue".to_vec())
    );
    assert_eq!(
        b.getxattr(readme.ino, "user.big")
            .await
            .expect("big xattr")
            .map(|v| v.len()),
        Some(6000)
    );
    let hello = b.lookup(ROOT_INO, "hello.bin").await.unwrap();
    let link = b.lookup(ROOT_INO, "hard.lnk").await.unwrap();
    assert_eq!(hello.ino, link.ino);
    assert_eq!(hello.nlink, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_forest_mounts_and_serves_the_conformance_population() {
    let file = NamedTempFile::new().unwrap();
    let inos = build_image(&file, true).await;
    let b = open_volume_for_mount(file.path().to_str().unwrap())
        .await
        .expect("mount the forest");
    assert!(b.symmetric_forest(), "the mount reports the forest posture");
    assert_population_served(&b, &inos).await;
    b.shutdown().await.expect("clean shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_un_stamped_volume_takes_the_shipped_path_and_serves_the_same_population() {
    let file = NamedTempFile::new().unwrap();
    let inos = build_image(&file, false).await;
    let b = open_volume_for_mount(file.path().to_str().unwrap())
        .await
        .expect("mount");
    assert!(!b.symmetric_forest());
    assert_population_served(&b, &inos).await;
    b.shutdown().await.expect("clean shutdown");
}

/// Both formats of the same description fold to the same digest — the
/// §4.10 digest walk is over LOGICAL records (kind + legacy key + value),
/// so the forest is a relayout, never a rewrite.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_forest_and_the_shipped_layout_fold_to_the_same_digest() {
    let flat = NamedTempFile::new().unwrap();
    build_image(&flat, false).await;
    let forest = NamedTempFile::new().unwrap();
    build_image(&forest, true).await;
    let a = open_volume_for_mount(flat.path().to_str().unwrap())
        .await
        .unwrap();
    let b = open_volume_for_mount(forest.path().to_str().unwrap())
        .await
        .unwrap();
    let da = digest_backend(&a).await.expect("digest flat");
    let db = digest_backend(&b).await.expect("digest forest");
    assert_eq!(da, db, "same logical records ⇒ same digest across layouts");
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Mutations on the forest: commit → checkpoint → remount → replay digest.
// ---------------------------------------------------------------------------

/// Populate a forest through the backend's mutating API — inodes,
/// dentries and xattrs; creates, a hard link, an unlink per four.
async fn churn(b: &KvMetaBackend, rounds: u32) -> u64 {
    let dir = Metadata::create(b, ROOT_INO, "storm", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .expect("mkdir storm")
        .ino;
    for i in 0..rounds {
        let f = Metadata::create(b, dir, &format!("f{i:04}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap_or_else(|e| panic!("create f{i}: {e}"))
            .ino;
        b.setxattr(f, &format!("user.k{i}"), format!("v{i}").as_bytes())
            .await
            .expect("setxattr");
        if i % 4 == 3 {
            b.unlink(dir, &format!("f{:04}", i - 1))
                .await
                .expect("unlink");
        }
    }
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forest_mutations_commit_checkpoint_and_replay_to_the_same_digest() {
    let file = NamedTempFile::new().unwrap();
    build_image(&file, true).await;
    let b = open_volume_for_mount(file.path().to_str().unwrap())
        .await
        .unwrap();
    let dir = churn(&b, 64).await;
    let live_digest = digest_backend(&b).await.unwrap();
    // A CLEAN shutdown: checkpoint → tail == head; the next mount replays
    // nothing and must fold to the same digest from the leaf images alone.
    b.shutdown().await.expect("shutdown");
    drop(b);
    let again = open_volume_for_mount(file.path().to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        again.replay_stats().entries,
        0,
        "clean shutdown ⇒ empty replay window"
    );
    assert_eq!(digest_backend(&again).await.unwrap(), live_digest);
    // The population is still served through the forest after remount.
    assert_eq!(again.lookup(ROOT_INO, "storm").await.unwrap().ino, dir);
    let names = again.readdir(dir, 0, usize::MAX).await.unwrap();
    // 64 created, 16 unlinked (every i % 4 == 3 removes i-1).
    assert_eq!(names.len(), 64 - 16);
    let f5 = again.lookup(dir, "f0005").await.unwrap();
    assert_eq!(
        again.getxattr(f5.ino, "user.k5").await.unwrap(),
        Some(b"v5".to_vec())
    );
    again.shutdown().await.unwrap();
}

/// Replay-twice: a mount that is dropped without a checkpoint leaves
/// every barriered commit in the ring; two successive re-opens replay
/// the same window and must agree with the live digest and with each
/// other (§4.10 — a deterministic total order per ring).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forest_replay_twice_digest_equality() {
    let file = NamedTempFile::new().unwrap();
    build_image(&file, true).await;
    let b = open_volume_for_mount(file.path().to_str().unwrap())
        .await
        .unwrap();
    churn(&b, 48).await;
    let live = digest_backend(&b).await.unwrap();
    // Barrier the ring WITHOUT a checkpoint, then abandon the mount: the
    // drop-then-reopen replay pattern (the writer flock releases with the
    // last `Arc`).
    b.sync_device().await.expect("barrier");
    drop(b);
    let r1 = open_volume_for_mount(file.path().to_str().unwrap())
        .await
        .unwrap();
    assert!(
        r1.replay_stats().entries > 0,
        "the window was replayed, not checkpointed away"
    );
    let d1 = digest_backend(&r1).await.unwrap();
    drop(r1);
    let r2 = open_volume_for_mount(file.path().to_str().unwrap())
        .await
        .unwrap();
    let d2 = digest_backend(&r2).await.unwrap();
    assert_eq!(d1, live, "replay reproduces the live state");
    assert_eq!(
        d1, d2,
        "replay is deterministic: twice gives the same digest"
    );
    r2.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Guest slot trees: a STAMPED set mints into rotor slots (MINT_SPREAD of
// them), so its records land in guest slot trees whose roots live in
// tree 0 — the forest proper.
// ---------------------------------------------------------------------------

fn set_opts() -> FormatV3Options {
    FormatV3Options {
        node_size: NODE_SIZE,
        journal_len_override: Some(RING_LEN),
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// Format a one-member derived-width set under the seam and open it
/// routed. Every mint on a stamped member lands in one of its
/// `MINT_SPREAD` hosted guest slots (design-dynamic-meta-routing §5.4).
async fn stamped_forest_set(dir: &std::path::Path) -> Vec<String> {
    let p = dir.join("meta0");
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    {
        let _g = SEAM.lock().await;
        std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
        let r = format_v3_stamped(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone()).await;
        std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
        r.expect("format stamped forest member");
    }
    vec![p.display().to_string()]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stamped_set_mints_into_guest_slot_trees_whose_roots_ride_tree_zero() {
    let dir = tempfile::tempdir().unwrap();
    let uris = stamped_forest_set(dir.path()).await;
    let routed = open_routed_meta_set(&uris)
        .await
        .expect("open routed forest set");
    let vol = &routed.volumes[0];
    assert!(vol.symmetric_forest());
    let before = vol.forest_census().expect("forest");
    assert_eq!(before.slot_trees, 1, "fresh: the native slot tree only");

    let d = routed
        .create(ROOT_INO, "jobs", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .expect("mkdir jobs")
        .ino;
    let mut children = Vec::new();
    for i in 0..(2 * MINT_SPREAD as u32) {
        let f = routed
            .create(d, &format!("c{i:03}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create")
            .ino;
        routed
            .setxattr(f, "user.tag", b"t")
            .await
            .expect("setxattr");
        children.push(f);
    }
    // The mints spread over the volume's rotor: several DISTINCT guest
    // slots now hold records, each in its own tree — lazily minted.
    let after = vol.forest_census().expect("forest");
    assert!(
        after.slot_trees > 1,
        "guest slot trees were minted on demand: {} trees",
        after.slot_trees
    );
    let live = digest_backend(vol).await.unwrap();

    // Checkpoint: every guest slot tree's root is published into tree 0
    // (one `slot_state` record per minted slot), and the ledger names
    // tree 0 + the native root only.
    vol.checkpoint_now().await.expect("checkpoint");
    let published = vol.forest_census().expect("forest");
    // MINT_SPREAD rotor slots + the native slot: 2 × MINT_SPREAD files
    // spread over every rotor slot (the mint-spread law), one tree each,
    // one extent each — the per-slot extent floor §1.6 names.
    assert_eq!(
        published.slot_trees as usize, MINT_SPREAD,
        "every rotor slot minted its tree"
    );
    assert_eq!(published.minted as usize, MINT_SPREAD - 1);
    assert_eq!(
        published.control_records,
        published.slot_trees - 1,
        "tree 0 names every guest slot tree's root (the native root rides the ledger)"
    );
    let sb = vol.superblock().clone();
    let ledger = read_newest_ledger(std::path::Path::new(&uris[0]), sb.root_ledger.start)
        .await
        .unwrap()
        .expect("ledger");
    let mut ids: Vec<u8> = ledger.tree_roots.iter().map(|r| r.tree_id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![KIND_INTERIOR, TREE_CONTROL]);

    // Remount from the checkpoint: the guest roots come back through
    // tree 0 and the digest is unchanged; every child still resolves.
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
    drop(routed);
    let again = open_routed_meta_set(&uris).await.expect("remount");
    let vol = &again.volumes[0];
    assert_eq!(digest_backend(vol).await.unwrap(), live);
    assert_eq!(vol.forest_census().unwrap().slot_trees, after.slot_trees);
    for (i, ino) in children.iter().enumerate() {
        let got = again
            .lookup(d, &format!("c{i:03}"))
            .await
            .expect("lookup child");
        assert_eq!(got.ino, *ino);
        assert_eq!(
            again.getxattr(*ino, "user.tag").await.unwrap(),
            Some(b"t".to_vec())
        );
    }
    for v in &again.volumes {
        v.shutdown().await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// Roots: lazy slot trees, the floor pin.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_slot_owns_no_extent() {
    let file = NamedTempFile::new().unwrap();
    build_image(&file, true).await;
    let b = open_volume_for_mount(file.path().to_str().unwrap())
        .await
        .unwrap();
    // Nothing has minted a guest slot on this single-member image: the
    // forest holds exactly the native slot tree; tree 0 carries no
    // `slot_state` record; no extent is claimed for any other slot.
    let census = b.forest_census().expect("the mount is a forest");
    assert_eq!(census.slot_trees, 1, "the native slot tree only");
    assert_eq!(
        census.control_records, 0,
        "no slot_state record for an empty slot"
    );
    assert!(
        b.slot_tree_root(guest_forest_slot(0)).is_none(),
        "a slot with no records has no root and owns no extent"
    );
    b.shutdown().await.unwrap();
}

/// The dying-floor law, per slot tree: a compaction that swaps a slot
/// tree's root has no journaled pointer record, so the checkpoint's tail
/// must not pass the swap until a ledger record covers the new root. The
/// covering direction is asserted: after a churn that swaps the native
/// root, the checkpoint names the swapped root, and the swap's floor is
/// released within the progress theorem's bound — the SECOND barriered
/// cycle's tail stands at or past the pre-swap head (the first cycle's
/// record covers the swap; its own SMO records land past that cycle's
/// `H`, so reclamation lags one cycle by design).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_root_swap_pins_the_floor_until_the_ledger_names_it() {
    let file = NamedTempFile::new().unwrap();
    build_image(&file, true).await;
    let b = open_volume_for_mount(file.path().to_str().unwrap())
        .await
        .unwrap();
    let before = b.slot_tree_root(NATIVE_FOREST_SLOT).expect("native root");
    // Enough churn to fill the native root leaf's log (64 KiB nodes; 4 KB
    // xattr values) so the flush pass compacts or splits it.
    let dir = Metadata::create(b.as_ref(), ROOT_INO, "swap", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    for i in 0..48u32 {
        let f = Metadata::create(
            b.as_ref(),
            dir,
            &format!("g{i:03}"),
            libc::S_IFREG | 0o644,
            0,
            0,
        )
        .await
        .unwrap()
        .ino;
        b.setxattr(f, "user.pad", &vec![0x5A; 4000]).await.unwrap();
    }
    let head_before = b.journal_ring().core().head();
    b.checkpoint_now().await.expect("checkpoint");
    let after = b.slot_tree_root(NATIVE_FOREST_SLOT).expect("native root");
    assert_ne!(
        before, after,
        "the churn compacted (or split) the native root"
    );
    let sb = b.superblock().clone();
    let ledger = read_newest_ledger(file.path(), sb.root_ledger.start)
        .await
        .unwrap()
        .expect("ledger");
    let named = ledger
        .tree_roots
        .iter()
        .find(|r| r.tree_id == KIND_INTERIOR)
        .expect("the ledger names the native slot root");
    assert_eq!(
        (named.node_addr, named.node_seq),
        (after.addr, after.seq),
        "the checkpoint's ledger record names the swapped root"
    );
    // The first record covers the swap; the second cycle's tail passes
    // the pre-swap head (the bound the flush pass's progress theorem
    // states: every floor live at the next cycle belongs to records ≥ H).
    b.checkpoint_now().await.expect("second checkpoint");
    let ledger2 = read_newest_ledger(file.path(), sb.root_ledger.start)
        .await
        .unwrap()
        .expect("ledger");
    assert!(
        ledger2.journal_tail_seq >= head_before,
        "the covering records released the swap's dying floor (tail {} < pre-swap head {})",
        ledger2.journal_tail_seq,
        head_before
    );
    b.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Review round 1 — the forest ARM's own contracts (every one red against
// `df4682ff`): block references and block maps on a forest, the refs probe
// over every slot tree, the top separator at replay, the deferred
// publication's floor, a guest root swap covered through tree 0, the
// coherent reader.
// ---------------------------------------------------------------------------

/// A narrow stamped set — `width` hosted slots per volume, so every rotor
/// slot fills in a few dozen creates (the heavy-tree pins below).
async fn stamped_forest_set_width(dir: &std::path::Path, width: u32) -> Vec<String> {
    let p = dir.join("meta0");
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set_with_width(1, width).expect("derived plan");
    {
        let _g = SEAM.lock().await;
        std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
        let r = format_v3_stamped(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone()).await;
        std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
        r.expect("format stamped forest member");
    }
    vec![p.display().to_string()]
}

/// Issues 1 + 2: a forest volume COMMITS block-reference records for
/// guest-owned files (the write-side audit reads the legacy key), and the
/// by-block probes see EVERY slot tree — a block referenced by two files
/// in two different guest slots counts 2, and the volume scan returns
/// every reference, whatever slot owns it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn block_references_of_guest_owned_files_commit_and_are_counted_across_slot_trees() {
    let dir = tempfile::tempdir().unwrap();
    let uris = stamped_forest_set(dir.path()).await;
    let routed = open_routed_meta_set(&uris).await.expect("open");
    let vol = &routed.volumes[0];
    assert!(vol.block_refs_engaged(), "the default format stamps bit 9");
    let d = routed
        .create(ROOT_INO, "data", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    // Consecutive mints land in DIFFERENT rotor slots (the mint-spread
    // law), so these two owners live in two guest slot trees.
    let mut owners = Vec::new();
    for i in 0..2 {
        let g = routed
            .create(d, &format!("o{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap()
            .ino;
        owners.push(routed.route_ino(g).1);
    }
    let slot_of = |ino: u64| squeezefs::meta_backend::kv::record::forest_slot_of_ino(ino);
    assert_ne!(slot_of(owners[0]), slot_of(owners[1]), "two guest slots");
    assert_ne!(slot_of(owners[0]), NATIVE_FOREST_SLOT);
    let tag = squeezefs::meta_backend::kv::block_refs::volume_tag("vol-0011223344556677");
    // Both owners reference block 7; owner 1 also holds block 8.
    for (i, &owner) in owners.iter().enumerate() {
        let mut ops = vec![BlockRefOp::taken(BlockRef {
            vol_tag: tag,
            block_idx: 7,
            owner_ino: owner,
            block_index: 0,
        })];
        if i == 1 {
            ops.push(BlockRefOp::taken(BlockRef {
                vol_tag: tag,
                block_idx: 8,
                owner_ino: owner,
                block_index: 1,
            }));
        }
        vol.set_layout_and_size(owner, b"layout", 4096 * (i as u64 + 1), &ops)
            .await
            .expect("a forest volume commits block-reference records");
    }
    assert_eq!(
        vol.block_ref_count(tag, 7).await.unwrap(),
        2,
        "refcount(block) is the population over EVERY slot tree"
    );
    assert_eq!(vol.block_ref_count(tag, 8).await.unwrap(), 1);
    assert_eq!(vol.block_ref_count(tag, 9).await.unwrap(), 0);
    let mut scanned = vol.block_ref_scan(tag).await.unwrap();
    scanned.sort_by_key(|r| (r.block_idx, r.owner_ino));
    assert_eq!(
        scanned.len(),
        3,
        "the volume scan returns every reference of every slot"
    );
    assert_eq!(
        scanned.iter().map(|r| r.block_idx).collect::<Vec<_>>(),
        vec![7, 7, 8]
    );
    // Survives a checkpoint + remount (the refs live in their owners'
    // slot trees, whose roots ride tree 0).
    vol.checkpoint_now().await.unwrap();
    let live = digest_backend(vol).await.unwrap();
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
    drop(routed);
    let again = open_routed_meta_set(&uris).await.unwrap();
    let vol = &again.volumes[0];
    assert_eq!(vol.block_ref_count(tag, 7).await.unwrap(), 2);
    assert_eq!(vol.block_ref_scan(tag).await.unwrap().len(), 3);
    assert_eq!(digest_backend(vol).await.unwrap(), live);
    for v in &again.volumes {
        v.shutdown().await.unwrap();
    }
}

/// Issue 3: the rightmost leaf of a slot tree of height ≥ 1 journals its
/// SMO pointer record under `KEY_SPACE_MAX`; replay must route it to the
/// tree that produced it — never mint a phantom slot from the sentinel's
/// bytes. The native slot tree is a slot tree for this purpose (its
/// interior records carry kind 0 like every other's).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rightmost_leaf_smo_in_the_window_replays_into_its_own_tree_not_a_phantom_slot() {
    let file = NamedTempFile::new().unwrap();
    build_image(&file, true).await;
    let b = open_volume_for_mount(file.path().to_str().unwrap())
        .await
        .unwrap();
    let dir = Metadata::create(b.as_ref(), ROOT_INO, "tall", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    let mut inos = Vec::new();
    async fn fill(b: &KvMetaBackend, dir: u64, lo: u32, hi: u32) -> Vec<u64> {
        let mut out = Vec::new();
        for i in lo..hi {
            let f = Metadata::create(b, dir, &format!("t{i:04}"), libc::S_IFREG | 0o644, 0, 0)
                .await
                .unwrap()
                .ino;
            b.setxattr(f, "user.pad", &vec![0x33; 4000]).await.unwrap();
            out.push(f);
        }
        out
    }
    // Round 1: past one 64 KiB leaf → the flush pass splits the root
    // (height 1). Round 2: the new (highest) inos fill the RIGHTMOST leaf,
    // whose compaction journals a pointer record keyed KEY_SPACE_MAX past
    // the cycle's `H` — an interior record IN the replay window.
    inos.extend(fill(b.as_ref(), dir, 0, 48).await);
    b.checkpoint_now().await.unwrap();
    let root_level = b
        .slot_tree_root_level(NATIVE_FOREST_SLOT)
        .await
        .expect("native root");
    assert!(
        root_level >= 1,
        "the churn must have grown an interior (got level {root_level})"
    );
    inos.extend(fill(b.as_ref(), dir, 48, 72).await);
    b.checkpoint_now().await.unwrap();
    let live = digest_backend(&b).await.unwrap();
    assert!(
        b.journal_ring().core().head() > b.ledger_tail(),
        "SMO records of the second cycle sit past its tail — the window is not empty"
    );
    b.sync_device().await.unwrap();
    drop(b);
    let again = open_volume_for_mount(file.path().to_str().unwrap())
        .await
        .unwrap();
    let census = again.forest_census().unwrap();
    assert_eq!(
        census.slot_trees, 1,
        "replay minted a PHANTOM slot tree from the KEY_SPACE_MAX separator"
    );
    assert_eq!(digest_backend(&again).await.unwrap(), live);
    for ino in &inos {
        assert_eq!(
            again
                .getxattr(*ino, "user.pad")
                .await
                .unwrap()
                .map(|v| v.len()),
            Some(4000)
        );
    }
    again.checkpoint_now().await.unwrap();
    assert_eq!(
        again.forest_census().unwrap().control_records,
        0,
        "no phantom slot_state was published"
    );
    again.shutdown().await.unwrap();
}

/// Issue 4: when the tree-0 publication is DEFERRED (checkpoint reserve
/// exhausted), the cycle's ledger record must not let the tail pass the
/// unpublished roots — a fresh guest tree's records stay in the replay
/// window until tree 0 names the root. Pinned through the publication
/// deferral seam: one deferred cycle, crash, every acked record served.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deferred_root_publication_keeps_the_unpublished_roots_in_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let uris = stamped_forest_set_width(dir.path(), 8).await;
    let routed = open_routed_meta_set(&uris).await.expect("open");
    let vol = &routed.volumes[0];
    let d = routed
        .create(ROOT_INO, "d", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    let mut children = Vec::new();
    for i in 0..16 {
        let f = routed
            .create(d, &format!("c{i:02}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap()
            .ino;
        routed.setxattr(f, "user.k", b"v").await.unwrap();
        children.push(f);
    }
    assert!(
        vol.forest_census().unwrap().slot_trees > 1,
        "guest trees were minted"
    );
    let live = digest_backend(vol).await.unwrap();
    // The seam: EVERY publication attempt answers JournalReserveExhausted
    // until cleared (the background tick must not publish behind the
    // test's back before the crash). The guard clears it on every exit
    // path — the seam is process-global.
    struct DeferSeam;
    impl Drop for DeferSeam {
        fn drop(&mut self) {
            squeezefs::meta_backend::kv::backend::TEST_FOREST_PUBLISH_DEFER
                .store(0, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let seam = DeferSeam;
    squeezefs::meta_backend::kv::backend::TEST_FOREST_PUBLISH_DEFER
        .store(u32::MAX, std::sync::atomic::Ordering::SeqCst);
    vol.checkpoint_now()
        .await
        .expect("a deferred publication is not an error");
    assert_eq!(
        vol.forest_census().unwrap().control_records,
        0,
        "nothing was published this cycle"
    );
    // Crash-equivalent: barriered, dropped without another checkpoint.
    vol.sync_device().await.unwrap();
    drop(routed);
    drop(seam);
    let again = open_routed_meta_set(&uris).await.expect("remount");
    let vol = &again.volumes[0];
    assert_eq!(
        digest_backend(vol).await.unwrap(),
        live,
        "acked records under unpublished roots were lost across the deferred cycle"
    );
    for (i, ino) in children.iter().enumerate() {
        assert_eq!(
            again
                .lookup(d, &format!("c{i:02}"))
                .await
                .expect("child resolves")
                .ino,
            *ino
        );
        assert_eq!(
            again.getxattr(*ino, "user.k").await.unwrap(),
            Some(b"v".to_vec())
        );
    }
    for v in &again.volumes {
        v.shutdown().await.unwrap();
    }
}

/// A GUEST root swap is covered through tree 0 (the new mechanism the
/// native pin cannot reach): every rotor slot's root leaf fills and
/// compacts/splits; the checkpoint publishes the swapped roots; a remount
/// reopens each guest at the root tree 0 names and serves everything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_guest_root_swap_is_covered_through_tree_zero() {
    let dir = tempfile::tempdir().unwrap();
    let uris = stamped_forest_set_width(dir.path(), 8).await;
    let routed = open_routed_meta_set(&uris).await.expect("open");
    let vol = &routed.volumes[0];
    let d = routed
        .create(ROOT_INO, "swap", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    // 8 rotor slots × 15 files × 4 KB xattrs: every guest root leaf's
    // log fills past one 64 KiB node.
    let mut inos = Vec::new();
    for i in 0..120u32 {
        let f = routed
            .create(d, &format!("g{i:03}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap()
            .ino;
        routed
            .setxattr(f, "user.pad", &vec![0x44; 4000])
            .await
            .unwrap();
        inos.push(f);
    }
    let before: Vec<(u32, RootPtr)> = vol.forest_roots();
    vol.checkpoint_now().await.unwrap();
    let after: Vec<(u32, RootPtr)> = vol.forest_roots();
    let swapped = before
        .iter()
        .zip(&after)
        .filter(|((s0, r0), (s1, r1))| s0 == s1 && s0 != &NATIVE_FOREST_SLOT && r0 != r1)
        .count();
    assert!(
        swapped >= 1,
        "at least one guest root swapped under the churn"
    );
    let census = vol.forest_census().unwrap();
    assert_eq!(census.control_records, census.slot_trees - 1);
    let live = digest_backend(vol).await.unwrap();
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
    drop(routed);
    let again = open_routed_meta_set(&uris).await.unwrap();
    let vol = &again.volumes[0];
    assert_eq!(
        vol.forest_roots(),
        after,
        "every guest reopened at the root tree 0 names"
    );
    assert_eq!(digest_backend(vol).await.unwrap(), live);
    for ino in &inos {
        assert_eq!(
            again
                .getxattr(*ino, "user.pad")
                .await
                .unwrap()
                .map(|v| v.len()),
            Some(4000)
        );
    }
    for v in &again.volumes {
        v.shutdown().await.unwrap();
    }
}

/// Issue 6: a coherent reader (`-o ro`) of a forest set resolves a guest
/// ino created AFTER its mount — its poll adopts tree 0's root, re-reads
/// the slot roots tree 0 names and opens the guests it did not know; it
/// never re-roots a guest at the native root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_reader_of_a_forest_resolves_guests_minted_after_its_mount() {
    let dir = tempfile::tempdir().unwrap();
    let uris = stamped_forest_set(dir.path()).await;
    let writer = open_routed_meta_set(&uris).await.expect("writer");
    // A first guest exists BEFORE the reader mounts (its root moves later).
    let d = writer
        .create(ROOT_INO, "shared", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    let early = writer
        .create(d, "early", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    for v in &writer.volumes {
        v.checkpoint_now().await.unwrap();
    }
    let reader = open_routed_meta_set_read_only(&uris).await.expect("reader");
    for v in &reader.volumes {
        v.arm_reader_revalidation(None).unwrap();
    }
    assert_eq!(reader.lookup(d, "early").await.unwrap().ino, early);

    // Post-mount: new guests (fresh slot trees the reader never saw) and
    // churn on the early one (its root swaps).
    let mut late = Vec::new();
    for i in 0..(2 * MINT_SPREAD as u32) {
        let f = writer
            .create(d, &format!("late{i:03}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap()
            .ino;
        writer
            .setxattr(f, "user.pad", &vec![0x55; 2048])
            .await
            .unwrap();
        late.push(f);
    }
    writer
        .setxattr(early, "user.pad", &vec![0x66; 4000])
        .await
        .unwrap();
    for v in &writer.volumes {
        v.checkpoint_now().await.unwrap();
    }
    let poller = RevalidationPoller::new(1);
    let outcomes = poller
        .poll_set_at(&reader.volumes, std::time::Instant::now())
        .await;
    for (_, res) in &outcomes {
        assert!(
            res.as_ref().expect("poll").advanced,
            "the reader adopted the new checkpoint"
        );
    }
    // Every post-mount guest resolves; the early one still does, with its
    // new content; nothing was re-rooted at the native root.
    for (i, ino) in late.iter().enumerate() {
        let got = reader
            .lookup(d, &format!("late{i:03}"))
            .await
            .unwrap_or_else(|e| {
                panic!("late{i:03} (a post-mount guest) must resolve on the reader: {e}")
            });
        assert_eq!(got.ino, *ino);
        assert_eq!(
            reader
                .getxattr(*ino, "user.pad")
                .await
                .unwrap()
                .map(|v| v.len()),
            Some(2048)
        );
    }
    assert_eq!(
        reader
            .getxattr(early, "user.pad")
            .await
            .unwrap()
            .map(|v| v.len()),
        Some(4000)
    );
    assert_eq!(
        reader.volumes[0].forest_census().unwrap().slot_trees,
        writer.volumes[0].forest_census().unwrap().slot_trees,
        "the reader knows every slot tree the writer minted"
    );
    for v in &writer.volumes {
        v.shutdown().await.unwrap();
    }
}

/// Review round 3, Issue 18: a NON-WRITER open (`-o ro`) of a forest volume
/// whose writer's journal window holds records of slot trees tree 0 does
/// not yet name performs ZERO device writes. Before the fix the open-time
/// replay MINTED every unknown slot — an extent claim plus a `write_node`
/// of an empty root — from a mount that may not write, onto the very
/// extents the live writer's unpublished roots occupy. The chosen posture
/// is the S5 bounded-stale contract: the reader SKIPS those records (it
/// sees the slot at the poll after the writer publishes), never routes
/// them into RAM-only trees the epoch drop pass would tear from under it.
///
/// **What the reader answers meanwhile (Issue 23):** its view is partial
/// BY SLOT — the slot trees tree 0 named at its last poll serve (their
/// window records included), a slot it does not hold answers as absent
/// everywhere at once: `lookup` ENOENT, `getattr` ENOENT, and `readdir`
/// OMITS the name (a dentry lives in the parent's slot and names a child
/// in the child's — the one cross-slot edge; the routed pager lists a
/// child iff the mount holds the child's slot tree, which is what its
/// `lookup` answers). The pin asserts both faces agree name by name, and
/// that every skipped record appears at the first poll after the writer
/// publishes — the whole slot at once, inside the published staleness
/// bound. Skipping the WHOLE window was rejected: the `writer_claim`
/// heartbeat is never checkpointed per beat, and the probes and the
/// reader's guard rows read it from the window.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reader_of_a_forest_with_unpublished_slots_in_the_window_writes_nothing() {
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
        }
    }
    // Park the writer's cadence: every device write below is one this
    // test drives, so the global write gauges are the reader's alone.
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let _cleanup = Cleanup;
    let dir = tempfile::tempdir().unwrap();
    let uris = stamped_forest_set(dir.path()).await;
    let writer = open_routed_meta_set(&uris).await.expect("writer");
    let d = writer
        .create(ROOT_INO, "d", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    // A few slots minted AND published (the checkpoint names their roots).
    let mut published = Vec::new();
    for i in 0..4 {
        published.push(
            writer
                .create(d, &format!("pub{i}"), libc::S_IFREG | 0o644, 0, 0)
                .await
                .unwrap()
                .ino,
        );
    }
    for v in &writer.volumes {
        v.checkpoint_now().await.unwrap();
    }
    let published_trees = writer.volumes[0].forest_census().unwrap().slot_trees;
    // Then many more mints, UNPUBLISHED: fresh rotor slots whose roots
    // only the writer's RAM knows — their records sit in the window.
    let mut unpublished = Vec::new();
    for i in 0..(2 * MINT_SPREAD as u32) {
        unpublished.push(
            writer
                .create(d, &format!("win{i:03}"), libc::S_IFREG | 0o644, 0, 0)
                .await
                .unwrap()
                .ino,
        );
    }
    assert!(
        writer.volumes[0].forest_census().unwrap().slot_trees > published_trees + 8,
        "the window holds records of slots tree 0 does not name"
    );
    writer.volumes[0].sync_device().await.unwrap();

    let minted0 = META_KV_FOREST_SLOT_TREES_MINTED.load(Ordering::Relaxed);
    let rewrite0 = META_KV_NODE_REWRITE_BYTES.load(Ordering::Relaxed);
    let appends0 = META_KV_NODE_APPENDS.load(Ordering::Relaxed);
    let reader = open_routed_meta_set_read_only(&uris).await.expect("reader");
    assert_eq!(
        META_KV_FOREST_SLOT_TREES_MINTED.load(Ordering::Relaxed),
        minted0,
        "a reader mints no slot tree"
    );
    assert_eq!(
        META_KV_NODE_REWRITE_BYTES.load(Ordering::Relaxed),
        rewrite0,
        "a reader writes no node image"
    );
    assert_eq!(
        META_KV_NODE_APPENDS.load(Ordering::Relaxed),
        appends0,
        "a reader appends no frame"
    );
    let rv = &reader.volumes[0];
    assert_eq!(rv.forest_census().unwrap().minted, 0);
    assert_eq!(
        rv.forest_census().unwrap().slot_trees,
        published_trees,
        "the reader opened exactly the slot trees tree 0 names"
    );
    // The published population serves; so does every window record of a
    // slot the reader KNOWS (the rotor re-visits the four published
    // slots); a record of an UNPUBLISHED slot is not yet visible (bounded
    // staleness — never a mint, never an error).
    for (i, ino) in published.iter().enumerate() {
        assert_eq!(
            reader.lookup(d, &format!("pub{i}")).await.unwrap().ino,
            *ino
        );
    }
    let known: std::collections::HashSet<_> = rv
        .forest_roots()
        .into_iter()
        .map(|(slot, _)| slot)
        .collect();
    let mut skipped = 0;
    for (i, ino) in unpublished.iter().enumerate() {
        let name = format!("win{i:03}");
        let slot =
            squeezefs::meta_backend::kv::record::forest_slot_of_ino(writer.route_ino(*ino).1);
        let r = reader.lookup(d, &name).await;
        if known.contains(&slot) {
            assert_eq!(
                r.unwrap().ino,
                *ino,
                "{name}: a known slot's window record serves"
            );
        } else {
            assert!(
                r.is_err(),
                "{name}: a record of an unpublished slot is not served before the writer publishes"
            );
            skipped += 1;
        }
    }
    assert!(
        skipped > 8,
        "the window held records of many unpublished slots ({skipped})"
    );
    // The reader's PARTIAL window is a consistent snapshot of its own (review
    // round 3, Issue 23): a dentry lives in its PARENT's slot and names a
    // child in the child's — the one edge that crosses slots — so a
    // non-writer skips a dentry whose child's slot is unpublished together
    // with the child (the create has not happened for this mount yet, as
    // ONE unit) and `readdir` never lists a name `lookup` refuses.
    let mut listed = std::collections::HashSet::new();
    let mut cookie = 0u64;
    loop {
        let page = reader.readdir_stream(d, cookie, 64).await.unwrap();
        let Some((last, _)) = page.last() else {
            break;
        };
        cookie = *last;
        for (_, e) in &page {
            listed.insert(e.name.clone());
        }
    }
    for i in 0..published.len() {
        assert!(listed.contains(&format!("pub{i}")));
    }
    for (i, ino) in unpublished.iter().enumerate() {
        let name = format!("win{i:03}");
        let slot =
            squeezefs::meta_backend::kv::record::forest_slot_of_ino(writer.route_ino(*ino).1);
        assert_eq!(
            listed.contains(&name),
            known.contains(&slot),
            "{name}: readdir lists a name iff lookup resolves it (child slot {slot} published: \
             {})",
            known.contains(&slot)
        );
    }
    // The other non-writer entry (`open_probe` — `squeezefs status`
    // against a live writer; `open_co_writer` and `open_peer_owned` take
    // the SAME `OpenPosture::NonWriter` arm, chosen at the call site):
    // the same replay, the same zero writes, the same skips.
    let skips0 = META_KV_FOREST_READER_WINDOW_SKIPS.load(Ordering::Relaxed);
    let probe = KvMetaBackend::open_probe(std::path::Path::new(&uris[0]))
        .await
        .expect("probe");
    assert_eq!(
        META_KV_FOREST_SLOT_TREES_MINTED.load(Ordering::Relaxed),
        minted0,
        "a probe mints no slot tree"
    );
    assert_eq!(
        META_KV_NODE_REWRITE_BYTES.load(Ordering::Relaxed),
        rewrite0,
        "a probe writes no node image"
    );
    assert_eq!(
        META_KV_NODE_APPENDS.load(Ordering::Relaxed),
        appends0,
        "a probe appends no frame"
    );
    assert_eq!(probe.forest_census().unwrap().minted, 0);
    assert_eq!(probe.forest_census().unwrap().slot_trees, published_trees);
    assert!(
        META_KV_FOREST_READER_WINDOW_SKIPS.load(Ordering::Relaxed) > skips0,
        "the probe's replay counted the window records it skipped"
    );
    drop(probe);
    // The writer publishes; the reader's next poll adopts the roots and
    // serves every window record — still without a mint.
    for v in &reader.volumes {
        v.arm_reader_revalidation(None).unwrap();
    }
    for v in &writer.volumes {
        v.checkpoint_now().await.unwrap();
    }
    let poller = RevalidationPoller::new(1);
    for (_, res) in poller
        .poll_set_at(&reader.volumes, std::time::Instant::now())
        .await
    {
        assert!(res.expect("poll").advanced);
    }
    for (i, ino) in unpublished.iter().enumerate() {
        assert_eq!(
            reader.lookup(d, &format!("win{i:03}")).await.unwrap().ino,
            *ino
        );
    }
    assert_eq!(
        rv.forest_census().unwrap().minted,
        0,
        "still no mint on the reader"
    );
    for v in &writer.volumes {
        v.shutdown().await.unwrap();
    }
}

/// The heap-full-with-unpublished-mints crash image (review round 3,
/// Issues 19 and 24): every rotor slot minted and published while the heap
/// has room, the heap driven to the growth floor with `mints + 3` extents
/// parked out of the free list, then — every publication deferred by the
/// seam — the parked extents released and `mints` NEW slot trees minted by
/// journaled commits (block references owned by inos of slots no rotor
/// reaches), the room drained back to the reserve exactly and landed: each
/// mint's bitmap bit durable, its root unpublished, its records in the
/// window by the unpublished-root floor. Returns the crash copy, the
/// reference tag, the slot-tree count before the mints, the reserve, and
/// the live writer set (its flock stays on the original).
struct HeapFullCrash {
    crash: NamedTempFile,
    tag: u64,
    trees_before: u64,
    reserve: u64,
    routed: Arc<squeezefs::meta_backend::RoutedMetaBackend>,
}

async fn heap_full_crash_with_unpublished_mints(
    dir: &std::path::Path,
    mints: usize,
) -> HeapFullCrash {
    use squeezefs::meta_backend::kv::alloc_ext::compaction_reserve_extents;
    let uris = stamped_forest_set(dir).await;
    let routed = open_routed_meta_set(&uris).await.expect("open");
    let vol = &routed.volumes[0];
    let d = routed
        .create(ROOT_INO, "d", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    // Every rotor slot minted and published while the heap has room.
    for i in 0..(2 * MINT_SPREAD as u32) {
        routed
            .create(d, &format!("f{i:03}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
    }
    vol.checkpoint_now().await.unwrap();

    // Drive the heap to the floor FIRST, with the mints' extents (plus a
    // margin for the commits' own leaves) parked out of the free list so
    // ENOSPC arrives with exactly that much room in hand: creates with
    // payloads until the growth floor refuses one (the fill's own
    // ring-full cycles publish every rotor slot).
    let alloc = Arc::clone(vol.allocator());
    let reserve = compaction_reserve_extents(vol.superblock().total_extents());
    let held: Vec<u64> = (0..mints + 3)
        .map(|_| alloc.claim_internal().expect("park an extent"))
        .collect();
    let mut i = 0u32;
    loop {
        let r = async {
            let f = routed
                .create(d, &format!("fill{i:05}"), libc::S_IFREG | 0o644, 0, 0)
                .await?;
            routed
                .setxattr(f.ino, "user.payload", &vec![0x5a; 12 * 1024])
                .await
        }
        .await;
        i += 1;
        match r {
            Ok(()) => {
                // The cadence is parked: cycle for ring room by hand.
                if i.is_multiple_of(8) {
                    vol.checkpoint_now().await.unwrap();
                }
            }
            Err(e) if e.to_errno() == libc::ENOSPC => break,
            Err(e) => panic!("fill: {e:?}"),
        }
        assert!(i < 100_000, "a 24 MiB heap absorbed 100k payloads");
    }
    assert!(vol.heap_full(), "the refused create latched the posture");
    vol.checkpoint_now().await.unwrap();
    let published = vol.forest_census().unwrap().control_records;

    // From here every publication is deferred (the seam). The parked
    // extents come back, `mints` more slot trees are minted by journaled
    // commits from that room (the runtime mint is USER growth), then the
    // room is taken back down to the reserve and landed.
    squeezefs::meta_backend::kv::backend::TEST_FOREST_PUBLISH_DEFER
        .store(u32::MAX, Ordering::SeqCst);
    for ext in held {
        alloc.release_unpublished(ext);
    }
    let tag = squeezefs::meta_backend::kv::block_refs::volume_tag("vol-0011223344556677");
    let trees_before = vol.forest_census().unwrap().slot_trees;
    for m in 0..mints {
        let owner = guest_local_ino(9_000 + m as u16, 1);
        let reference = BlockRef {
            vol_tag: tag,
            block_idx: 7 + m as u64,
            owner_ino: owner,
            block_index: 0,
        };
        vol.commit_block_refs(owner, &[BlockRefOp::taken(reference)])
            .await
            .expect("the mint has room");
        assert_eq!(vol.block_ref_count(tag, 7 + m as u64).await.unwrap(), 1);
    }
    assert_eq!(
        vol.forest_census().unwrap().slot_trees,
        trees_before + mints as u64
    );
    while alloc.free_extents() > reserve {
        alloc.claim_internal().expect("drain to the reserve");
    }
    vol.checkpoint_now().await.unwrap();
    assert_eq!(
        vol.forest_census().unwrap().control_records,
        published,
        "the late mints are still unpublished"
    );
    assert_eq!(
        alloc.free_extents(),
        reserve,
        "the heap sits at the reserve"
    );
    vol.sync_device().await.unwrap();
    // Crash: the image as it stands, reopened elsewhere (the writer keeps
    // its flock on the original).
    let crash = NamedTempFile::new().unwrap();
    std::fs::copy(&uris[0], crash.path()).unwrap();
    squeezefs::meta_backend::kv::backend::TEST_FOREST_PUBLISH_DEFER.store(0, Ordering::SeqCst);
    HeapFullCrash {
        crash,
        tag,
        trees_before,
        reserve,
        routed,
    }
}

/// Review round 3, Issue 19: a forest volume in the `heap_full` posture
/// that crashes with an UNPUBLISHED slot in its window (the publication
/// deferred, the mint's extent durable in the bitmap) REMOUNTS. The replay
/// re-mints the slot tree — a RECOVERY act — from the compaction reserve
/// (`claim_internal`, the flat bit-9/16 mount-time mints' class), never
/// the user class the runtime mint uses (refused at the growth floor —
/// which on this volume refused the MOUNT itself, every mount, for ever).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_heap_full_forest_with_an_unpublished_mint_in_the_window_remounts() {
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
            squeezefs::meta_backend::kv::backend::TEST_FOREST_PUBLISH_DEFER
                .store(0, Ordering::SeqCst);
        }
    }
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let _cleanup = Cleanup;
    let dir = tempfile::tempdir().unwrap();
    let fx = heap_full_crash_with_unpublished_mints(dir.path(), 1).await;

    let again = KvMetaBackend::open(fx.crash.path())
        .await
        .expect("a heap-full forest with an unpublished mint in its window REMOUNTS");
    assert_eq!(
        again.block_ref_count(fx.tag, 7).await.unwrap(),
        1,
        "the window's record folded into the re-minted slot tree"
    );
    assert_eq!(
        again.forest_census().unwrap().slot_trees,
        fx.trees_before + 1
    );
    again.shutdown().await.unwrap();
    for v in &fx.routed.volumes {
        v.shutdown().await.unwrap();
    }
}

/// Review round 3, Issue 24: the writer's replay re-mints EVERY slot tree
/// the window holds records for and tree 0 does not name — one
/// recovery-class extent each, the originals orphaned in the bitmap until
/// the hygiene sweep — so a crash behind a deferred publication with MORE
/// unpublished mints than the compaction reserve holds (`reserve + 1`
/// here; a first-touch burst is bounded by `MINT_SPREAD` rotor slots plus
/// the explicitly targeted ones, against a reserve of `max(8, 2 %)` of the
/// heap) cannot mount from the reserve. The refusal is the SPACE class
/// (ENOSPC — never the corruption class) and NAMES the count: how many
/// slot trees the window needs, what the heap holds against the reserve,
/// and where the orphans went. Adopting the orphaned root extents at
/// replay is owed (note §7) — it needs the unreachability proof fsck's C6
/// census gives, not a mount-time guess.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_heap_full_forest_with_more_unpublished_mints_than_the_reserve_refuses_by_count() {
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
            squeezefs::meta_backend::kv::backend::TEST_FOREST_PUBLISH_DEFER
                .store(0, Ordering::SeqCst);
        }
    }
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let _cleanup = Cleanup;
    let dir = tempfile::tempdir().unwrap();
    let probe = heap_full_crash_with_unpublished_mints(dir.path(), 1).await;
    let mints = probe.reserve as usize + 1;
    for v in &probe.routed.volumes {
        v.shutdown().await.unwrap();
    }
    drop(probe);
    let dir2 = tempfile::tempdir().unwrap();
    let fx = heap_full_crash_with_unpublished_mints(dir2.path(), mints).await;

    let err = KvMetaBackend::open(fx.crash.path())
        .await
        .expect_err("the reserve cannot re-mint reserve + 1 slot trees");
    let text = err.to_string();
    assert!(
        text.contains(&format!("EACH of the {mints} slot tree(s)")),
        "the refusal names the count of slot trees the window needs: {text}"
    );
    assert!(
        text.contains("orphaned in the bitmap"),
        "the refusal names where the originals went: {text}"
    );
    assert!(
        !text.starts_with("corrupt KV encoding"),
        "a space refusal is never the corruption class: {text}"
    );
    match &err {
        squeezefs::meta_backend::kv::KvError::Io(inner) => {
            assert_eq!(inner.to_errno(), libc::ENOSPC, "{inner:?}")
        }
        other => panic!("the refusal is the space class, got {other:?}"),
    }
    for v in &fx.routed.volumes {
        v.shutdown().await.unwrap();
    }
}
