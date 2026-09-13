//! Fuzz the **slot-tree record codecs** of the symmetric-metadata forest
//! (`src/meta_backend/kv/record.rs` — `docs/design-symmetric-metadata.md`
//! §5.2.1, incompat bit 17) and the **kvmap block-map tree** codecs this
//! target absorbed (`src/meta_backend/kv/block_map.rs` —
//! `docs/design-kvmap-block-map-tree.md` §2/§12; the retired `kvmap_record`
//! target verbatim, below the forest section).
//!
//! The forest key is the ONE codec every record of a forest volume passes
//! through: `forest_key(kind, legacy)` frames a shipped per-kind key as
//! `ino ‖ kind ‖ rest` (the by-block refs family as `0x06 ‖ …`), and
//! `split_forest_key` / `forest_key_kind` / `forest_key_slot` read it back
//! at every lookup, range walk, replay and fsck census. A frame that
//! decodes WRONG routes a record to a foreign slot tree — a file whose
//! `stat` and `layout` land in different trees, a reference counted on
//! the wrong owner. The laws (spec §11 TEST-4 + the design's round-3
//! Issue 6):
//!
//! 1. **Total.** Every decoder answers `Ok` or a typed error on arbitrary
//!    bytes — never a panic (a slot-tree leaf is checksummed but a torn
//!    write or a writer bug produces valid-looking bytes).
//! 2. **Exact.** `split_forest_key ∘ forest_key = id` over the encoder's
//!    whole domain, and whatever `split_forest_key` accepts re-encodes to
//!    the SAME bytes — the parser accepts nothing the encoder cannot
//!    produce.
//! 3. **A kind byte is never another tree's id.** Only the five content
//!    kinds (1/2/3/6/7) frame or decode; the interior marker (0), the
//!    reserved ids (4/5), tree 0 (8) and the shared index (9) refuse in
//!    BOTH directions.
//! 4. **One slot namespace, both directions.** The routing ino (the
//!    leading ino; the OWNER ino for refs) names a slot `≤ FOREST_SLOT_MAX`
//!    or the key is refused at the encoder AND the decoder.
//! 5. **Slot-tree interior journal keys** (`slot: u32 BE ‖ separator`)
//!    split back to their `(slot, separator)`; a key too short to carry a
//!    separator refuses.
#![no_main]

use libfuzzer_sys::fuzz_target;
use squeezefs::meta_backend::kv::block_map::{
    block_map_key, decode_block_map_key, decode_block_map_value, index_range_from,
    parse_kvmap_head, KvmapHead, MapEntry, BLOCK_MAP_KEY_LEN, RUN_LEN_MAX,
};
use squeezefs::meta_backend::kv::block_refs::{block_ref_key, BLOCK_REF_KEY_LEN};
use squeezefs::meta_backend::kv::forest::{
    interior_journal_key, split_interior_journal_key, INTERIOR_JOURNAL_SLOT_LEN,
};
use squeezefs::meta_backend::kv::record::{
    dentry_key, forest_key, forest_key_kind, forest_key_slot, forest_slot_of_ino, inode_key,
    is_slot_tree_kind, split_forest_key, xattr_key, FOREST_SLOT_MAX, TREE_BLOCK_MAP,
    TREE_BLOCK_REFS, TREE_DENTRIES, TREE_INODES, TREE_XATTRS,
};

fuzz_target!(|data: &[u8]| {
    // --- the forest key: decode side ------------------------------------------
    let kind = forest_key_kind(data);
    let split = split_forest_key(data);
    let slot = forest_key_slot(data);
    assert_eq!(
        kind.is_ok(),
        split.is_ok(),
        "kind and split agree on admissibility"
    );
    if let Ok((k, legacy)) = &split {
        assert!(is_slot_tree_kind(*k), "a decoded kind is a content kind");
        assert_eq!(kind.as_ref().ok(), Some(k));
        let re = forest_key(*k, legacy).expect("a decoded key re-encodes");
        assert_eq!(&re[..], data, "the forest key is byte-exact");
        let s = slot.expect("a decoded key routes");
        assert!(
            s <= FOREST_SLOT_MAX,
            "a routed slot is inside the namespace"
        );
        let route_off = if *k == TREE_BLOCK_REFS { 16 } else { 0 };
        let ino = u64::from_be_bytes(legacy[route_off..route_off + 8].try_into().unwrap());
        assert_eq!(
            s,
            forest_slot_of_ino(ino),
            "the slot is the routing ino's top bits"
        );
    } else {
        assert!(slot.is_err(), "an undecodable key routes nowhere");
    }

    // --- the forest key: encode side, over the encoder's whole domain ----------
    if data.len() >= 1 + 8 + 8 + 8 + 4 {
        let kind = data[0];
        let a = u64::from_be_bytes(data[1..9].try_into().unwrap());
        let b = u64::from_be_bytes(data[9..17].try_into().unwrap());
        let c = u64::from_be_bytes(data[17..25].try_into().unwrap());
        let d = u32::from_be_bytes(data[25..29].try_into().unwrap());
        // Every kind's own legacy key, plus the wrong-length probe.
        let legacy: Option<Vec<u8>> = match kind {
            TREE_INODES => Some(inode_key(a).to_vec()),
            TREE_DENTRIES => Some(dentry_key(a, b & ((1 << 54) - 1), d as u8).to_vec()),
            TREE_XATTRS => Some(xattr_key(a, b & ((1 << 56) - 1), d as u8).to_vec()),
            TREE_BLOCK_MAP => block_map_key(a, d).ok().map(|k| k.to_vec()),
            TREE_BLOCK_REFS => Some(block_ref_key(b, c, a, d).to_vec()),
            _ => None,
        };
        match legacy {
            Some(legacy) => {
                let framed = forest_key(kind, &legacy);
                let in_namespace = forest_slot_of_ino(a) <= FOREST_SLOT_MAX;
                assert_eq!(
                    framed.is_ok(),
                    in_namespace,
                    "the encoder frames exactly the inos a slot names (kind {kind}, ino {a:#x})"
                );
                if let Ok(f) = framed {
                    assert_eq!(f.len(), legacy.len() + 1, "one kind byte");
                    let (k2, l2) = split_forest_key(&f).expect("a framed key splits");
                    assert_eq!((k2, &l2[..]), (kind, &legacy[..]), "round trip");
                    assert_eq!(
                        forest_key_slot(&f).expect("a framed key routes"),
                        forest_slot_of_ino(a)
                    );
                }
                // The wrong-length probe: one byte short / long refuses.
                assert!(forest_key(kind, &legacy[..legacy.len() - 1]).is_err());
                let mut long = legacy.clone();
                long.push(0);
                assert!(forest_key(kind, &long).is_err());
            }
            None => {
                // Not a content kind: nothing frames under it, whatever
                // the length (law 3 — interior marker, reserved ids, tree
                // 0, the shared index, and every byte above).
                for len in [8usize, 12, 16, BLOCK_REF_KEY_LEN] {
                    assert!(
                        forest_key(kind, &data[1..1 + len]).is_err(),
                        "kind {kind} is never a slot-tree kind"
                    );
                }
            }
        }
    }

    // --- slot-tree interior journal keys -----------------------------------------
    match split_interior_journal_key(data) {
        Ok((slot, sep)) => {
            assert!(!sep.is_empty(), "an interior separator is never empty");
            assert_eq!(
                interior_journal_key(slot, sep),
                data,
                "the interior journal key is byte-exact"
            );
        }
        Err(_) => assert!(
            data.len() <= INTERIOR_JOURNAL_SLOT_LEN,
            "only a key with no separator refuses"
        ),
    }

    // =========================================================================
    // The kvmap block-map tree (the absorbed `kvmap_record` target).
    // =========================================================================

    // --- the key ----------------------------------------------------------
    if let Ok((ino, idx)) = decode_block_map_key(data) {
        assert_ne!(
            idx,
            u32::MAX,
            "the reserved index must be refused, not decoded"
        );
        let re = block_map_key(ino, idx).expect("a decoded key re-encodes");
        assert_eq!(&re[..], data, "the key is byte-exact");
        // The range window for this ino contains its own key and stops
        // short of the next ino: the bound is the reserved index, raw.
        let (lo, hi) = index_range_from(ino, idx);
        assert!(
            lo <= re && re <= hi,
            "a key must sit inside its own range window"
        );
        assert!(
            decode_block_map_key(&hi).is_err(),
            "the end bound is a bound, never a key"
        );
    }
    if data.len() >= BLOCK_MAP_KEY_LEN {
        // The constructive direction on the same bytes: any (ino, idx)
        // pair except the reserved index encodes and decodes back.
        let ino = u64::from_be_bytes(data[0..8].try_into().unwrap());
        let idx = u32::from_be_bytes(data[8..12].try_into().unwrap());
        match block_map_key(ino, idx) {
            Ok(k) => assert_eq!(decode_block_map_key(&k).ok(), Some((ino, idx))),
            Err(_) => assert_eq!(idx, u32::MAX, "only the reserved index is refused"),
        }
    }

    // --- the value ----------------------------------------------------------
    if let Ok(entry) = decode_block_map_value(data) {
        let re = entry.encode();
        assert_eq!(re, data, "a decoded value re-encodes byte-identically");
        assert_eq!(
            entry.encoded_len(),
            re.len(),
            "encoded_len is the encode's length"
        );
        let run = entry.run_len();
        match &entry {
            MapEntry::Run { len, .. } | MapEntry::RunStamped { len, .. } => {
                assert!(
                    (2..=RUN_LEN_MAX).contains(len),
                    "run length outside A6's bound"
                );
                assert_eq!(run, *len);
            }
            MapEntry::Point { .. } | MapEntry::String(_) | MapEntry::PointStamped { .. } => {
                assert_eq!(run, 1);
            }
        }
        if let MapEntry::PointStamped { incarnation, .. } = &entry {
            assert_ne!(
                *incarnation, 0,
                "stamp 0 has a POINT spelling — must refuse"
            );
        }
        if let MapEntry::RunStamped {
            len,
            start_incarnation,
            ..
        } = &entry
        {
            assert_ne!(*start_incarnation, 0);
            assert!(
                start_incarnation.checked_add(u64::from(*len) - 1).is_some(),
                "a wrapping RUN2 span cannot have been emitted"
            );
        }
    }

    // --- the head sentinel --------------------------------------------------
    if let Ok(s) = std::str::from_utf8(data) {
        for candidate in [s.to_string(), format!("kvmap:{s}")] {
            if let Ok(head) = parse_kvmap_head(&candidate) {
                // Exactness covers the `gen:0` law by itself: a parse of
                // ";gen:0" would re-encode WITHOUT the segment and fail here
                // (gen 0 is ABSENT — `kvmap:1` legitimately parses to it).
                assert_eq!(head.encode(), candidate, "the head grammar is exact");
                assert!(
                    !candidate.contains(";gen:0"),
                    "an explicit gen:0 has no encoder — must refuse"
                );
                let again = parse_kvmap_head(&head.encode()).expect("a re-encoded head parses");
                assert_eq!(again, head);
            }
        }
    }
    // The constructive direction: every (sweep, gen) the encoder can
    // produce parses back — gen 0 is encoded as absence by law.
    if data.len() >= 13 {
        let sweep = (data[0] & 1 == 1).then(|| u32::from_le_bytes(data[1..5].try_into().unwrap()));
        let gen = u64::from_le_bytes(data[5..13].try_into().unwrap());
        let head = KvmapHead {
            sweep_cursor: sweep,
            gen,
        };
        let s = head.encode();
        assert_eq!(parse_kvmap_head(&s).expect("an encoded head parses"), head);
    }
});
