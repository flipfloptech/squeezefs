//! Bsets: sorted, xxh3-checksummed record batches — build / verify / binary
//! search / n-way merge (design §4.2, §4.3).
//!
//! A bset is the unit node appends and compaction outputs are written in.
//! Verification happens **once** at load (§4.3) — after [`BsetView::parse`]
//! succeeds, record access is infallible zero-copy slicing. Binary search
//! compares raw big-endian key bytes and never decodes values (§4.2).

use super::record::{Folded, Record, RecordRef};
use super::KvError;

/// Bset header magic (`"BSET"`).
pub const BSET_MAGIC: u32 = 0x4253_4554;
/// Current bset encoding version.
pub const BSET_VERSION: u16 = 1;
/// Fixed header length: `magic: u32 | version: u16 | reserved: u16 |
/// record_count: u32 | data_len: u32 | journal_seq_horizon: u64 |
/// checksum: u64`, little-endian.
pub const BSET_HEADER_LEN: usize = 32;

/// Serialize `records` into a checksummed bset image.
///
/// `records` must be non-empty and strictly ascending by `(key, seq)` —
/// duplicate `(key, seq)` pairs within one bset are a writer bug and are
/// rejected. `journal_seq_horizon` is the newest journal seq this bset
/// covers; the K2 torn-tail classifier branches on it (§4.5).
pub fn build_bset(_records: &[Record], _journal_seq_horizon: u64) -> Result<Vec<u8>, KvError> {
    todo!()
}

/// A parsed, verified, zero-copy view over a bset image.
///
/// Construction verifies the xxh3 checksum, bounds-checks every length field
/// against its container (§9), and validates strict `(key, seq)` ordering;
/// afterwards records are handed out as borrowed slices with no further
/// validation cost (§4.3: per-read checksum cost is zero).
pub struct BsetView<'a> {
    _buf: &'a [u8],
}

impl<'a> BsetView<'a> {
    /// Parse and verify a bset image. The buffer must be the exact
    /// `BSET_HEADER_LEN + data_len` bytes (callers slice off any node-side
    /// padding first).
    pub fn parse(_buf: &'a [u8]) -> Result<Self, KvError> {
        todo!()
    }

    /// Number of records.
    pub fn len(&self) -> usize {
        todo!()
    }

    pub fn is_empty(&self) -> bool {
        todo!()
    }

    /// The newest journal seq this bset covers (§4.5 classifier input).
    pub fn journal_seq_horizon(&self) -> u64 {
        todo!()
    }

    /// The `i`-th record (records are in strict `(key, seq)` ascending
    /// order). Panics on out-of-range `i`, like slice indexing.
    pub fn record(&self, _i: usize) -> RecordRef<'a> {
        todo!()
    }

    /// Binary search on raw key bytes — no decoding (§4.2): the index range
    /// of records whose key equals `key` (empty at the insertion point when
    /// absent). A multi-seq key yields a seq-ascending range.
    pub fn find(&self, _key: &[u8]) -> std::ops::Range<usize> {
        todo!()
    }

    /// Iterate records in `(key, seq)` order.
    pub fn iter(&self) -> BsetIter<'_, 'a> {
        todo!()
    }
}

/// Iterator over a [`BsetView`]'s records.
pub struct BsetIter<'v, 'a> {
    _view: &'v BsetView<'a>,
}

impl<'a> Iterator for BsetIter<'_, 'a> {
    type Item = RecordRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        todo!()
    }
}

/// N-way merge over bset views, `sources[0]` newest. Yields the record
/// multiset ordered by `(key asc, seq desc, source index asc)` — each key's
/// group comes out contiguous and newest-first, exactly the fold algebra's
/// input shape. Equal `(key, seq)` across sources (a node bset overlapping
/// the journal replay window) yields the newer source first; both survive
/// for the fold, which is idempotent over identical-effect records.
pub fn merge<'v, 'a>(_sources: &'v [BsetView<'a>]) -> MergeIter<'v, 'a> {
    todo!()
}

/// Iterator returned by [`merge`].
pub struct MergeIter<'v, 'a> {
    _sources: &'v [BsetView<'a>],
}

impl<'a> Iterator for MergeIter<'_, 'a> {
    type Item = RecordRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        todo!()
    }
}

/// Point lookup across sources (`sources[0]` newest): gather `key`'s records
/// from every view, order newest-first, and run the single fold algebra —
/// the same fold compaction and replay use (§4.2).
pub fn lookup<'a>(_sources: &[BsetView<'a>], _key: &[u8]) -> Result<Folded<'a>, KvError> {
    todo!()
}

/// Compact sources into one folded record per surviving key: n-way merge,
/// group by key, [`super::record::compact_fold`] each group under the §4.2
/// tombstone elision rule. The output is strictly key-ascending and
/// key-unique — directly buildable into a fresh bset.
pub fn compact(_sources: &[BsetView<'_>], _durable_tail: u64) -> Result<Vec<Record>, KvError> {
    todo!()
}

// ---------------------------------------------------------------------------
// Tests — build/verify/search/merge contracts (PR K1, tests-first).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::record::{
        assign_dentry_coll_seq, dentry_key, encode_readdir_cookie, inode_key, DentryValue,
        InodeDelta, InodeValue, RecordKind,
    };
    use super::*;
    use std::borrow::Cow;

    fn iv(seed: u64) -> InodeValue {
        InodeValue {
            mode: 0o100644,
            uid: seed as u32,
            gid: 0,
            nlink: 1,
            flags: 0,
            flags2: 0,
            size: seed * 7,
            atime: seed,
            mtime: seed,
            ctime: seed,
        }
    }

    fn put(ino: u64, seq: u64) -> Record {
        Record::put(inode_key(ino).to_vec(), seq, iv(seq).encode())
    }

    /// Recompute and restamp the checksum of a (possibly doctored) image so
    /// bounds/ordering validation is reached past the checksum gate.
    fn restamp_checksum(buf: &mut [u8]) {
        use xxhash_rust::xxh3::Xxh3;
        let mut h = Xxh3::new();
        h.update(&buf[..24]);
        h.update(&[0u8; 8]);
        h.update(&buf[32..]);
        let sum = h.digest();
        buf[24..32].copy_from_slice(&sum.to_le_bytes());
    }

    // -- build + parse roundtrip ----------------------------------------------

    #[test]
    fn bset_build_parse_roundtrip_preserves_records_and_header() {
        let records = vec![
            put(1, 4),
            // Same key, two seqs — legal across commits within one frozen delta.
            Record::delta(inode_key(1).to_vec(), 9, &InodeDelta::times(1, 2)),
            put(2, 5),
            Record::delete(inode_key(3).to_vec(), 6),
        ];
        let buf = build_bset(&records, 9).expect("build");
        assert_eq!(&buf[0..4], &BSET_MAGIC.to_le_bytes(), "header magic");

        let view = BsetView::parse(&buf).expect("parse verifies");
        assert_eq!(view.len(), 4);
        assert!(!view.is_empty());
        assert_eq!(view.journal_seq_horizon(), 9);
        let roundtripped: Vec<Record> = view.iter().map(|r| r.to_record()).collect();
        assert_eq!(roundtripped, records);
        assert_eq!(view.record(3).kind, RecordKind::Delete);
    }

    #[test]
    fn bset_build_rejects_empty_unsorted_and_duplicate_inputs() {
        assert!(
            matches!(build_bset(&[], 0), Err(KvError::Corrupt(_))),
            "an empty bset is a writer bug (freeze writes only dirty deltas)"
        );

        // Key order violated.
        let unsorted = vec![put(2, 1), put(1, 2)];
        assert!(matches!(build_bset(&unsorted, 2), Err(KvError::Corrupt(_))));

        // Same key must be seq-ascending.
        let seq_desc = vec![
            put(1, 5),
            Record::delta(inode_key(1).to_vec(), 3, &InodeDelta::times(1, 2)),
        ];
        assert!(matches!(build_bset(&seq_desc, 5), Err(KvError::Corrupt(_))));

        // Duplicate (key, seq) within one bset is a writer bug.
        let dup = vec![put(1, 5), put(1, 5)];
        assert!(matches!(build_bset(&dup, 5), Err(KvError::Corrupt(_))));
    }

    // -- verification (§4.3): checksum + bounds + order -----------------------

    #[test]
    fn bset_parse_detects_corruption_anywhere_via_xxh3() {
        let buf = build_bset(&[put(1, 1), put(2, 2)], 2).expect("build");
        // Flip one byte in the header (past the magic/version gate), in the
        // record area, and at the very end — every flip must be caught.
        for &pos in &[9usize, BSET_HEADER_LEN + 3, buf.len() - 1] {
            let mut bad = buf.clone();
            bad[pos] ^= 0x40;
            let err = BsetView::parse(&bad)
                .err()
                .unwrap_or_else(|| panic!("flipped byte at {pos} must fail parse"));
            assert!(
                matches!(err, KvError::ChecksumMismatch { .. }),
                "flipped byte at {pos} must fail the checksum, got {err:?}"
            );
        }
        // Structural gates come first and stay typed as corruption.
        let mut bad_magic = buf.clone();
        bad_magic[0] ^= 0xFF;
        assert!(matches!(
            BsetView::parse(&bad_magic),
            Err(KvError::Corrupt(_))
        ));
        let mut bad_version = buf.clone();
        bad_version[4] = 0xEE;
        assert!(matches!(
            BsetView::parse(&bad_version),
            Err(KvError::Corrupt(_))
        ));
        // Truncations.
        assert!(matches!(
            BsetView::parse(&buf[..16]),
            Err(KvError::Corrupt(_))
        ));
        assert!(matches!(
            BsetView::parse(&buf[..buf.len() - 1]),
            Err(KvError::Corrupt(_))
        ));
        // Trailing bytes beyond data_len (callers must slice exactly).
        let mut padded = buf.clone();
        padded.extend_from_slice(&[0u8; 8]);
        assert!(matches!(BsetView::parse(&padded), Err(KvError::Corrupt(_))));
    }

    #[test]
    fn bset_parse_bounds_checks_every_length_field() {
        let buf = build_bset(&[put(1, 1), put(2, 2)], 2).expect("build");

        // A record's val_len lying past the data area — checksum restamped so
        // the §9 bounds check itself must catch it.
        let mut lying = buf.clone();
        let val_len_off = BSET_HEADER_LEN + 11;
        lying[val_len_off..val_len_off + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        restamp_checksum(&mut lying);
        assert!(matches!(BsetView::parse(&lying), Err(KvError::Corrupt(_))));

        // record_count lying high (walk runs out of data).
        let mut extra = buf.clone();
        extra[8..12].copy_from_slice(&3u32.to_le_bytes());
        restamp_checksum(&mut extra);
        assert!(matches!(BsetView::parse(&extra), Err(KvError::Corrupt(_))));

        // record_count lying low (trailing garbage after the walk).
        let mut fewer = buf.clone();
        fewer[8..12].copy_from_slice(&1u32.to_le_bytes());
        restamp_checksum(&mut fewer);
        assert!(matches!(BsetView::parse(&fewer), Err(KvError::Corrupt(_))));

        // data_len disagreeing with the buffer.
        let mut short_data = buf.clone();
        let real_data_len = (buf.len() - BSET_HEADER_LEN) as u32;
        short_data[12..16].copy_from_slice(&(real_data_len - 1).to_le_bytes());
        restamp_checksum(&mut short_data);
        assert!(matches!(
            BsetView::parse(&short_data),
            Err(KvError::Corrupt(_))
        ));
    }

    #[test]
    fn bset_parse_rejects_out_of_order_records_behind_a_valid_checksum() {
        // Two independently valid bsets spliced so records are unsorted, with
        // the checksum restamped: parse must still refuse (writer-bug guard).
        let a = build_bset(&[put(2, 1)], 1).expect("build");
        let b = build_bset(&[put(1, 2)], 2).expect("build");
        let mut spliced = Vec::new();
        spliced.extend_from_slice(&a[..8]);
        spliced.extend_from_slice(&2u32.to_le_bytes()); // record_count = 2
        let data_len = (a.len() - BSET_HEADER_LEN + b.len() - BSET_HEADER_LEN) as u32;
        spliced.extend_from_slice(&data_len.to_le_bytes());
        spliced.extend_from_slice(&a[16..24]); // horizon
        spliced.extend_from_slice(&[0u8; 8]); // checksum placeholder
        spliced.extend_from_slice(&a[BSET_HEADER_LEN..]);
        spliced.extend_from_slice(&b[BSET_HEADER_LEN..]);
        restamp_checksum(&mut spliced);
        assert!(matches!(
            BsetView::parse(&spliced),
            Err(KvError::Corrupt(_))
        ));
    }

    // -- binary search (§4.2: memcmp, no decoding) -----------------------------

    #[test]
    fn bset_binary_search_finds_ranges_without_decoding() {
        let records = vec![
            put(10, 1),
            put(20, 2),
            Record::delta(inode_key(20).to_vec(), 5, &InodeDelta::times(1, 1)),
            Record::delta(inode_key(20).to_vec(), 8, &InodeDelta::times(2, 2)),
            put(30, 3),
        ];
        let buf = build_bset(&records, 8).expect("build");
        let view = BsetView::parse(&buf).expect("parse");

        assert_eq!(view.find(&inode_key(10)), 0..1);
        let multi = view.find(&inode_key(20));
        assert_eq!(multi, 1..4, "multi-seq key yields the whole range");
        let seqs: Vec<u64> = multi.map(|i| view.record(i).seq).collect();
        assert_eq!(
            seqs,
            vec![2, 5, 8],
            "within a key the range is seq-ascending"
        );
        assert_eq!(view.find(&inode_key(30)), 4..5);

        // Misses: before, between, after — empty ranges at the insertion point.
        assert_eq!(view.find(&inode_key(5)), 0..0);
        assert_eq!(view.find(&inode_key(25)), 4..4);
        assert_eq!(view.find(&inode_key(99)), 5..5);
    }

    // -- n-way merge ------------------------------------------------------------

    #[test]
    fn bset_nway_merge_orders_key_asc_seq_desc_source_priority() {
        // Three sources, newest first, with interleaved keys, a multi-source
        // key, and one exact (key, seq) duplicate across sources 0 and 2.
        let newest = build_bset(
            &[
                Record::delta(inode_key(1).to_vec(), 9, &InodeDelta::times(9, 9)),
                put(2, 7),
            ],
            9,
        )
        .expect("build");
        let mid = build_bset(&[put(1, 5), put(3, 6)], 6).expect("build");
        let oldest = build_bset(&[put(1, 2), put(2, 7), put(4, 1)], 7).expect("build");

        let sources = [
            BsetView::parse(&newest).expect("parse"),
            BsetView::parse(&mid).expect("parse"),
            BsetView::parse(&oldest).expect("parse"),
        ];
        let merged: Vec<(Vec<u8>, u64)> =
            merge(&sources).map(|r| (r.key.to_vec(), r.seq)).collect();
        assert_eq!(
            merged,
            vec![
                (inode_key(1).to_vec(), 9),
                (inode_key(1).to_vec(), 5),
                (inode_key(1).to_vec(), 2),
                (inode_key(2).to_vec(), 7), // from source 0 (newer) …
                (inode_key(2).to_vec(), 7), // … then the source-2 duplicate
                (inode_key(3).to_vec(), 6),
                (inode_key(4).to_vec(), 1),
            ],
            "merge must yield (key asc, seq desc, source priority) with \
             duplicates preserved for the idempotent fold"
        );
    }

    // -- lookup fold across sources ---------------------------------------------

    #[test]
    fn bset_lookup_folds_across_sources_with_the_single_algebra() {
        let old = build_bset(&[put(1, 2), put(2, 3), put(3, 4)], 4).expect("build");
        let new = build_bset(
            &[
                Record::delta(inode_key(1).to_vec(), 9, &InodeDelta::times(90, 99)),
                Record::delete(inode_key(2).to_vec(), 8),
            ],
            9,
        )
        .expect("build");
        let sources = [
            BsetView::parse(&new).expect("parse"),
            BsetView::parse(&old).expect("parse"),
        ];

        // Δ in the newer bset folds onto the base Put in the older one.
        match lookup(&sources, &inode_key(1)).expect("fold") {
            Folded::Put { value, seq } => {
                assert_eq!(seq, 9);
                let v = InodeValue::decode(&value).expect("decodes");
                assert_eq!((v.mtime, v.ctime), (90, 99));
            }
            other => panic!("expected folded Put, got {other:?}"),
        }

        // Tombstone in the newer bset shadows the older Put.
        assert_eq!(
            lookup(&sources, &inode_key(2)).expect("fold"),
            Folded::Tombstone { seq: 8 }
        );

        // Untouched key: zero-copy borrowed straight out of the old bset.
        match lookup(&sources, &inode_key(3)).expect("fold") {
            Folded::Put { value, seq } => {
                assert_eq!(seq, 4);
                assert!(
                    matches!(value, Cow::Borrowed(_)),
                    "plain Put lookups must borrow from the bset buffer (zero-copy)"
                );
            }
            other => panic!("expected borrowed Put, got {other:?}"),
        }

        // Absent key.
        assert_eq!(
            lookup(&sources, &inode_key(99)).expect("fold"),
            Folded::Absent
        );
    }

    // -- compaction ---------------------------------------------------------------

    #[test]
    fn bset_compact_emits_a_buildable_folded_output() {
        let old = build_bset(&[put(1, 1), put(2, 2), put(3, 3), put(4, 4)], 4).expect("build");
        let new = build_bset(
            &[
                Record::delta(inode_key(1).to_vec(), 9, &InodeDelta::times(90, 99)),
                Record::delete(inode_key(2).to_vec(), 5),
                Record::delete(inode_key(3).to_vec(), 8),
            ],
            9,
        )
        .expect("build");
        let sources = [
            BsetView::parse(&new).expect("parse"),
            BsetView::parse(&old).expect("parse"),
        ];

        // durable_tail = 6: the seq-5 tombstone (and the key it shadows) is
        // checkpoint-covered ⇒ elided entirely; the seq-8 tombstone is still
        // inside the replay window ⇒ survives (§4.2 elision rule).
        let out = compact(&sources, 6).expect("compact");
        let shape: Vec<(Vec<u8>, RecordKind, u64)> =
            out.iter().map(|r| (r.key.clone(), r.kind, r.seq)).collect();
        assert_eq!(
            shape,
            vec![
                (inode_key(1).to_vec(), RecordKind::Put, 9),
                (inode_key(3).to_vec(), RecordKind::Delete, 8),
                (inode_key(4).to_vec(), RecordKind::Put, 4),
            ],
            "one folded Put (or a surviving tombstone) per key, key-ascending"
        );
        let folded = InodeValue::decode(&out[0].value).expect("folded value decodes");
        assert_eq!((folded.mtime, folded.ctime), (90, 99));

        // The output must feed straight back into build_bset (strictly sorted,
        // unique keys) — the compaction rewrite path in K2.
        let rebuilt = build_bset(&out, 9).expect("compaction output is buildable");
        let reparsed = BsetView::parse(&rebuilt).expect("parse");
        assert_eq!(reparsed.len(), 3);

        // Compacting the compacted output again is a fixed point.
        let twice = compact(&[reparsed], 6).expect("compact twice");
        assert_eq!(twice, out, "compaction is idempotent");
    }

    // -- forced hash collision chain, end to end (§4.2 + §5.1) --------------------

    #[test]
    fn forced_collision_chain_resolves_by_name_and_orders_cookies() {
        // Two names forced onto the same (parent, hash54) chain — the
        // collision is injected at the coll_seq layer, which is exactly what
        // the seeded hash makes adversarially unreachable (§9).
        let parent = 1u64;
        let hash54 = 0xABCDEF__u64;

        let first_seq = assign_dentry_coll_seq(std::iter::empty()).expect("empty chain");
        assert_eq!(first_seq, 0);
        let second_seq = assign_dentry_coll_seq([first_seq]).expect("one occupant");
        assert_eq!(second_seq, 1);

        let alpha = DentryValue {
            child_ino: 100,
            file_type: 8,
            name: b"alpha".to_vec(),
        };
        let beta = DentryValue {
            child_ino: 200,
            file_type: 8,
            name: b"beta".to_vec(),
        };
        let records = vec![
            Record::put(
                dentry_key(parent, hash54, first_seq).to_vec(),
                1,
                alpha.encode().expect("encode"),
            ),
            Record::put(
                dentry_key(parent, hash54, second_seq).to_vec(),
                2,
                beta.encode().expect("encode"),
            ),
        ];
        let buf = build_bset(&records, 2).expect("chain keys are adjacent and ordered");
        let view = BsetView::parse(&buf).expect("parse");

        // Probe the chain comparing full names — the §4.2 collision rule.
        let mut resolved = Vec::new();
        for coll_seq in 0..=255u8 {
            let range = view.find(&dentry_key(parent, hash54, coll_seq));
            for i in range {
                let d = DentryValue::decode(view.record(i).value).expect("decode");
                resolved.push((coll_seq, d));
            }
        }
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0], (0, alpha));
        assert_eq!(resolved[1], (1, beta));
        let by_name = resolved
            .iter()
            .find(|(_, d)| d.name == b"beta")
            .expect("full-name comparison resolves the collision");
        assert_eq!(by_name.1.child_ino, 200);

        // Their readdir cookies differ only in coll_seq and sort like the keys.
        let c0 = encode_readdir_cookie(hash54, 0);
        let c1 = encode_readdir_cookie(hash54, 1);
        assert_eq!(c1, c0 + 1);
        assert!(
            dentry_key(parent, hash54, 0) < dentry_key(parent, hash54, 1),
            "cookie order must equal key order for resume"
        );
    }
}
