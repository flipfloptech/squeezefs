//! Write-commit-economy campaign (2026-07-30) — the **layout delta
//! record** fold contracts (lever 2 of
//! `.benchmarks/2026-07-30-write-commit-economy.md`).
//!
//! The conviction being fixed: every block publish re-serialized the
//! WHOLE layout value into its journal entry and node writeback —
//! O(file_size) meta bytes per 4 MiB published block (18.2 KiB mean
//! journal entry field-wide; `.benchmarks/2026-07-30-meta-plane-writes.md`
//! §3). The fix: a `Delta`-kind record on the layout xattr key carrying
//! only the publish batch's `(block → key)` inserts plus tiny absolute
//! non-map fields; the §4.2 fold algebra — the ONE function shared by
//! point lookup, bset merge, compaction, and journal replay —
//! reconstructs the full value.
//!
//! RED at the contract commit: `fold_newest_first` / `fold_forward`
//! reject layout-delta payloads (`InodeDelta::decode` refuses the
//! magic), so every fold contract below fails until the fold branch
//! lands. The refusal contracts (indirect/JSON bases) and the
//! tombstone/orphan algebra pins document behavior that must HOLD.

use bytes::Bytes;
use squeezefs::layout_wire::{LayoutDelta, LayoutMetadata, LAYOUT_DELTA_MAGIC};
use squeezefs::meta_backend::kv::record::{
    compact_fold, fold_forward, fold_newest_first, xattr_key, Folded, FoldedHead, Record,
    RecordKind, XattrValue,
};
use std::collections::HashMap;

fn key() -> Vec<u8> {
    xattr_key(7, 0x00AB_CDEF_1234, 0).to_vec()
}

/// A striped base layout with blocks `0..n` mapped.
fn base_layout(n: u32, size: u64) -> LayoutMetadata {
    let mut map = HashMap::new();
    for b in 0..n {
        map.insert(b, format!("oss0://{}", b as u64 * 4096));
    }
    LayoutMetadata {
        file_type: "striped".into(),
        size,
        block_map_id: Some("block_map_7".into()),
        block_prefix: None,
        file_id: None,
        data_key: None,
        block_map: Some(map),
    }
}

fn base_put(n: u32, size: u64, seq: u64) -> Record {
    let value = bincode::serialize(&base_layout(n, size)).expect("serialize");
    Record::put(
        key(),
        seq,
        XattrValue::encode_parts(b"layout", &value).expect("encode"),
    )
}

/// A publish-batch delta appending blocks `[from, to)` with a size floor
/// at the batch end.
fn publish_delta(from: u32, to: u32, size: u64, seq: u64) -> Record {
    let entries: Vec<(u32, String)> = (from..to)
        .map(|b| (b, format!("oss0://{}", b as u64 * 4096)))
        .collect();
    let d = LayoutDelta {
        file_type: "striped".into(),
        size,
        block_map_id: Some("block_map_7".into()),
        block_prefix: None,
        file_id: None,
        data_key: None,
        entries,
    };
    Record {
        key: key(),
        seq,
        kind: RecordKind::Delta,
        value: d.encode(),
    }
}

fn folded_layout(f: &Folded<'_>) -> LayoutMetadata {
    let Folded::Put { value, .. } = f else {
        panic!("expected a live fold, got {f:?}");
    };
    let x = XattrValue::decode(value).expect("folded value must be a valid XattrValue");
    assert_eq!(x.name, b"layout");
    bincode::deserialize::<LayoutMetadata>(&x.value)
        .expect("folded layout must decode as bincode LayoutMetadata")
}

fn assert_layout_eq(got: &LayoutMetadata, want: &LayoutMetadata) {
    assert_eq!(got.file_type, want.file_type);
    assert_eq!(got.size, want.size);
    assert_eq!(got.block_map_id, want.block_map_id);
    assert_eq!(got.block_prefix, want.block_prefix);
    assert_eq!(got.file_id, want.file_id);
    assert_eq!(got.data_key, want.data_key);
    assert_eq!(got.block_map, want.block_map);
}

// ---------------------------------------------------------------------------
// Fold contracts (RED until the fold branch lands)
// ---------------------------------------------------------------------------

/// One delta onto a base Put folds to the merged layout.
#[test]
fn layout_delta_folds_onto_base_put() {
    let recs = [base_put(4, 4 * 4096, 10), publish_delta(4, 8, 8 * 4096, 11)];
    let folded = fold_newest_first(recs.iter().rev().map(|r| r.record_ref()))
        .expect("layout delta must fold, not error");
    let got = folded_layout(&folded);
    assert_layout_eq(&got, &base_layout(8, 8 * 4096));
}

/// A delta CHAIN folds in ascending seq order: entries accumulate,
/// absolute fields last-writer-win.
#[test]
fn layout_delta_chain_accumulates_entries_and_lww_fields() {
    let recs = [
        base_put(2, 2 * 4096, 10),
        publish_delta(2, 5, 5 * 4096, 11),
        publish_delta(5, 6, 6 * 4096, 12),
        publish_delta(6, 9, 9 * 4096, 13),
    ];
    let folded = fold_newest_first(recs.iter().rev().map(|r| r.record_ref()))
        .expect("layout delta chain must fold");
    let got = folded_layout(&folded);
    assert_layout_eq(&got, &base_layout(9, 9 * 4096));
    // Folding twice is deterministic at the byte level (the digest-walk
    // and replay-twice requirement).
    let f1 = fold_newest_first(recs.iter().rev().map(|r| r.record_ref())).expect("fold 1");
    let f2 = fold_newest_first(recs.iter().rev().map(|r| r.record_ref())).expect("fold 2");
    assert_eq!(
        f1.live_value().expect("live"),
        f2.live_value().expect("live"),
        "fold output must be byte-deterministic"
    );
}

/// SIZE-NEVER-LEADS-DATA, structural face: the delta record carries the
/// size floor AND its batch's map entries in ONE record value — a fold
/// can never observe the size without the entries (and one tx = one
/// checksummed journal entry makes the durable face identical).
#[test]
fn delta_size_and_entries_are_one_record() {
    let rec = publish_delta(4, 8, 8 * 4096, 11);
    let d = LayoutDelta::decode(&rec.value).expect("decode");
    assert_eq!(d.size, 8 * 4096);
    assert_eq!(d.entries.len(), 4, "the size floor rides its data's map");
    assert_eq!(
        u16::from_le_bytes([rec.value[0], rec.value[1]]),
        LAYOUT_DELTA_MAGIC
    );
}

/// `fold_forward` (the D7 overlay-head step) matches the from-scratch
/// fold over layout histories — the same equivalence theorem the inode
/// deltas pin, extended to the layout class.
#[test]
fn fold_forward_matches_from_scratch_on_layout_chains() {
    let recs = [
        base_put(1, 4096, 5),
        publish_delta(1, 3, 3 * 4096, 6),
        publish_delta(3, 4, 4 * 4096, 7),
    ];
    let mut head = FoldedHead::Absent;
    for r in &recs {
        head = fold_forward(&head, r.kind, &Bytes::from(r.value.clone()))
            .expect("layout history must fold forward");
    }
    let from_scratch = fold_newest_first(recs.iter().rev().map(|r| r.record_ref())).expect("fold");
    match (&head, &from_scratch) {
        (FoldedHead::Live { value, .. }, Folded::Put { value: v, .. }) => {
            assert_eq!(
                value.as_ref(),
                v.as_ref(),
                "fold-forward head must byte-equal the from-scratch fold"
            );
        }
        (h, f) => panic!("divergent outcomes: head {h:?} vs from-scratch {f:?}"),
    }
}

/// Randomized equivalence: any mix of layout Puts, layout deltas, and
/// tombstones folds identically incremental vs from-scratch (the R7
/// property, layout class).
#[test]
fn fold_forward_equivalence_property_layout_class() {
    use proptest::prelude::*;
    use proptest::strategy::{Strategy, ValueTree};
    let step = prop_oneof![
        (0u32..12, 0u64..1 << 20).prop_map(|(n, size)| {
            let v = bincode::serialize(&base_layout(n, size)).unwrap();
            (
                RecordKind::Put,
                XattrValue::encode_parts(b"layout", &v).unwrap(),
            )
        }),
        (0u32..12, 1u32..4, 0u64..1 << 20).prop_map(|(from, len, size)| {
            let d = LayoutDelta {
                file_type: "striped".into(),
                size,
                block_map_id: Some("block_map_7".into()),
                block_prefix: None,
                file_id: None,
                data_key: None,
                entries: (from..from + len)
                    .map(|b| (b, format!("oss0://{}", b as u64 * 4096)))
                    .collect(),
            };
            (RecordKind::Delta, d.encode())
        }),
        Just((RecordKind::Delete, Vec::new())),
    ];
    let histories = proptest::collection::vec(step, 1..24);
    let mut runner = proptest::test_runner::TestRunner::default();
    for _ in 0..256 {
        let steps = histories
            .new_tree(&mut runner)
            .expect("gen")
            .current()
            .to_vec();
        let records: Vec<Record> = steps
            .iter()
            .enumerate()
            .map(|(i, (kind, value))| Record {
                key: key(),
                seq: i as u64 + 1,
                kind: *kind,
                value: value.clone(),
            })
            .collect();
        let mut head = FoldedHead::Absent;
        for r in &records {
            head = fold_forward(&head, r.kind, &Bytes::from(r.value.clone()))
                .expect("valid layout history never errors");
        }
        let from_scratch = fold_newest_first(records.iter().rev().map(|r| r.record_ref()))
            .expect("valid layout history never errors");
        match (&head, &from_scratch) {
            (FoldedHead::Live { value, .. }, Folded::Put { value: v, .. }) => {
                assert_eq!(value.as_ref(), v.as_ref());
            }
            (FoldedHead::Tombstone, Folded::Tombstone { .. }) => {}
            (FoldedHead::Absent, Folded::Absent) => {}
            (h, f) => panic!("divergence: {h:?} vs {f:?}"),
        }
    }
}

/// Compaction materializes a layout chain into ONE folded Put — the
/// node-writeback economy face (chains collapse on the background
/// cadence, never accumulate on disk forever).
#[test]
fn compact_fold_materializes_layout_chain_into_one_put() {
    let recs = [
        base_put(2, 2 * 4096, 10),
        publish_delta(2, 6, 6 * 4096, 11),
        publish_delta(6, 7, 7 * 4096, 12),
    ];
    let refs: Vec<_> = recs.iter().rev().map(|r| r.record_ref()).collect();
    let out = compact_fold(&refs, 0)
        .expect("compaction fold must handle layout deltas")
        .expect("live key compacts to a Put");
    assert_eq!(out.kind, RecordKind::Put);
    let x = XattrValue::decode(&out.value).expect("compacted value is an XattrValue");
    let got: LayoutMetadata = bincode::deserialize(&x.value).expect("layout");
    assert_layout_eq(&got, &base_layout(7, 7 * 4096));
}

// ---------------------------------------------------------------------------
// Algebra pins (must hold before AND after the fold branch)
// ---------------------------------------------------------------------------

/// A tombstone shadows older layout records and discards newer-than-Put
/// deltas per §4.2 — destroy semantics are untouched by the new class.
#[test]
fn tombstone_shadows_layout_chain() {
    let recs = [
        base_put(2, 2 * 4096, 10),
        Record::delete(key(), 11),
        publish_delta(2, 3, 3 * 4096, 12),
    ];
    // Delta newer than the tombstone, no newer Put: the scan hits the
    // Delete and DISCARDS collected deltas (§4.2 "the tombstone is the
    // underlying truth") — a tombstone outcome, never a decode of the
    // orphaned layout delta.
    let folded = fold_newest_first(recs.iter().rev().map(|r| r.record_ref())).expect("fold");
    assert!(
        matches!(folded, Folded::Tombstone { .. }),
        "delta-after-delete folds to the tombstone (collected deltas discarded), got {folded:?}"
    );
    // Delete newest: tombstone.
    let recs2 = [base_put(2, 2 * 4096, 10), Record::delete(key(), 11)];
    let folded2 = fold_newest_first(recs2.iter().rev().map(|r| r.record_ref())).expect("fold");
    assert!(matches!(folded2, Folded::Tombstone { .. }));
}

/// A layout delta whose base is an `indirect:` layout is REFUSED LOUD at
/// fold time (the writer eligibility ladder makes it unreachable; a hit
/// is genuine corruption, never a guess).
#[test]
fn layout_delta_onto_indirect_base_fails_loud() {
    let indirect = LayoutMetadata {
        file_type: "striped".into(),
        size: 4096,
        block_map_id: Some("indirect:oss0://8192".into()),
        block_prefix: None,
        file_id: None,
        data_key: None,
        block_map: None,
    };
    let v = bincode::serialize(&indirect).unwrap();
    let recs = [
        Record::put(
            key(),
            10,
            XattrValue::encode_parts(b"layout", &v).expect("encode"),
        ),
        publish_delta(0, 1, 4096, 11),
    ];
    let err = fold_newest_first(recs.iter().rev().map(|r| r.record_ref()))
        .expect_err("delta onto an indirect base must fail loud");
    let msg = format!("{err}");
    assert!(
        msg.contains("indirect"),
        "the refusal must name the indirect base, got: {msg}"
    );
}
