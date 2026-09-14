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

use squeezefs::cluster_wire::{
    hex_decode, hex_encode, read_plain_frame, session_framers, session_key, write_plain_frame,
    FrameClass, Role, RpcFrame,
};
use squeezefs::layout_wire::{decode_base_layout, encode_layout, LayoutDelta, LayoutMetadata};
use squeezefs::meta_backend::kv::appender::{
    classify_page, newest_valid, AppenderIdentity, AppenderPage, AppenderState, DirHeader,
    GrantRun, PageRead, SlotEntry, SlotEntryState, APPENDER_PAGE_LEN, GRANT_RUNS_MAX,
    RING_SEGMENTS_MAX, SLOT_PAGE_BUDGET,
};
use squeezefs::meta_backend::kv::block_map::{
    block_map_key, decode_block_map_key, decode_block_map_value, parse_kvmap_head,
};
use squeezefs::meta_backend::kv::block_refs::block_ref_key;
use squeezefs::meta_backend::kv::bset::{
    build_bset, checksum_image, BsetView, BSET_HEADER_LEN, BSET_MAGIC, BSET_VERSION,
};
use squeezefs::meta_backend::kv::forest::{
    interior_journal_key, split_interior_journal_key, INTERIOR_JOURNAL_SLOT_LEN,
};
use squeezefs::meta_backend::kv::journal::{decode_entry_payload, encode_entry_payload};
use squeezefs::meta_backend::kv::node::{verify_node_extent, NodeLayout};
use squeezefs::meta_backend::kv::record::{
    classify_refused_key, decode_dentry_key, decode_inode_key, decode_readdir_cookie,
    decode_xattr_key, dentry_key, forest_key, forest_key_kind, forest_key_slot, forest_slot_of_ino,
    inode_key, is_slot_tree_kind, split_forest_key, xattr_key, DentryValue, InodeDelta, InodeValue,
    RawKeyDefect, Record, RecordRef, XattrValue, DENTRY_KEY_LEN, FOREST_BLOCK_REF_KEY_LEN,
    FOREST_SLOT_MAX, INODE_KEY_LEN, TREE_BLOCK_MAP, TREE_BLOCK_REFS, TREE_DENTRIES, TREE_INODES,
    TREE_XATTRS, XATTR_KEY_LEN,
};
use squeezefs::meta_backend::kv::slot_state::{
    decode_slot_state_key, slot_state_key, SlotState, SLOT_STATE_KEY_LEN, SLOT_STATE_VERSION,
};
use squeezefs::meta_backend::kv::superblock::{ExtentRef, SuperblockV3};
use squeezefs::meta_backend::kv::tree::RootPtr;
use squeezefs::meta_backend::GUEST_NS_SHIFT;
use squeezefs::meta_ship::manager::{
    decode_reply as decode_manager_reply, decode_request as decode_manager_request,
    encode_reply as encode_manager_reply, encode_request as encode_manager_request, ManagerCall,
    ManagerReply, ManagerReplyFrame, ManagerRequestFrame, WireIdentity, MANAGER_SCHEMA,
};
use squeezefs::meta_ship::publish::{
    decode_reply_frame, decode_request_frame, encode_reply_frame, encode_request_frame,
    PublishCall, PublishCallOutcome, PublishReply, PublishReplyFrame, PublishRequestFrame,
    WireBlockRefOp, WireFreedBlock, WireLaneFree, PUBLISH_SCHEMA,
};
use squeezefs::meta_ship::wire::{
    decode_reclaim, decode_reply, decode_request, encode_reclaim, ReclaimFrame, WireError,
};

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
// The find: `hex_decode` sliced a &str by BYTE index
// ---------------------------------------------------------------------------

/// **Regression pin for a 1.2 fuzz find** (`cluster_wire_frame`, 414
/// executions in). `cluster_wire::hex_decode` walked its input two BYTES at
/// a time with `&s[i..i + 2]` — a `str` slice, which panics when the end
/// index lands inside a multi-byte character. Its documented contract is
/// "`None` on any non-hex input", and its callers decode the `job:enroll`
/// record's `secret` — on-disk metadata anyone who can write the volume
/// controls — so a four-byte xattr like `":ז\0"` was a daemon **abort**
/// (`panic = "abort"` in the release profile) at the first job-wire or
/// membership enrollment. The same walk used `u8::from_str_radix`, which
/// accepts a leading sign: `"+f"` decoded to `[0x0f]`, a form
/// `hex_encode` never produces (a parser wider than its encoder).
#[test]
fn cluster_wire_hex_decode_is_total_and_exact() {
    // The minimized fuzz artifact: `:`, then U+05D6 (2 bytes), then NUL —
    // 4 bytes, even, and byte index 2 is inside the character.
    let artifact = std::str::from_utf8(&[0x3a, 0xd7, 0x96, 0x00]).expect("valid UTF-8");
    assert_eq!(artifact.len(), 4);
    assert_eq!(
        hex_decode(artifact),
        None,
        "non-ASCII is non-hex, never a panic"
    );
    // Sign characters are not hex digits, whatever `from_str_radix` thinks.
    assert_eq!(hex_decode("+f"), None);
    assert_eq!(hex_decode("-0"), None);
    assert_eq!(hex_decode("0+"), None);
    // The honest boundary: both cases decode, and lower-case is canonical.
    assert_eq!(hex_decode("0aFf"), Some(vec![0x0a, 0xff]));
    assert_eq!(hex_decode(""), Some(Vec::new()));
    assert_eq!(hex_decode("abc"), None, "odd length");
    assert_eq!(
        hex_decode(&hex_encode(&[0, 127, 128, 255])),
        Some(vec![0, 127, 128, 255])
    );
}

/// A `slot_state` tails vector the u16 count cannot express is refused at
/// the ENCODER — never truncated into a record whose count lies (the
/// decoder checks the count against the length, so a truncated image would
/// be corruption on the next mount).
#[test]
fn slot_state_tails_past_u16_refuse_at_the_encoder() {
    let too_many = SlotState::Unleased {
        root: RootPtr { addr: 1, seq: 1 },
        cursor: 0,
        g: 0,
        tails: vec![(0, 0); usize::from(u16::MAX) + 1],
    };
    assert!(too_many.encode().is_err());
    let at_cap = SlotState::Unleased {
        root: RootPtr { addr: 1, seq: 1 },
        cursor: 0,
        g: 0,
        tails: vec![(0, 0); usize::from(u16::MAX)],
    };
    let bytes = at_cap.encode().expect("u16::MAX tails encode");
    assert_eq!(SlotState::decode(&bytes).expect("decodes"), at_cap);
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

    /// PR 6a (design §12): every EMITTABLE run/stamped form round-trips —
    /// the constructive mirror of the byte-identity law above (run len in
    /// `2..=RUN_LEN_MAX`, nonzero stamps, non-wrapping RUN2 spans — the
    /// emitter's whole domain).
    #[test]
    fn block_map_run_and_stamped_forms_round_trip(
        vol_tag in any::<u64>(),
        offset in any::<u64>(),
        len in 2u32..=squeezefs::meta_backend::kv::block_map::RUN_LEN_MAX,
        inc in 1u64..=(u64::MAX - u64::from(squeezefs::meta_backend::kv::block_map::RUN_LEN_MAX)),
    ) {
        use squeezefs::meta_backend::kv::block_map::MapEntry;
        for entry in [
            MapEntry::Run { vol_tag, start_offset: offset, len },
            MapEntry::PointStamped { vol_tag, offset, incarnation: inc },
            MapEntry::RunStamped {
                vol_tag,
                start_offset: offset,
                len,
                start_incarnation: inc,
            },
        ] {
            prop_assert_eq!(
                decode_block_map_value(&entry.encode()).expect("emittable form decodes"),
                entry
            );
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

    /// The slot-tree forest's ONE key codec
    /// (docs/design-symmetric-metadata.md §5.2.1, incompat bit 17; the
    /// `slot_tree_record` fuzz target's mirror) is total, and whatever it
    /// accepts re-encodes byte-identically and routes to a slot inside the
    /// namespace.
    #[test]
    fn forest_key_decoders_never_panic(data in prop::collection::vec(any::<u8>(), 0..64)) {
        let kind = forest_key_kind(&data);
        let split = split_forest_key(&data);
        let slot = forest_key_slot(&data);
        prop_assert_eq!(kind.is_ok(), split.is_ok());
        match split {
            Ok((k, legacy)) => {
                prop_assert!(is_slot_tree_kind(k));
                prop_assert_eq!(kind.ok(), Some(k));
                prop_assert_eq!(forest_key(k, &legacy).expect("re-encodes"), data);
                let s = slot.expect("a decoded key routes");
                prop_assert!(s <= FOREST_SLOT_MAX);
            }
            Err(_) => prop_assert!(slot.is_err()),
        }
    }

    /// The encoder's whole domain round-trips, and the slot namespace is
    /// ONE bound enforced in both directions: an ino at or above
    /// `(FOREST_SLOT_MAX + 1) << 40` — the routing ino, i.e. the OWNER for
    /// the refs family — frames iff a slot names it.
    #[test]
    fn forest_key_round_trips_over_the_encoders_domain(
        kind in prop::sample::select(vec![
            TREE_INODES, TREE_DENTRIES, TREE_XATTRS, TREE_BLOCK_MAP, TREE_BLOCK_REFS,
        ]),
        // Half the draws inside the namespace, half at/above its edge.
        ino in prop_oneof![
            0u64..=((u64::from(FOREST_SLOT_MAX) + 1) << GUEST_NS_SHIFT) - 1,
            ((u64::from(FOREST_SLOT_MAX) + 1) << GUEST_NS_SHIFT)..=u64::MAX,
        ],
        hash in any::<u64>(),
        coll in any::<u8>(),
        idx in 0u32..u32::MAX,
        vol_tag in any::<u64>(),
        block_idx in any::<u64>(),
    ) {
        let legacy: Vec<u8> = match kind {
            TREE_INODES => inode_key(ino).to_vec(),
            TREE_DENTRIES => dentry_key(ino, hash & ((1 << 54) - 1), coll).to_vec(),
            TREE_XATTRS => xattr_key(ino, hash & ((1 << 56) - 1), coll).to_vec(),
            TREE_BLOCK_MAP => block_map_key(ino, idx).expect("non-reserved index").to_vec(),
            _ => block_ref_key(vol_tag, block_idx, ino, idx).to_vec(),
        };
        let framed = forest_key(kind, &legacy);
        let in_namespace = forest_slot_of_ino(ino) <= FOREST_SLOT_MAX;
        prop_assert_eq!(framed.is_ok(), in_namespace, "kind {} ino {:#x}", kind, ino);
        if let Ok(f) = framed {
            prop_assert_eq!(f.len(), legacy.len() + 1);
            let (k2, l2) = split_forest_key(&f).expect("splits");
            prop_assert_eq!((k2, l2), (kind, legacy.clone()));
            prop_assert_eq!(forest_key_slot(&f).expect("routes"), forest_slot_of_ino(ino));
            // A forged kind byte (any non-content kind) refuses at the decoder.
            let kind_off = if kind == TREE_BLOCK_REFS { 0 } else { 8 };
            for forged_kind in [0u8, 4, 5, 8, 9, 10, 0xFF] {
                let mut forged = f.clone();
                forged[kind_off] = forged_kind;
                prop_assert!(split_forest_key(&forged).is_err(), "kind byte {}", forged_kind);
            }
        }
        // Wrong lengths refuse at the encoder.
        prop_assert!(forest_key(kind, &legacy[..legacy.len() - 1]).is_err());
        let mut long = legacy.clone();
        long.push(0);
        prop_assert!(forest_key(kind, &long).is_err());
    }

    /// The refused-key classifier (fsck's raw C1 deletion gate, review
    /// round 3 Issue 25) is total and agrees with the codec: it names a
    /// defect for exactly the keys the codec refuses, and it names
    /// `MalformedKnownKind` ONLY for a kind byte the codec knows as a
    /// slot-tree kind — an unknown kind (a later binary's record?) and a
    /// key too short to carry a kind are never the deletable class. The
    /// refs kind byte in the ino-major position is refused by the codec
    /// (references are by-block-prefixed) and classified as the known
    /// kind in a shape it never takes.
    #[test]
    fn refused_key_classifier_agrees_with_the_codec(
        data in prop::collection::vec(any::<u8>(), 0..64),
    ) {
        let defect = classify_refused_key(&data);
        prop_assert_eq!(defect.is_none(), split_forest_key(&data).is_ok());
        match defect {
            Some(RawKeyDefect::MalformedKnownKind { kind, want, got }) => {
                prop_assert!(is_slot_tree_kind(kind));
                prop_assert_eq!(got, data.len());
                // `want` is the codec's forest key length for that kind
                // (a position error can carry the right length).
                let codec_len = match kind {
                    TREE_INODES => INODE_KEY_LEN + 1,
                    TREE_DENTRIES => DENTRY_KEY_LEN + 1,
                    TREE_XATTRS => XATTR_KEY_LEN + 1,
                    TREE_BLOCK_MAP => squeezefs::meta_backend::kv::block_map::BLOCK_MAP_KEY_LEN + 1,
                    _ => FOREST_BLOCK_REF_KEY_LEN,
                };
                prop_assert_eq!(want, codec_len);
            }
            Some(RawKeyDefect::UnknownKind { kind }) => prop_assert!(!is_slot_tree_kind(kind)),
            Some(RawKeyDefect::Truncated { got }) => {
                prop_assert_eq!(got, data.len());
                prop_assert!(got < 9);
            }
            // Right kind, right length, a routing ino no slot names: the
            // ONE bound, enforced at the decoder as at the encoder — the
            // key would never have re-encoded.
            Some(RawKeyDefect::SlotOutOfNamespace { kind, slot }) => {
                prop_assert!(is_slot_tree_kind(kind));
                prop_assert!(slot > FOREST_SLOT_MAX);
            }
            None => {}
        }
        if data.len() >= 9 && data[0] <= 0x01 && data[8] == TREE_BLOCK_REFS {
            prop_assert!(forest_key_kind(&data).is_err());
            let refs_in_ino_major = matches!(
                defect,
                Some(RawKeyDefect::MalformedKnownKind { kind: TREE_BLOCK_REFS, .. })
            );
            prop_assert!(refs_in_ino_major, "{:?}", defect);
        }
    }

    /// A kind byte is never another tree's id (round-3 Issue 6): nothing
    /// frames under the interior marker, the reserved ids, tree 0, the
    /// shared index or any byte above — at any length.
    #[test]
    fn non_content_kinds_never_frame(
        kind in prop::sample::select(vec![0u8, 4, 5, 8, 9, 10, 0x7F, 0xFF]),
        legacy in prop::collection::vec(any::<u8>(), 0..40),
    ) {
        prop_assert!(!is_slot_tree_kind(kind));
        prop_assert!(forest_key(kind, &legacy).is_err());
    }

    /// A slot tree's interior journal key (`slot ‖ separator`) splits
    /// exactly; only a key with no separator refuses.
    #[test]
    fn interior_journal_key_split_is_total_and_exact(
        data in prop::collection::vec(any::<u8>(), 0..48),
    ) {
        match split_interior_journal_key(&data) {
            Ok((slot, sep)) => {
                prop_assert!(!sep.is_empty());
                prop_assert_eq!(interior_journal_key(slot, sep), data);
            }
            Err(_) => prop_assert!(data.len() <= INTERIOR_JOURNAL_SLOT_LEN),
        }
    }

    /// Tree 0's `slot_state` codecs (design §5.2.2; the
    /// `slot_state_record` fuzz target's mirror) are total, and whatever
    /// decodes re-encodes byte-identically.
    #[test]
    fn slot_state_decoders_never_panic(data in prop::collection::vec(any::<u8>(), 0..256)) {
        match decode_slot_state_key(&data) {
            Ok(slot) => prop_assert_eq!(slot_state_key(slot), data.clone()),
            Err(_) => prop_assert!(
                data.len() != SLOT_STATE_KEY_LEN || !data.starts_with(b"slot_state:")
            ),
        }
        if let Ok(state) = SlotState::decode(&data) {
            prop_assert_eq!(data[0], SLOT_STATE_VERSION);
            prop_assert_eq!(state.encode().expect("re-encodes"), data);
        }
    }

    /// Every emittable `slot_state` record (both variants, any tails
    /// length a u16 expresses) round-trips; a future version and an
    /// unknown variant refuse; truncation refuses.
    #[test]
    fn slot_state_round_trips_over_the_encoders_domain(
        addr in any::<u64>(),
        seq in any::<u64>(),
        cursor in any::<u64>(),
        g in any::<u32>(),
        tails in prop::collection::vec((any::<u64>(), any::<u32>()), 0..48),
        slot in any::<u32>(),
    ) {
        prop_assert_eq!(decode_slot_state_key(&slot_state_key(slot)).ok(), Some(slot));
        let unleased = SlotState::Unleased { root: RootPtr { addr, seq }, cursor, g, tails };
        let leased = SlotState::Leased { appender_id: g, g: g.wrapping_add(1), page_addr: addr };
        for state in [unleased, leased] {
            let bytes = state.encode().expect("encodes");
            prop_assert_eq!(SlotState::decode(&bytes).expect("decodes"), state.clone());
            let mut future = bytes.clone();
            future[0] = SLOT_STATE_VERSION.wrapping_add(1);
            prop_assert!(SlotState::decode(&future).is_err());
            let mut variant = bytes.clone();
            variant[1] = 0x7F;
            prop_assert!(SlotState::decode(&variant).is_err());
            prop_assert!(SlotState::decode(&bytes[..bytes.len() - 1]).is_err());
        }
    }

    /// The appender page + directory-header codecs (design-symmetric-
    /// metadata §5.3.2, PR 2; the `appender_page` fuzz target's mirror)
    /// are total over arbitrary page-sized bytes — a blank page is
    /// `Blank`, anything unverifiable `Corrupt`, never a panic — and
    /// whatever decodes re-encodes byte-identically (canonical: every
    /// count is bounded, every reserved byte zero).
    #[test]
    fn appender_page_decoders_never_panic(
        data in prop::collection::vec(any::<u8>(), 0..(APPENDER_PAGE_LEN + 8)),
    ) {
        match classify_page(&data) {
            PageRead::Blank => prop_assert!(data.iter().all(|b| *b == 0)),
            PageRead::Valid(p) => {
                prop_assert_eq!(data.len(), APPENDER_PAGE_LEN);
                prop_assert_eq!(p.encode().expect("re-encodes"), data.clone());
                prop_assert!(p.segments.len() <= RING_SEGMENTS_MAX);
                prop_assert!(p.grant.len() <= GRANT_RUNS_MAX);
                prop_assert!(p.slots.len() <= SLOT_PAGE_BUDGET);
            }
            PageRead::Corrupt(_) => {}
        }
        if let Ok(h) = DirHeader::decode(&data) {
            prop_assert_eq!(h.encode(), data.clone());
        }
        // Newest-valid-wins is total over any image set.
        let _ = newest_valid(&[data.as_slice(), &[0u8; APPENDER_PAGE_LEN]]);
    }

    /// Every emittable appender page — any state, any counts within the
    /// bounds, slot entries in slot order — round-trips; one more entry
    /// than the budget refuses at the encoder; the newest generation wins
    /// over its predecessors and a torn newest falls back.
    #[test]
    fn appender_page_round_trips_over_the_encoders_domain(
        id in any::<u32>(),
        generation in any::<u64>(),
        node_token in any::<u64>(),
        mount_slot in any::<u32>(),
        writer_id in any::<u128>(),
        term in any::<u64>(),
        state in 0u8..4,
        n_segments in 0usize..=RING_SEGMENTS_MAX,
        n_runs in 0usize..=GRANT_RUNS_MAX,
        n_slots in 0usize..=SLOT_PAGE_BUDGET,
        tail in any::<u64>(),
    ) {
        let state = match state {
            0 => AppenderState::Free,
            1 => AppenderState::Live,
            2 => AppenderState::Recovering,
            _ => AppenderState::Recovered,
        };
        let mut page = AppenderPage::free(id, generation);
        page.identity = AppenderIdentity { node_token, mount_slot, writer_id };
        page.term = term;
        page.state = state;
        page.is_manager = term % 2 == 0;
        page.home_volume = (term % 251) as u8;
        page.ledger_tail_seq = tail;
        page.ckpt_seq = tail.wrapping_add(1);
        page.segments = (0..n_segments as u64)
            .map(|i| ExtentRef { start: i * 0x4_0000, len: 0x4_0000 })
            .collect();
        page.grant = (0..n_runs as u64).map(|i| GrantRun { start: i * 8, len: 8 }).collect();
        page.slots = (0..n_slots as u16)
            .map(|i| SlotEntry {
                slot: i,
                state: if i % 2 == 0 { SlotEntryState::Live } else { SlotEntryState::Releasing },
                g: u32::from(i),
                slot_tree_extents: u32::from(i) * 3,
                root: RootPtr { addr: u64::from(i) * 0x1_0000, seq: tail.wrapping_add(u64::from(i)) },
                cursor: u64::from(i),
            })
            .collect();
        let img = page.encode().expect("encodes");
        prop_assert_eq!(AppenderPage::decode(&img).expect("decodes"), page.clone());
        let mut over = page.clone();
        over.slots = (0..=SLOT_PAGE_BUDGET as u16)
            .map(|i| SlotEntry {
                slot: i,
                state: SlotEntryState::Live,
                g: 0,
                slot_tree_extents: 0,
                root: RootPtr { addr: 0, seq: 0 },
                cursor: 0,
            })
            .collect();
        prop_assert!(over.encode().is_err());
        // A/B: the higher generation wins; tearing it falls back.
        let mut older = page.clone();
        older.generation = generation.wrapping_sub(1);
        let older_img = older.encode().expect("encodes");
        if generation > 0 {
            let (i, _) = newest_valid(&[older_img.clone(), img.clone()]).expect("valid");
            prop_assert_eq!(i, 1);
            let mut torn = img.clone();
            torn[40] ^= 0xFF;
            let (i, _) = newest_valid(&[older_img, torn]).expect("the predecessor");
            prop_assert_eq!(i, 0);
        }
    }

    /// The S9 publish frames (schema 13 — a co-writer's bytes at the ONE
    /// metadata authority, and the authority's bytes at every co-writer)
    /// are total over arbitrary bytes, and whatever decodes re-encodes to
    /// an equal frame.
    #[test]
    fn publish_wire_decoders_never_panic(data in prop::collection::vec(any::<u8>(), 0..512)) {
        if let Ok(f) = decode_request_frame(&data) {
            let re = encode_request_frame(&f).expect("an accepted request frame re-encodes");
            prop_assert_eq!(decode_request_frame(&re).expect("re-decodes"), f);
        }
        if let Ok(f) = decode_reply_frame(&data) {
            let re = encode_reply_frame(&f).expect("an accepted reply frame re-encodes");
            prop_assert_eq!(decode_reply_frame(&re).expect("re-decodes"), f);
        }
    }

    /// The symmetric manager's frames (design-symmetric-metadata §6.3,
    /// `MANAGER_SCHEMA` 1 under `CLUSTER_WIRE_SCHEMA` 5 — a joiner's bytes
    /// at the node holding the manager lease, and the manager's bytes at
    /// every appender) are total over arbitrary bytes, and whatever
    /// decodes re-encodes canonically to an equal frame.
    #[test]
    fn manager_call_decoders_never_panic(data in prop::collection::vec(any::<u8>(), 0..512)) {
        if let Ok(f) = decode_manager_request(&data) {
            let re = encode_manager_request(&f).expect("an accepted request frame re-encodes");
            prop_assert_eq!(decode_manager_request(&re).expect("re-decodes"), f);
            prop_assert_eq!(encode_manager_request(&decode_manager_request(&re).unwrap()).unwrap(), re);
        }
        if let Ok(f) = decode_manager_reply(&data) {
            let re = encode_manager_reply(&f).expect("an accepted reply frame re-encodes");
            prop_assert_eq!(decode_manager_reply(&re).expect("re-decodes"), f);
            prop_assert_eq!(encode_manager_reply(&decode_manager_reply(&re).unwrap()).unwrap(), re);
        }
    }

    /// Review round 1, Issue 2 — "bounded codec = bounded EXECUTION": a
    /// decoded integer is never an allocation authority at the manager's
    /// service edge either. Over arbitrary `ReturnExtents` runs and an
    /// arbitrary volume size, the validator rejects exactly the runs that
    /// overflow or lie outside the volume (touching nothing else), the
    /// record intersection materializes at most the RECORD's extents
    /// whatever the runs name (every one inside the record), and an
    /// explicit `ExtentGrant { want }` is clamped to the derivation's cap.
    #[test]
    fn manager_service_edge_is_bounded_by_durable_state(
        runs in prop::collection::vec((any::<u64>(), any::<u32>()), 0..16),
        total in 1u64..(1 << 40),
        record_seed in prop::collection::vec(0u64..(1 << 16), 0..64),
        want in any::<u32>(),
        cap in 8u64..(1 << 32),
    ) {
        use squeezefs::meta_backend::kv::appender::{
            clamp_grant_want, coalesce_runs, intersect_runs_with_record, runs_extent_count,
            validate_return_runs, GrantRun,
        };
        use squeezefs::meta_backend::kv::slot_state::ExtentGrantRecord;
        let runs: Vec<GrantRun> = runs
            .iter()
            .map(|&(start, len)| GrantRun { start, len })
            .collect();
        match validate_return_runs(&runs, total) {
            Ok(()) => {
                prop_assert!(runs.iter().all(|r| r.start + u64::from(r.len) <= total));
            }
            Err(offender) => {
                prop_assert!(offender
                    .start
                    .checked_add(u64::from(offender.len))
                    .is_none_or(|end| end > total));
                prop_assert!(runs.contains(&offender));
            }
        }
        // The coalesce (Issue 2's residual): bounded by the frame's own
        // run count, disjoint and ascending, naming no more than the input
        // and exactly the input's distinct extents.
        let coalesced = coalesce_runs(&runs);
        prop_assert!(coalesced.len() <= runs.len());
        prop_assert!(coalesced
            .windows(2)
            .all(|w| w[0].start + u64::from(w[0].len) < w[1].start));
        prop_assert!(runs_extent_count(&coalesced) <= runs_extent_count(&runs));
        let distinct: std::collections::BTreeSet<u64> = runs
            .iter()
            .filter(|r| r.len <= 64 && r.start.checked_add(u64::from(r.len)).is_some())
            .flat_map(|r| r.start..r.start + u64::from(r.len))
            .collect();
        if runs.iter().all(|r| r.len <= 64) {
            prop_assert_eq!(runs_extent_count(&coalesced), distinct.len() as u64);
        }
        let record = ExtentGrantRecord::from_extents(record_seed.iter().map(|e| *e % total));
        let inside = intersect_runs_with_record(&runs, &record);
        prop_assert!(inside.len() as u64 <= record.len());
        prop_assert!(inside.iter().all(|e| record.contains(*e)));
        prop_assert!(inside.windows(2).all(|w| w[0] < w[1]), "strictly ascending — no dedup step");
        // Every extent a run names that the record holds IS in the list.
        for r in runs.iter().filter(|r| r.len <= 64) {
            for e in r.start..r.start.saturating_add(u64::from(r.len)) {
                prop_assert_eq!(record.contains(e), inside.binary_search(&e).is_ok());
            }
        }
        let w = clamp_grant_want(want, cap);
        prop_assert!(w <= cap && w > 0);
        prop_assert_eq!(clamp_grant_want(0, cap), cap);
    }

    /// The cluster wire's `RpcFrame` reader (every distributed plane's
    /// transport) and the S8 verb-body decoders are total over an
    /// arbitrary byte STREAM under every class cap; an arbitrary tag never
    /// verifies on the authenticated path.
    #[test]
    fn cluster_wire_decoders_never_panic(data in prop::collection::vec(any::<u8>(), 0..512)) {
        for class in [FrameClass::Handshake, FrameClass::Control, FrameClass::Bulk] {
            let mut cur = std::io::Cursor::new(&data[..]);
            for _ in 0..16 {
                match read_plain_frame::<_, RpcFrame>(&mut cur, class.cap(), None) {
                    Ok(Some(frame)) => {
                        let mut re = Vec::new();
                        write_plain_frame(&mut re, class, &frame)
                            .expect("an accepted frame re-encodes under its class");
                        let mut cur2 = std::io::Cursor::new(&re[..]);
                        let again: RpcFrame = read_plain_frame(&mut cur2, class.cap(), None)
                            .expect("reads")
                            .expect("one frame");
                        let mut re2 = Vec::new();
                        write_plain_frame(&mut re2, class, &again).expect("re-encodes");
                        prop_assert_eq!(re2, re, "canonical encoding");
                    }
                    Ok(None) | Err(_) => break,
                }
            }
        }
        let key = session_key(&data, "p", "n0", "n1", None);
        let (_, mut rx) = session_framers(&key, Role::Peer);
        let mut cur = std::io::Cursor::new(&data[..]);
        prop_assert!(!matches!(
            rx.recv::<_, RpcFrame>(&mut cur, FrameClass::Bulk.cap(), None),
            Ok(Some(_))
        ));
        let _ = decode_request(&data);
        let _ = decode_reply(&data);
        if let Ok(f) = decode_reclaim(&data) {
            let re = encode_reclaim(&f).expect("re-encodes");
            prop_assert_eq!(decode_reclaim(&re).expect("re-decodes"), f);
        }
    }

    /// `hex_decode` is total over arbitrary Unicode (the 1.2 find above,
    /// as a law) and exact: whatever it accepts is `hex_encode`'s own
    /// output up to case.
    #[test]
    fn cluster_wire_hex_decode_never_panics(s in "\\PC{0,24}") {
        if let Some(bytes) = hex_decode(&s) {
            prop_assert_eq!(bytes.len() * 2, s.len());
            prop_assert_eq!(hex_encode(&bytes), s.to_ascii_lowercase());
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

    /// A multi-call publish request frame (D-1b) round-trips — the kvmap
    /// `MigrateBlockMap` train with its whole entries map, refs frame and
    /// `base_gen` among the calls.
    #[test]
    fn publish_request_frame_round_trips(
        client in "[a-z0-9:-]{1,24}",
        calls in prop::collection::vec(arb_publish_call(), 1..6),
        pack_group in any::<bool>(),
    ) {
        let frame = PublishRequestFrame { schema: PUBLISH_SCHEMA, client, calls, pack_group };
        let enc = encode_request_frame(&frame).expect("encodes");
        prop_assert_eq!(decode_request_frame(&enc).expect("decodes"), frame);
    }

    /// Every `ManagerCall` — `JoinAppender` with its KD-MW-2 identity,
    /// `ExtentGrant`, `ReturnExtents` with its runs — round-trips through
    /// the bounded codec inside a request frame.
    #[test]
    fn manager_request_frame_round_trips(
        request_id in any::<u64>(),
        call in arb_manager_call(),
    ) {
        let frame = ManagerRequestFrame { schema: MANAGER_SCHEMA, request_id, call };
        let enc = encode_manager_request(&frame).expect("encodes");
        prop_assert_eq!(decode_manager_request(&enc).expect("decodes"), frame);
    }

    /// Every `ManagerReply` — `Joined` (page, ring table, grant, `already`),
    /// `Granted`, `Returned { cleared, already }`, `Refused` — round-trips.
    #[test]
    fn manager_reply_frame_round_trips(
        request_id in any::<u64>(),
        reply in arb_manager_reply(),
    ) {
        let frame = ManagerReplyFrame { schema: MANAGER_SCHEMA, request_id, reply };
        let enc = encode_manager_reply(&frame).expect("encodes");
        prop_assert_eq!(decode_manager_reply(&enc).expect("decodes"), frame);
    }

    /// A publish reply frame round-trips one outcome per call, `MapMigrated
    /// { recomputed, gen, … }` included, plus its lane-free notices.
    #[test]
    fn publish_reply_frame_round_trips(
        outcomes in prop::collection::vec(arb_publish_outcome(), 0..6),
        lane_frees in prop::collection::vec(
            (any::<u64>(), any::<u64>(), any::<u64>()).prop_map(
                |(vol_tag, block_idx, after_grants)| WireLaneFree { vol_tag, block_idx, after_grants }
            ),
            0..6,
        ),
    ) {
        let frame = PublishReplyFrame { schema: PUBLISH_SCHEMA, outcomes, lane_frees };
        let enc = encode_reply_frame(&frame).expect("encodes");
        prop_assert_eq!(decode_reply_frame(&enc).expect("decodes"), frame);
    }

    /// An `RpcFrame` rides the authenticated path: the peer verifies and
    /// decodes what the coordinator sent, and a reflected or replayed copy
    /// fails verification instead of being skipped.
    #[test]
    fn cluster_wire_authenticated_frame_round_trips(
        secret in prop::collection::vec(any::<u8>(), 0..48),
        id in any::<u64>(),
        verb in any::<u16>(),
        body in prop::collection::vec(any::<u8>(), 0..256),
    ) {
        let key = session_key(&secret, "peer", "n0", "n1", Some(&secret));
        let (mut c_tx, mut c_rx) = session_framers(&key, Role::Coordinator);
        let (_, mut p_rx) = session_framers(&key, Role::Peer);
        let frame = RpcFrame::Call { id, verb, body: body.clone() };
        let mut wire = Vec::new();
        c_tx.send(&mut wire, FrameClass::Control, &frame).expect("sends");
        let mut cur = std::io::Cursor::new(&wire[..]);
        let got: RpcFrame = p_rx
            .recv(&mut cur, FrameClass::Control.cap(), None)
            .expect("verifies")
            .expect("one frame");
        match got {
            RpcFrame::Call { id: i, verb: v, body: b } => {
                prop_assert_eq!(i, id);
                prop_assert_eq!(v, verb);
                prop_assert_eq!(b, body);
            }
            other => prop_assert!(false, "decoded a different frame: {other:?}"),
        }
        let mut cur = std::io::Cursor::new(&wire[..]);
        prop_assert!(c_rx.recv::<_, RpcFrame>(&mut cur, FrameClass::Control.cap(), None).is_err(),
            "reflection must fail");
        let mut cur = std::io::Cursor::new(&wire[..]);
        prop_assert!(p_rx.recv::<_, RpcFrame>(&mut cur, FrameClass::Control.cap(), None).is_err(),
            "replay must fail");
    }

    /// The S8 reclaim frame — the smallest verb body — round-trips.
    #[test]
    fn meta_ship_reclaim_frame_round_trips(
        schema in any::<u32>(),
        client_epoch in any::<u64>(),
        inos in prop::collection::vec(any::<u64>(), 0..32),
    ) {
        let f = ReclaimFrame { schema, client_epoch, inos };
        let enc = encode_reclaim(&f).expect("encodes");
        prop_assert_eq!(decode_reclaim(&enc).expect("decodes"), f);
    }
}

// ---------------------------------------------------------------------------
// manager-wire generators (design-symmetric-metadata §6.3)
// ---------------------------------------------------------------------------

fn arb_runs() -> impl Strategy<Value = Vec<(u64, u32)>> {
    prop::collection::vec((any::<u64>(), any::<u32>()), 0..6)
}

fn arb_manager_call() -> impl Strategy<Value = ManagerCall> {
    prop_oneof![
        (any::<u64>(), any::<u32>(), any::<u128>(), any::<u64>()).prop_map(
            |(node_token, mount_slot, writer_id, ring_want_bytes)| ManagerCall::JoinAppender {
                identity: WireIdentity {
                    node_token,
                    mount_slot,
                    writer_id,
                },
                ring_want_bytes,
            }
        ),
        (any::<u32>(), any::<u32>())
            .prop_map(|(appender_id, want)| ManagerCall::ExtentGrant { appender_id, want }),
        (any::<u32>(), arb_runs())
            .prop_map(|(appender_id, runs)| ManagerCall::ReturnExtents { appender_id, runs }),
    ]
}

fn arb_manager_reply() -> impl Strategy<Value = ManagerReply> {
    prop_oneof![
        (
            any::<u32>(),
            any::<u64>(),
            prop::collection::vec((any::<u64>(), any::<u64>()), 0..9),
            arb_runs(),
            any::<bool>(),
        )
            .prop_map(|(appender_id, page_addr, ring_segments, grant, already)| {
                ManagerReply::Joined {
                    appender_id,
                    page_addr,
                    ring_segments,
                    grant,
                    already,
                }
            }),
        arb_runs().prop_map(|runs| ManagerReply::Granted { runs }),
        (any::<u64>(), any::<u64>())
            .prop_map(|(cleared, already)| ManagerReply::Returned { cleared, already }),
        "[ -~]{0,64}".prop_map(|reason| ManagerReply::Refused { reason }),
    ]
}

// ---------------------------------------------------------------------------
// publish-wire generators
// ---------------------------------------------------------------------------

fn arb_ref() -> impl Strategy<Value = WireBlockRefOp> {
    (
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u32>(),
        any::<bool>(),
    )
        .prop_map(
            |(vol_tag, block_idx, owner_ino, block_index, take)| WireBlockRefOp {
                vol_tag,
                block_idx,
                owner_ino,
                block_index,
                take,
            },
        )
}

fn arb_publish_call() -> impl Strategy<Value = PublishCall> {
    let refs = || prop::collection::vec(arb_ref(), 0..4);
    let bytes = || prop::collection::vec(any::<u8>(), 0..64);
    prop_oneof![
        (
            any::<u64>(),
            bytes(),
            any::<u64>(),
            refs(),
            any::<u64>(),
            any::<u64>()
        )
            .prop_map(|(ino, layout, size, refs, lease_epoch, request_id)| {
                PublishCall::SetLayoutAndSize {
                    ino,
                    layout,
                    size,
                    refs,
                    lease_epoch,
                    request_id,
                }
            }),
        (any::<u64>(), refs(), any::<u64>(), any::<u64>()).prop_map(
            |(ino, refs, lease_epoch, request_id)| PublishCall::CommitBlockRefs {
                ino,
                refs,
                lease_epoch,
                request_id,
            }
        ),
        (
            any::<u64>(),
            prop::collection::vec(any::<u64>(), 0..8),
            any::<u64>(),
            any::<u64>()
        )
            .prop_map(|(vol_tag, blocks, lease_epoch, request_id)| {
                PublishCall::FreeBlocks {
                    vol_tag,
                    blocks,
                    lease_epoch,
                    request_id,
                }
            }),
        (
            any::<u64>(),
            bytes(),
            any::<u64>(),
            prop::collection::vec((any::<u32>(), "[a-z0-9:_-]{1,16}"), 0..8),
            refs(),
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
        )
            .prop_map(
                |(ino, layout, size, entries, refs, base_gen, lease_epoch, request_id)| {
                    PublishCall::MigrateBlockMap {
                        ino,
                        layout,
                        size,
                        entries,
                        refs,
                        base_gen,
                        lease_epoch,
                        request_id,
                    }
                }
            ),
    ]
}

/// The schema-15 freed set: bounded, the durable identity pair.
fn arb_freed() -> impl Strategy<Value = Vec<WireFreedBlock>> {
    prop::collection::vec(
        (any::<u64>(), any::<u64>())
            .prop_map(|(vol_tag, block_idx)| WireFreedBlock { vol_tag, block_idx }),
        0..8,
    )
}

fn arb_publish_outcome() -> impl Strategy<Value = PublishCallOutcome> {
    prop_oneof![
        Just(PublishCallOutcome::Done(Ok(PublishReply::Unit))),
        (any::<bool>(), arb_freed()).prop_map(|(recomputed, freed)| {
            PublishCallOutcome::Done(Ok(PublishReply::PutDone { recomputed, freed }))
        }),
        (
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            any::<bool>(),
            arb_freed(),
            any::<u64>()
        )
            .prop_map(
                |(records, record_bytes, preexisting, recomputed, freed, gen)| {
                    PublishCallOutcome::Done(Ok(PublishReply::MapMigrated {
                        records,
                        record_bytes,
                        preexisting,
                        recomputed,
                        freed,
                        gen,
                    }))
                }
            ),
        (any::<i32>(), "\\PC{0,32}")
            .prop_map(|(errno, msg)| PublishCallOutcome::Done(Err(WireError { errno, msg }))),
        (any::<u16>(), "\\PC{0,32}")
            .prop_map(|(status, detail)| PublishCallOutcome::Refused { status, detail }),
    ]
}
