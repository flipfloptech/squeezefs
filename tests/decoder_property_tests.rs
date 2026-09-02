//! **TEST-4** (`docs/pre-rc-engineering-spec.md` §11): property coverage
//! for the on-disk decoders — round-trip and never-panic-on-arbitrary-bytes
//! for `kv/{record,node,bset,journal}.rs` and `layout_wire.rs`.
//!
//! Why this exists next to `fuzz/`: the `cargo-fuzz` targets are the deep,
//! coverage-guided campaign, but they need a nightly toolchain and a
//! wall-clock budget. These proptests are the same laws at per-commit
//! cadence, on stable, inside the normal gate — so a regression that
//! reintroduces a panic-on-corrupt-bytes cannot wait for the next fuzz run
//! to be noticed.
//!
//! The laws:
//!
//! 1. **Total decoders.** Every decoder is total over arbitrary bytes:
//!    `Ok` or a typed error, never a panic, never an unbounded allocation.
//!    On-disk metadata is checksummed but NOT authenticated — anyone who
//!    can write the device can re-stamp a digest, and a torn write
//!    produces valid-looking headers over garbage by construction.
//! 2. **Round-trip.** Anything that decodes must re-encode to something
//!    that decodes identically. A decoder that accepts a form its own
//!    encoder cannot produce is a format hole.
//! 3. **Bounded work.** A length field is a claim, never an allocation
//!    authority (§9). The `record_count` case below is the regression pin
//!    for a real find (see `kv_bset_record_count_cannot_drive_allocation`).

use proptest::prelude::*;
use std::os::unix::process::CommandExt;

use squeezefs::layout_wire::{decode_base_layout, encode_layout, LayoutDelta, LayoutMetadata};
use squeezefs::meta_backend::kv::block_map::{
    decode_block_map_key, decode_block_map_value, parse_kvmap_head,
};
use squeezefs::meta_backend::kv::bset::{
    build_bset, checksum_image, BsetView, BSET_HEADER_LEN, BSET_MAGIC, BSET_VERSION,
};
use squeezefs::meta_backend::kv::journal::{decode_entry_payload, encode_entry_payload};
use squeezefs::meta_backend::kv::node::{verify_node_extent, NodeLayout};
use squeezefs::meta_backend::kv::record::{
    decode_dentry_key, decode_inode_key, decode_readdir_cookie, decode_xattr_key, DentryValue,
    InodeDelta, InodeValue, Record, RecordRef, XattrValue,
};
use squeezefs::meta_backend::kv::superblock::SuperblockV3;

// ---------------------------------------------------------------------------
// The find: a lying `record_count` used to be an allocation authority
// ---------------------------------------------------------------------------

/// **Regression pin for a TEST-4 find.** `BsetView::parse` sized its
/// per-record metadata vector straight from the header's `record_count`:
///
/// ```ignore
/// let mut metas = Vec::with_capacity(record_count);   // record_count: u32
/// ```
///
/// The checksum is integrity, not authentication — a corrupt device, a
/// misdirected write, or anyone who can write the volume can present a
/// 32-byte bset whose header claims `record_count = u32::MAX` and whose
/// digest verifies. That is `Vec::with_capacity(4_294_967_295)` of a
/// 32-byte struct: a **128 GiB** allocation request on a decoder whose
/// documented contract is that corrupt bytes are detected, not fatal. The
/// allocation aborts the process (Rust's OOM handler is `abort`), so a
/// single bad sector takes the daemon down — and `--test-threads=1` would
/// have made it a whole-suite kill.
///
/// A bset record cannot be smaller than `RECORD_HEADER_LEN + 1` (a
/// one-byte key, no value), so `record_count` is bounded by `data_len`
/// exactly, and the check costs one division.
#[test]
fn kv_bset_record_count_cannot_drive_allocation() {
    // A well-formed, checksum-valid, EMPTY-data bset image whose header
    // claims the maximum record count.
    let mut img = vec![0u8; BSET_HEADER_LEN];
    img[0..4].copy_from_slice(&BSET_MAGIC.to_le_bytes());
    img[4..6].copy_from_slice(&BSET_VERSION.to_le_bytes());
    img[6..8].copy_from_slice(&0u16.to_le_bytes()); // reserved
    img[8..12].copy_from_slice(&u32::MAX.to_le_bytes()); // record_count: the lie
    img[12..16].copy_from_slice(&0u32.to_le_bytes()); // data_len: the truth
    img[16..24].copy_from_slice(&0u64.to_le_bytes()); // horizon
    let sum = checksum_image(&img);
    img[24..32].copy_from_slice(&sum.to_le_bytes());

    // Pre-fix this line attempts a 128 GiB allocation and aborts.
    let msg = match BsetView::parse(&img) {
        Ok(_) => panic!("a bset claiming 4 G records in 0 data bytes must be refused"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("4294967295") && msg.contains("records"),
        "the refusal must name the impossible count, got: {msg}"
    );

    // And the honest boundary still parses: a real single-record bset.
    let recs = vec![Record::put(b"k".to_vec(), 1, b"v".to_vec())];
    let good = build_bset(&recs, 1).expect("a real bset builds");
    let view = BsetView::parse(&good).expect("a real bset parses");
    assert_eq!(view.len(), 1);
}

/// The **bounded-work** half of the same find, pinned mechanically rather
/// than by reading the error string: run the parse in a child with a tight
/// `RLIMIT_AS`.
///
/// Measured on the dev box (2026-08-02): the pre-fix parse requests
/// 137,438,953,440 B — `VmPeak` 128 GiB. With address space unlimited that
/// is "only" a 128 GiB VA reservation the fuzzer shrugs at; under any
/// address-space cap (a systemd `LimitAS=`, a container, a hardened
/// service posture) it is `memory allocation of 137438953440 bytes
/// failed` → **SIGABRT, core dumped**. One corrupt sector kills the
/// daemon. Under a 1 GiB cap the fixed parser must simply return `Err`.
#[test]
fn kv_bset_parse_survives_a_tight_address_space_limit() {
    if std::env::var("SQZ_BSET_ALLOC_CHILD").is_ok() {
        // Child: the parse under the caller's RLIMIT_AS.
        let mut img = vec![0u8; BSET_HEADER_LEN];
        img[0..4].copy_from_slice(&BSET_MAGIC.to_le_bytes());
        img[4..6].copy_from_slice(&BSET_VERSION.to_le_bytes());
        img[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        let sum = checksum_image(&img);
        img[24..32].copy_from_slice(&sum.to_le_bytes());
        assert!(BsetView::parse(&img).is_err(), "must refuse, not allocate");
        return;
    }

    let mut cmd = std::process::Command::new(std::env::current_exe().expect("test binary"));
    cmd.args([
        "kv_bset_parse_survives_a_tight_address_space_limit",
        "--exact",
        "--nocapture",
        "--test-threads=1",
    ])
    .env("SQZ_BSET_ALLOC_CHILD", "1")
    .env("RUST_BACKTRACE", "0");
    // SAFETY: `setrlimit` is async-signal-safe and touches only this
    // freshly-forked child's own limits, before exec.
    unsafe {
        cmd.pre_exec(|| {
            let lim = libc::rlimit {
                rlim_cur: 1 << 30, // 1 GiB of address space
                rlim_max: 1 << 30,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &lim) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = cmd.output().expect("spawn the rlimit-bounded child");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "parsing a bset that claims u32::MAX records must not allocate \
         proportionally to the CLAIM — the child died under a 1 GiB \
         RLIMIT_AS:\n{text}"
    );
}

/// The same law stated as a bound rather than an example: for every
/// `data_len`, a claimed `record_count` past what `data_len` can physically
/// hold is refused — with a valid checksum, so only the count is at fault.
#[test]
fn kv_bset_record_count_bound_holds_across_data_lengths() {
    for data_len in [0usize, 1, 15, 16, 64, 4096] {
        // The minimum encoded record is a 15-byte header + a 1-byte key.
        let max_possible = data_len / 16;
        let claimed = (max_possible + 1) as u32;
        let mut img = vec![0u8; BSET_HEADER_LEN + data_len];
        img[0..4].copy_from_slice(&BSET_MAGIC.to_le_bytes());
        img[4..6].copy_from_slice(&BSET_VERSION.to_le_bytes());
        img[8..12].copy_from_slice(&claimed.to_le_bytes());
        img[12..16].copy_from_slice(&(data_len as u32).to_le_bytes());
        let sum = checksum_image(&img);
        img[24..32].copy_from_slice(&sum.to_le_bytes());
        assert!(
            BsetView::parse(&img).is_err(),
            "data_len {data_len} cannot hold {claimed} records — must refuse"
        );
    }
}

// ---------------------------------------------------------------------------
// never-panic over arbitrary bytes
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// Every record-level decoder is total.
    #[test]
    fn record_decoders_never_panic(data in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = decode_inode_key(&data);
        let _ = decode_dentry_key(&data);
        let _ = decode_xattr_key(&data);
        let _ = InodeValue::decode(&data);
        let _ = DentryValue::decode(&data);
        let _ = XattrValue::decode(&data);
        let _ = InodeDelta::decode(&data);
        let _ = RecordRef::decode(&data);
        if data.len() >= 8 {
            let cookie = u64::from_le_bytes(data[..8].try_into().unwrap());
            let _ = decode_readdir_cookie(cookie);
        }
    }

    /// The bset parser is total on raw bytes AND on bytes whose checksum
    /// has been re-stamped — the corrupt-but-verifying case a device-level
    /// fault or a writer bug produces, and the only arm that reaches the
    /// record walk at all.
    #[test]
    fn bset_parse_never_panics(data in prop::collection::vec(any::<u8>(), 0..1024)) {
        let _ = BsetView::parse(&data);
        if data.len() >= BSET_HEADER_LEN {
            let mut restamped = data.clone();
            restamped[24..32].copy_from_slice(&0u64.to_le_bytes());
            let sum = checksum_image(&restamped);
            restamped[24..32].copy_from_slice(&sum.to_le_bytes());
            if let Ok(v) = BsetView::parse(&restamped) {
                // Whatever it accepted must be internally consistent.
                for i in 0..v.len() {
                    let r = v.record(i);
                    prop_assert!(!r.key.is_empty());
                }
                prop_assert_eq!(v.iter().count(), v.len());
            }
        }
    }

    /// The whole node-extent grammar is total: header page, self-address,
    /// the §4.5 append walk, and the torn-tail diagnosis pass.
    #[test]
    fn node_extent_verify_never_panics(seed in prop::collection::vec(any::<u8>(), 0..2048)) {
        const NODE_SIZE: usize = 64 * 1024;
        let layout = NodeLayout::new(NODE_SIZE).expect("64 KiB node size");
        let mut buf = vec![0u8; NODE_SIZE];
        let n = seed.len().min(NODE_SIZE);
        buf[..n].copy_from_slice(&seed[..n]);
        let claimed = u64::from_le_bytes(buf[8..16].try_into().unwrap()) & !0xFFF;
        for addr in [0u64, claimed] {
            if let Ok(node) = verify_node_extent(
                bytes::Bytes::from(buf.clone()),
                &layout,
                addr,
                0,
            ) {
                prop_assert!(node.tail_offset() <= NODE_SIZE);
                for i in 0..node.bset_count() {
                    prop_assert!(node.bset(i).is_ok());
                }
            }
        }
    }

    /// Replay reads the journal payload at every mount, over a ring whose
    /// tail is by definition partially written.
    #[test]
    fn journal_payload_decode_never_panics(data in prop::collection::vec(any::<u8>(), 0..512)) {
        if let Ok(records) = decode_entry_payload(&data) {
            let re = encode_entry_payload(&records);
            let again = decode_entry_payload(&re).expect("a re-encoded payload decodes");
            prop_assert_eq!(again.len(), records.len());
        }
    }

    /// Superblock sector 0 — the one structure with no second copy.
    #[test]
    fn superblock_decode_never_panics(data in prop::collection::vec(any::<u8>(), 0..1024)) {
        let _ = SuperblockV3::decode_sector(&data);
        let _ = SuperblockV3::decode_sector_with_known(&data, 0);
    }

    /// The layout delta wire and its bincode base.
    #[test]
    fn layout_wire_decoders_never_panic(data in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = decode_base_layout(&data);
        if let Ok(d) = LayoutDelta::decode(&data) {
            let re = d.encode();
            let again = LayoutDelta::decode(&re).expect("a decoded delta re-encodes");
            prop_assert_eq!(again.size, d.size);
            prop_assert_eq!(again.entries.len(), d.entries.len());
        }
    }

    /// The block-map tree's key decoder (PR 1,
    /// docs/design-kvmap-block-map-tree.md §2) is total.
    #[test]
    fn block_map_key_decode_never_panics(data in prop::collection::vec(any::<u8>(), 0..64)) {
        let _ = decode_block_map_key(&data);
    }

    /// The block-map tree's value decoder is total, and anything it
    /// accepts round-trips byte-identically (the exact-encoding law).
    #[test]
    fn block_map_value_decode_never_panics(data in prop::collection::vec(any::<u8>(), 0..128)) {
        if let Ok(entry) = decode_block_map_value(&data) {
            prop_assert_eq!(entry.encode(), data);
        }
    }

    /// The `kvmap:` head-sentinel parser is total over arbitrary strings —
    /// both far-from-grammar inputs and near-misses behind the prefix —
    /// and anything it accepts round-trips through its own encoder.
    #[test]
    fn kvmap_head_parse_never_panics(s in "\\PC{0,48}") {
        let _ = parse_kvmap_head(&s);
        let prefixed = format!("kvmap:{s}");
        if let Ok(head) = parse_kvmap_head(&prefixed) {
            prop_assert_eq!(head.encode(), prefixed);
        }
    }
}

// ---------------------------------------------------------------------------
// round-trip
// ---------------------------------------------------------------------------

fn arb_record() -> impl Strategy<Value = Record> {
    (
        prop::collection::vec(any::<u8>(), 1..24),
        any::<u64>(),
        prop::collection::vec(any::<u8>(), 0..48),
        any::<bool>(),
    )
        .prop_map(|(key, seq, value, tombstone)| {
            if tombstone {
                Record::delete(key, seq)
            } else {
                Record::put(key, seq, value)
            }
        })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn record_frame_round_trips(r in arb_record()) {
        let mut buf = Vec::new();
        r.record_ref().encode_into(&mut buf);
        let (back, used) = RecordRef::decode(&buf).expect("encoded record decodes");
        prop_assert_eq!(used, buf.len());
        prop_assert_eq!(back.key, r.key.as_slice());
        prop_assert_eq!(back.seq, r.seq);
        prop_assert_eq!(back.value, r.value.as_slice());
        prop_assert_eq!(back.kind, r.kind);
    }

    #[test]
    fn inode_value_round_trips(
        mode in any::<u32>(), uid in any::<u32>(), gid in any::<u32>(),
        nlink in any::<u32>(), flags in any::<u32>(), rdev in any::<u32>(),
        size in any::<u64>(), atime in any::<u64>(), mtime in any::<u64>(),
        ctime in any::<u64>(),
    ) {
        let v = InodeValue { mode, uid, gid, nlink, flags, rdev, size, atime, mtime, ctime };
        let enc = v.encode();
        prop_assert_eq!(InodeValue::decode(&enc).expect("decodes"), v);
    }

    #[test]
    fn dentry_value_round_trips(
        child_ino in any::<u64>(),
        file_type in any::<u8>(),
        name in prop::collection::vec(any::<u8>(), 0..255),
    ) {
        let v = DentryValue { child_ino, file_type, name };
        let enc = v.encode().expect("encodes");
        prop_assert_eq!(DentryValue::decode(&enc).expect("decodes"), v);
    }

    #[test]
    fn xattr_value_round_trips(
        name in prop::collection::vec(any::<u8>(), 0..255),
        value in prop::collection::vec(any::<u8>(), 0..512),
    ) {
        let v = XattrValue { name, value };
        let enc = v.encode().expect("encodes");
        prop_assert_eq!(XattrValue::decode(&enc).expect("decodes"), v);
    }

    /// A bset built from strictly ascending records must parse back to the
    /// same records, in the same order, with the same horizon.
    #[test]
    fn bset_round_trips(
        mut recs in prop::collection::vec(arb_record(), 1..24),
        horizon in any::<u64>(),
    ) {
        recs.sort_by(|a, b| a.key.cmp(&b.key).then(a.seq.cmp(&b.seq)));
        recs.dedup_by(|a, b| a.key == b.key && a.seq == b.seq);
        let img = build_bset(&recs, horizon).expect("ascending records build");
        let view = BsetView::parse(&img).expect("a built bset parses");
        prop_assert_eq!(view.len(), recs.len());
        prop_assert_eq!(view.journal_seq_horizon(), horizon);
        for (i, want) in recs.iter().enumerate() {
            let got = view.record(i);
            prop_assert_eq!(got.key, want.key.as_slice());
            prop_assert_eq!(got.seq, want.seq);
            prop_assert_eq!(got.value, want.value.as_slice());
        }
    }

    /// A journal payload round-trips through its tag/record framing.
    #[test]
    fn journal_payload_round_trips(
        recs in prop::collection::vec(arb_record(), 0..16),
        tree in squeezefs::meta_backend::kv::record::TREE_INODES
            ..=squeezefs::meta_backend::kv::record::TREE_BACKPTR_RESERVED,
    ) {
        let tagged: Vec<(u8, Record)> = recs.into_iter().map(|r| (tree, r)).collect();
        let enc = encode_entry_payload(&tagged);
        let back = decode_entry_payload(&enc).expect("encoded payload decodes");
        prop_assert_eq!(back.len(), tagged.len());
        for (a, b) in back.iter().zip(tagged.iter()) {
            prop_assert_eq!(a.0, b.0);
            prop_assert_eq!(&a.1.key, &b.1.key);
            prop_assert_eq!(a.1.seq, b.1.seq);
            prop_assert_eq!(&a.1.value, &b.1.value);
        }
    }

    /// The layout-delta wire round-trips its whole optional-field matrix —
    /// including the §6.2 item-9 version pair (present ⇔ `version != 0`;
    /// an unversioned record cannot carry a base claim, which is the
    /// `set_versions` invariant the generator honours).
    #[test]
    fn layout_delta_round_trips(
        file_type in "[a-z]{1,8}",
        size in any::<u64>(),
        block_map_id in prop::option::of("[a-z0-9:]{1,16}"),
        block_prefix in prop::option::of("[a-z0-9/]{1,16}"),
        file_id in prop::option::of("[a-z0-9-]{1,16}"),
        data_key in prop::option::of(prop::collection::vec(any::<u8>(), 1..32)),
        entries in prop::collection::vec((any::<u32>(), "[a-z0-9]{1,12}"), 0..8),
        versions in prop::option::of((any::<u64>(), 1u64..=u64::MAX)),
    ) {
        let mut d = LayoutDelta {
            file_type,
            size,
            block_map_id,
            block_prefix,
            file_id,
            data_key,
            entries,
            ..Default::default()
        };
        if let Some((base, version)) = versions {
            d.set_versions(base, version);
        }
        let enc = d.encode();
        let back = LayoutDelta::decode(&enc).expect("an encoded delta decodes");
        prop_assert_eq!(&back, &d, "whole-struct roundtrip (versions included)");
        // The strip form is the un-stamped volume's wire: byte-identical
        // to the zero-version twin (the bit-5 compatibility law).
        let mut stripped = d.clone();
        stripped.set_versions(0, 0);
        prop_assert_eq!(d.encode_unversioned(), stripped.encode());
    }

    /// A layout base survives encode → decode with its map intact.
    #[test]
    fn layout_base_round_trips(
        size in any::<u64>(),
        map in prop::collection::vec((any::<u32>(), "[a-z0-9]{1,12}"), 0..8),
    ) {
        let mut layout = LayoutMetadata {
            size,
            ..Default::default()
        };
        if !map.is_empty() {
            layout.block_map = Some(map.iter().cloned().collect());
        }
        let enc = encode_layout(&layout).expect("encodes");
        let back = decode_base_layout(&enc).expect("decodes");
        prop_assert_eq!(back.size, layout.size);
        prop_assert_eq!(back.block_map, layout.block_map);
    }
}
