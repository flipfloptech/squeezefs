//! PR K6a/K6b integration tests: superblock v3 + the sector-0 version
//! gate, the offline bulk image builder, and the `KvMetaBackend` —
//! design `docs/design-cow-kv-metadata.md` §4.1/§4.5/§5.1/§6.1 and the
//! pre-resolved OQ 1 (ring clamp) / OQ 2 (dual atomicity fields) decisions.
//!
//! Contracts pinned:
//! - **Backend conformance**: one population, built as a v3 image; every
//!   lookup/getattr/readdir/getxattr/listxattr assertion (these cases
//!   originally ran against both formats; the v2 leg was deleted with v2
//!   support — the assertions are unchanged).
//! - **The §6.1 version gate**: blank, legacy-v2, v3, foreign-magic, and
//!   future-version sector 0s classify loudly and distinctly — a v2
//!   superblock refuses with the precise "no longer supported" message —
//!   and unknown incompat feature bits refuse naming the bits; the v3
//!   checksum covers the whole sector.
//! - **Resolved OQ 1**: `journal_ring_len = clamp(volume/64, 8, 32 MiB)`;
//!   `--meta-node-kib` validation (allowed set, 64 KiB floor, sub-256 KiB
//!   warning naming the reduced `node_size/4` record-value cap).
//! - **Resolved OQ 2**: v3 volumes report the `cow-checksummed` contract
//!   class with the physical probe classification alongside.
//! - **Builder determinism**: identical descriptions (fixed seed/uuid)
//!   build byte-identical images and equal post-fold digest walks (§4.10).
//! - **Mount = SB → ledger → bitmap → replay**: journal entries written
//!   into a built image are recovered at open — read-only, into the K5
//!   cache, per-key LWW by seq — and the §4.8 `next_ino` watermark
//!   advances over replayed inos.
//! - **v3 format guards**: the preflight policy (already formatted ⇒
//!   refused without `--force`).
//!
//! Torn-superblock (loud) and torn-newest-ledger (predecessor fallback)
//! at the backend level live in `tests/crash_contract_tests.rs` (the
//! shared fault-injection harness).

use squeezefs::meta_backend::atomicity::{
    probe_meta_volume, AtomicityClass, META_VOLUME_ATOMICITY_COW,
};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{
    digest_backend, format_v3, BuilderConfig, FormatV3Options, ImageBuilder, ROOT_INO,
};
use squeezefs::meta_backend::kv::journal::{checkpoint_reserve_bytes, entry_len_for, JournalRing};
use squeezefs::meta_backend::kv::journal_core::AdmissionClass;
use squeezefs::meta_backend::kv::node::record_value_cap;
use squeezefs::meta_backend::kv::record::{
    dentry_key, dentry_name_hash54, encode_readdir_cookie, inode_key, DentryValue, InodeDelta,
    InodeValue, Record, TREE_DENTRIES, TREE_INODES,
};
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, journal_ring_len, validate_node_kib, write_superblock_v3, SuperblockV3,
    VolumeFormat, FEATURE_INCOMPAT_KV_V3, JOURNAL_RING_MAX, JOURNAL_RING_MIN,
    SUPERBLOCK_V3_VERSION,
};
use squeezefs::meta_backend::kv::KvError;
use squeezefs::meta_backend::{open_volume_for_mount, Metadata};
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// Harness: one population, two formats.
// ---------------------------------------------------------------------------

/// v3 test volumes: small node size + overridden 1 MiB ring keep them tiny.
const V3_VOL_LEN: u64 = 64 * 1024 * 1024;
const V3_NODE_SIZE: usize = 64 * 1024;
const V3_RING_LEN: u64 = 1024 * 1024;
/// Fixed identity for deterministic images.
const TEST_SEED: u64 = 0x5EED_CAFE_F00D_D00D;
const TEST_UUID: [u8; 16] = *b"kv-backend-test!";

struct Population {
    backend: Arc<KvMetaBackend>,
    inos: HashMap<&'static str, u64>,
    _file: NamedTempFile,
}

fn big_xattr() -> Vec<u8> {
    vec![0xAB; 6000]
}

fn v3_builder_config() -> BuilderConfig {
    BuilderConfig {
        node_size: V3_NODE_SIZE,
        journal_len_override: Some(V3_RING_LEN),
        hash_seed: TEST_SEED,
        uuid: TEST_UUID,
    }
}

/// The shared description ([`describe_v3`]): /docs (0750 1000:1000),
/// /docs/readme.txt (0644 1000:1000, 4096 B, two xattrs), /hello.bin
/// (0600 0:0, 0 B), /empty (0755 0:0), /hard.lnk = hard link to hello.bin.
///
/// Timestamps stamped on readme.txt by [`describe_v3`] (ns) — builder
/// times default to 0 (the determinism contract) and are settable.
const README_TIMES: (u64, u64, u64) = (11_111, 22_222, 33_333);

fn describe_v3() -> (ImageBuilder, HashMap<&'static str, u64>) {
    let mut b = ImageBuilder::new(v3_builder_config()).unwrap();
    let mut inos = HashMap::new();
    let docs = b.add_dir(ROOT_INO, "docs", 0o750, 1000, 1000).unwrap();
    inos.insert("docs", docs);
    let readme = b
        .add_file(docs, "readme.txt", 0o644, 1000, 1000, 4096)
        .unwrap();
    inos.insert("readme.txt", readme);
    b.set_times(readme, README_TIMES.0, README_TIMES.1, README_TIMES.2)
        .unwrap();
    b.set_xattr(readme, "user.color", b"blue").unwrap();
    b.set_xattr(readme, "user.big", &big_xattr()).unwrap();
    let hello = b.add_file(ROOT_INO, "hello.bin", 0o600, 0, 0, 0).unwrap();
    inos.insert("hello.bin", hello);
    let empty = b.add_dir(ROOT_INO, "empty", 0o755, 0, 0).unwrap();
    inos.insert("empty", empty);
    b.add_link(hello, ROOT_INO, "hard.lnk").unwrap();
    assert_eq!(b.inode_count(), 5, "root + docs + readme + hello + empty");
    (b, inos)
}

async fn population() -> Population {
    let file = NamedTempFile::new().expect("temp volume");
    file.as_file().set_len(V3_VOL_LEN).unwrap();
    let (b, inos) = describe_v3();
    b.build(file.path(), V3_VOL_LEN)
        .await
        .expect("build v3 image");
    let backend = open_volume_for_mount(file.path().to_str().unwrap())
        .await
        .expect("open_volume_for_mount");
    Population {
        backend,
        inos,
        _file: file,
    }
}

// ---------------------------------------------------------------------------
// Read-side conformance: the same assertions over both backends.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_lookup_and_getattr() {
    let p = population().await;
    let b = &p.backend;

    let root = b.getattr(ROOT_INO).await.expect("root getattr");
    assert_eq!(root.ino, ROOT_INO);
    assert_eq!(
        root.mode & libc::S_IFMT,
        libc::S_IFDIR,
        "root is a directory"
    );

    let docs = b.lookup(ROOT_INO, "docs").await.expect("lookup docs");
    assert_eq!(docs.ino, p.inos["docs"]);
    assert_eq!(docs.mode, libc::S_IFDIR | 0o750);
    assert_eq!((docs.uid, docs.gid), (1000, 1000));

    let readme = b
        .lookup(docs.ino, "readme.txt")
        .await
        .expect("lookup readme");
    assert_eq!(readme.ino, p.inos["readme.txt"]);
    assert_eq!(readme.mode, libc::S_IFREG | 0o644);
    assert_eq!(readme.size, 4096, "size must round-trip");
    assert_eq!(readme.nlink, 1);

    // getattr agrees with lookup.
    let again = b.getattr(readme.ino).await.expect("getattr readme");
    assert_eq!(
        (again.mode, again.uid, again.gid, again.size),
        (readme.mode, readme.uid, readme.gid, readme.size)
    );

    // Missing name / missing ino fail loud on both formats.
    assert!(
        b.lookup(ROOT_INO, "no-such-entry").await.is_err(),
        "lookup of a missing name must error"
    );
    assert!(
        b.lookup(999_999, "docs").await.is_err(),
        "lookup under a missing parent must error"
    );
    assert!(
        b.getattr(999_999).await.is_err(),
        "getattr of a missing ino must error"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_readdir_sets() {
    let p = population().await;
    let b = &p.backend;

    let mut root_names: Vec<(String, u64, u32)> = b
        .readdir(ROOT_INO, 0, usize::MAX)
        .await
        .expect("readdir root")
        .into_iter()
        .map(|d| (d.name, d.ino, d.file_type))
        .collect();
    root_names.sort();
    assert_eq!(
        root_names,
        vec![
            ("docs".to_string(), p.inos["docs"], libc::S_IFDIR),
            ("empty".to_string(), p.inos["empty"], libc::S_IFDIR),
            ("hard.lnk".to_string(), p.inos["hello.bin"], libc::S_IFREG),
            ("hello.bin".to_string(), p.inos["hello.bin"], libc::S_IFREG),
        ],
        "root readdir must return exactly the population (backend layer emits no ./..)"
    );

    let docs_entries = b
        .readdir(p.inos["docs"], 0, usize::MAX)
        .await
        .expect("readdir docs");
    assert_eq!(docs_entries.len(), 1);
    assert_eq!(docs_entries[0].name, "readme.txt");

    assert!(
        b.readdir(p.inos["empty"], 0, usize::MAX)
            .await
            .expect("readdir empty dir")
            .is_empty(),
        "an empty directory lists empty"
    );
    assert!(
        b.readdir(424_242, 0, usize::MAX)
            .await
            .expect("readdir of an unknown ino")
            .is_empty(),
        "readdir of an ino with no dentries is empty on both formats (the v2 contract)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_xattr_surface() {
    let p = population().await;
    let b = &p.backend;
    let readme = p.inos["readme.txt"];

    assert_eq!(
        b.getxattr(readme, "user.color")
            .await
            .expect("getxattr")
            .as_deref(),
        Some(b"blue".as_slice())
    );
    assert_eq!(
        b.getxattr(readme, "user.big").await.expect("getxattr big"),
        Some(big_xattr()),
        "a multi-KiB value must round-trip byte-exact"
    );
    assert_eq!(
        b.getxattr(readme, "user.absent")
            .await
            .expect("absent name"),
        None,
        "an absent xattr name is Ok(None), not an error"
    );

    let mut names = b.listxattr(readme).await.expect("listxattr");
    names.sort();
    assert_eq!(
        names,
        vec!["user.big".to_string(), "user.color".to_string()]
    );

    assert!(
        b.listxattr(p.inos["hello.bin"])
            .await
            .expect("no-xattr ino")
            .is_empty(),
        "an ino with no xattrs lists empty"
    );
    assert_eq!(
        b.getxattr(999_999, "user.color")
            .await
            .expect("missing ino"),
        None,
        "getxattr of a missing ino degrades to Ok(None) (the v2 contract)"
    );
    assert!(
        b.listxattr(999_999)
            .await
            .expect("missing ino listxattr")
            .is_empty(),
        "listxattr of a missing ino degrades to empty (the v2 contract)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_hard_links() {
    let p = population().await;
    let b = &p.backend;

    let by_name = b
        .lookup(ROOT_INO, "hello.bin")
        .await
        .expect("original name");
    let by_link = b.lookup(ROOT_INO, "hard.lnk").await.expect("link name");
    assert_eq!(by_name.ino, by_link.ino, "both names resolve to one ino");
    assert_eq!(by_name.nlink, 2, "nlink counts both names");
}

// ---------------------------------------------------------------------------
// The §6.1 version gate.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn version_gate_distinguishes_blank_v2_v3_foreign_future() {
    // Blank: classification succeeds (format's cue); mount refuses loud
    // with the actionable message.
    let blank = NamedTempFile::new().unwrap();
    blank.as_file().set_len(8 * 1024 * 1024).unwrap();
    assert!(matches!(
        classify_volume(blank.path())
            .await
            .expect("blank classifies"),
        VolumeFormat::Blank
    ));
    let err = open_volume_for_mount(blank.path().to_str().unwrap())
        .await
        .expect_err("mounting a blank volume must fail loud")
        .to_string();
    assert!(
        err.contains("format"),
        "the blank-volume refusal must point at `squeezefs format`, got: {err}"
    );

    // Legacy v2: SqueezeFS magic + version 2 (crafted bytes — no v2
    // writer exists anymore) classifies V2Legacy; the mount refuses with
    // the precise "no longer supported" message, never "run format" or
    // "foreign".
    let v2 = NamedTempFile::new().unwrap();
    v2.as_file().set_len(8 * 1024 * 1024).unwrap();
    let mut legacy_sb = Vec::with_capacity(12);
    legacy_sb.extend_from_slice(b"METALV01");
    legacy_sb.extend_from_slice(&2u32.to_le_bytes());
    squeezefs::uring_fs::write_at(v2.path(), 0, bytes::Bytes::from(legacy_sb))
        .await
        .unwrap();
    assert!(matches!(
        classify_volume(v2.path()).await.expect("v2 classifies"),
        VolumeFormat::V2Legacy
    ));
    let err = open_volume_for_mount(v2.path().to_str().unwrap())
        .await
        .expect_err("mounting a legacy v2 volume must refuse loud")
        .to_string();
    assert!(
        err.contains("no longer supported") && err.contains("v2"),
        "the v2 refusal must be the precise 'no longer supported' message, got: {err}"
    );
    assert!(
        err.contains("reformat") || err.contains("format"),
        "the v2 refusal must point at the reformat path, got: {err}"
    );

    // v3: classifies as V3 with the geometry parsed.
    let v3 = NamedTempFile::new().unwrap();
    v3.as_file().set_len(V3_VOL_LEN).unwrap();
    ImageBuilder::new(v3_builder_config())
        .unwrap()
        .build(v3.path(), V3_VOL_LEN)
        .await
        .unwrap();
    match classify_volume(v3.path()).await.expect("v3 classifies") {
        VolumeFormat::V3(sb) => {
            assert_eq!(sb.node_size as usize, V3_NODE_SIZE);
            assert_eq!(sb.journal.len, V3_RING_LEN);
            assert_eq!(sb.hash_seed, TEST_SEED);
            assert_ne!(
                sb.features_incompat & FEATURE_INCOMPAT_KV_V3,
                0,
                "every v3 volume carries the KV_V3 incompat bit"
            );
        }
        other => panic!("a v3 volume must classify V3, got {other:?}"),
    }

    // Foreign magic: loud, named as foreign/corrupt — never "run format".
    let foreign = NamedTempFile::new().unwrap();
    foreign.as_file().set_len(8 * 1024 * 1024).unwrap();
    squeezefs::uring_fs::write_at(
        foreign.path(),
        0,
        bytes::Bytes::from_static(b"EXT4SUPRJUNKJUNK"),
    )
    .await
    .unwrap();
    let err = classify_volume(foreign.path())
        .await
        .expect_err("foreign magic must classify loud")
        .to_string();
    assert!(
        err.contains("foreign") || err.contains("magic"),
        "foreign-volume error must name the magic mismatch, got: {err}"
    );

    // Future version: the version check precedes checksum verification —
    // report "upgrade squeezefs", never a checksum mismatch.
    let future = NamedTempFile::new().unwrap();
    future.as_file().set_len(V3_VOL_LEN).unwrap();
    ImageBuilder::new(v3_builder_config())
        .unwrap()
        .build(future.path(), V3_VOL_LEN)
        .await
        .unwrap();
    // DUR-5: sector-0 damage alone is survivable now (the redundant copy
    // carries the volume), so the gate is exercised by stamping the
    // future version on BOTH slots.
    let future_backup = squeezefs::meta_backend::kv::superblock::backup_offset(V3_VOL_LEN)
        .expect("a fresh format reserves the backup slot");
    for off in [8u64, future_backup + 8] {
        squeezefs::uring_fs::write_at(
            future.path(),
            off,
            bytes::Bytes::copy_from_slice(&99u32.to_le_bytes()),
        )
        .await
        .unwrap();
    }
    let err = classify_volume(future.path())
        .await
        .expect_err("a future version must refuse loud")
        .to_string();
    assert!(
        err.contains("upgrade"),
        "future-version refusal must say 'upgrade squeezefs', got: {err}"
    );
    assert!(
        !err.contains("checksum"),
        "version gating must precede checksum verification, got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_superblock_checksum_covers_the_whole_sector() {
    let file = NamedTempFile::new().unwrap();
    file.as_file().set_len(V3_VOL_LEN).unwrap();
    ImageBuilder::new(v3_builder_config())
        .unwrap()
        .build(file.path(), V3_VOL_LEN)
        .await
        .unwrap();

    // Flip one byte in the sector's zero padding, far past the struct
    // fields: the whole-sector checksum must still catch it.
    squeezefs::uring_fs::write_at(file.path(), 900, bytes::Bytes::from_static(&[0xFF]))
        .await
        .unwrap();
    // DUR-5 first: with only sector 0 damaged the redundant copy carries
    // the volume — that IS the durability fix, and it must not mask the
    // checksum contract below.
    assert!(
        classify_volume(file.path()).await.is_ok(),
        "sector-0 padding corruption alone must recover from the redundant copy"
    );
    let backup = squeezefs::meta_backend::kv::superblock::backup_offset(V3_VOL_LEN)
        .expect("a fresh format reserves the backup slot");
    squeezefs::uring_fs::write_at(
        file.path(),
        backup + 900,
        bytes::Bytes::from_static(&[0xFF]),
    )
    .await
    .unwrap();
    let err = classify_volume(file.path())
        .await
        .expect_err("padding corruption must fail the whole-sector checksum")
        .to_string();
    assert!(
        err.contains("checksum"),
        "corruption must surface as a checksum mismatch, got: {err}"
    );
    let err = KvMetaBackend::open(file.path())
        .await
        .expect_err("mount must refuse the corrupt superblock")
        .to_string();
    assert!(
        err.contains("checksum"),
        "mount error must carry the cause, got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_unknown_incompat_bit_refuses_naming_it_and_unknown_ro_does_not() {
    let file = NamedTempFile::new().unwrap();
    file.as_file().set_len(V3_VOL_LEN).unwrap();
    ImageBuilder::new(v3_builder_config())
        .unwrap()
        .build(file.path(), V3_VOL_LEN)
        .await
        .unwrap();
    let sb = match classify_volume(file.path()).await.unwrap() {
        VolumeFormat::V3(sb) => sb,
        other => panic!("expected V3, got {other:?}"),
    };

    // Unknown incompat bit ⇒ refuse mount naming the bit (§6.1).
    let mut incompat = sb.clone();
    incompat.features_incompat |= 1 << 9;
    write_superblock_v3(file.path(), &incompat).await.unwrap();
    let err = classify_volume(file.path())
        .await
        .expect_err("unknown incompat bits must refuse the mount")
        .to_string();
    assert!(
        err.contains("bit 9") || err.contains("0x200"),
        "the refusal must name the unknown bit, got: {err}"
    );

    // Pre-watermark v3 (incompat bit 1 absent) ⇒ refuse loud, naming the
    // reformat requirement (Finding A: such a volume's node-seq mints
    // re-minted across clean remounts; its ledger slots no longer decode
    // either — forward-only, no shims).
    let mut pre_watermark = sb.clone();
    pre_watermark.features_incompat = FEATURE_INCOMPAT_KV_V3;
    write_superblock_v3(file.path(), &pre_watermark)
        .await
        .unwrap();
    let err = classify_volume(file.path())
        .await
        .expect_err("pre-watermark v3 volumes must refuse the mount")
        .to_string();
    assert!(
        err.contains("reformat required") && err.contains("watermark"),
        "the refusal must name the watermark gate and the remedy, got: {err}"
    );

    // Unknown RO bit ⇒ classification and the read-side mount succeed
    // (§4.11: read-only semantics; K6a's read path has nothing to
    // withhold — K6b withholds the write path).
    let mut ro = sb.clone();
    ro.features_ro |= 1 << 3;
    write_superblock_v3(file.path(), &ro).await.unwrap();
    match classify_volume(file.path())
        .await
        .expect("unknown ro bits still classify")
    {
        VolumeFormat::V3(got) => {
            assert_eq!(got.unknown_ro(), 1 << 3);
            assert_eq!(got.unknown_incompat(), 0);
        }
        other => panic!("expected V3, got {other:?}"),
    }
    let be = KvMetaBackend::open(file.path())
        .await
        .expect("the read side mounts a volume with unknown ro bits");
    assert_eq!(be.superblock().unknown_ro(), 1 << 3);
}

// ---------------------------------------------------------------------------
// Resolved OQ 1: ring clamp + node-kib knob.
// ---------------------------------------------------------------------------

#[test]
fn journal_ring_clamp_math() {
    let mib = 1024 * 1024;
    // Floor: a 256 MiB volume wants 4 MiB (256/64) → clamped up to 8 MiB.
    assert_eq!(journal_ring_len(256 * mib), JOURNAL_RING_MIN);
    // Linear region: 1 GiB / 64 = 16 MiB.
    assert_eq!(journal_ring_len(1024 * mib), 16 * mib);
    // Ceiling: 4 GiB / 64 = 64 MiB → clamped down to 32 MiB.
    assert_eq!(journal_ring_len(4096 * mib), JOURNAL_RING_MAX);
    // Way past the ceiling stays at the ceiling.
    assert_eq!(journal_ring_len(100 * 1024 * 1024 * mib), JOURNAL_RING_MAX);
    // The clamp always yields whole 4 KiB pages.
    for vol in [256 * mib, 999 * mib + 12345, 1024 * mib, 4096 * mib] {
        assert_eq!(
            journal_ring_len(vol) % 4096,
            0,
            "ring length must be page-aligned"
        );
    }
}

#[test]
fn node_kib_knob_floor_warning_and_allowed_set() {
    // The allowed set (§5.1): 64/128/256/512/1024.
    for kib in [64u32, 128, 256, 512, 1024] {
        let (bytes, _warn) = validate_node_kib(kib).expect("allowed node-kib value");
        assert_eq!(bytes, kib as usize * 1024);
    }
    // Sub-256 KiB values warn, naming the reduced record-value cap
    // node_size/4 (§4.2 / §5.1).
    let (bytes, warn) = validate_node_kib(64).unwrap();
    let warn = warn.expect("64 KiB must carry the reduced-cap warning");
    assert!(
        warn.contains(&format!("{}", record_value_cap(bytes))),
        "warning must state the resulting cap {}, got: {warn}",
        record_value_cap(bytes)
    );
    assert!(
        validate_node_kib(128).unwrap().1.is_some(),
        "128 KiB warns too"
    );
    // 256 and above do not warn.
    assert!(validate_node_kib(256).unwrap().1.is_none());
    assert!(validate_node_kib(1024).unwrap().1.is_none());
    // Below the 64 KiB floor and off-set values are refused.
    assert!(validate_node_kib(32).is_err(), "below the 64 KiB floor");
    assert!(validate_node_kib(0).is_err());
    assert!(validate_node_kib(100).is_err(), "not in the allowed set");
    assert!(validate_node_kib(2048).is_err(), "above the 1 MiB ceiling");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_plan_uses_clamp_default_and_honors_override() {
    // Default: the OQ 1 clamp (floor at this volume size).
    let sb = SuperblockV3::plan(V3_VOL_LEN, 256 * 1024, None, TEST_UUID, TEST_SEED)
        .expect("plan with defaults");
    assert_eq!(sb.journal.len, journal_ring_len(V3_VOL_LEN));
    assert_eq!(
        sb.journal.len, JOURNAL_RING_MIN,
        "64 MiB volume sits on the floor"
    );

    // Override: `--meta-journal-mb` replaces the clamp.
    let sb = SuperblockV3::plan(
        V3_VOL_LEN,
        256 * 1024,
        Some(2 * 1024 * 1024),
        TEST_UUID,
        TEST_SEED,
    )
    .expect("plan with override");
    assert_eq!(sb.journal.len, 2 * 1024 * 1024);

    // Geometry is ordered and non-overlapping: SB | ledger | ring |
    // bitmap | heap, with the heap holding at least one extent.
    assert!(sb.root_ledger.start >= 4096);
    assert!(sb.journal.start >= sb.root_ledger.end());
    assert!(sb.alloc_bitmap.start >= sb.journal.end());
    assert!(sb.heap.start >= sb.alloc_bitmap.end());
    assert!(sb.heap.end() <= V3_VOL_LEN);
    assert!(sb.total_extents() >= 1);

    // A volume too small for the fixed structures fails loud and typed.
    assert!(
        SuperblockV3::plan(4 * 1024 * 1024, 256 * 1024, None, TEST_UUID, TEST_SEED).is_err(),
        "a 4 MiB volume cannot hold an 8 MiB ring"
    );
}

// ---------------------------------------------------------------------------
// Resolved OQ 2: dual atomicity fields.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_reports_cow_contract_class_with_physical_probe_alongside() {
    let p = population().await;
    let be = p.backend.clone();
    // The contract class is constant-by-construction on v3 (§4.10).
    assert_eq!(be.atomicity_contract(), META_VOLUME_ATOMICITY_COW);
    assert_eq!(META_VOLUME_ATOMICITY_COW, "cow-checksummed");
    // The physical probe keeps running informationally: a temp file
    // classifies file-backed — hardware truth, reported alongside.
    assert_eq!(
        probe_meta_volume(be.device_path()),
        AtomicityClass::FileBacked
    );
}

// ---------------------------------------------------------------------------
// Builder determinism + digest walk.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builder_output_is_deterministic_and_digest_stable() {
    let build_once = || async {
        let file = NamedTempFile::new().unwrap();
        file.as_file().set_len(V3_VOL_LEN).unwrap();
        let (b, _) = describe_v3();
        let img = b.build(file.path(), V3_VOL_LEN).await.expect("build");
        (file, img)
    };
    let (f1, img1) = build_once().await;
    let (f2, img2) = build_once().await;

    assert_eq!(img1.nodes_written, img2.nodes_written);
    assert_eq!(
        img1.extents_allocated, img1.nodes_written,
        "one extent per node"
    );
    assert_eq!(img1.next_ino, img2.next_ino);
    assert_eq!(img1.ledger_seq, img2.ledger_seq);

    // Byte-identical images: the determinism contract that lets §8 gates
    // measure real volumes and K9's dry-run diff mean anything.
    let b1 = squeezefs::uring_fs::read_at(f1.path(), 0, V3_VOL_LEN as usize)
        .await
        .unwrap();
    let b2 = squeezefs::uring_fs::read_at(f2.path(), 0, V3_VOL_LEN as usize)
        .await
        .unwrap();
    assert_eq!(
        xxhash_rust::xxh3::xxh3_64(&b1),
        xxhash_rust::xxh3::xxh3_64(&b2),
        "identical descriptions must build byte-identical images"
    );

    // The §4.10 post-fold digest walk agrees across the two builds.
    let be1 = KvMetaBackend::open(f1.path()).await.expect("open build 1");
    let be2 = KvMetaBackend::open(f2.path()).await.expect("open build 2");
    assert_eq!(
        digest_backend(&be1).await.unwrap(),
        digest_backend(&be2).await.unwrap(),
        "post-fold digest walks must match"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builder_rejects_bad_descriptions_typed() {
    let mut b = ImageBuilder::new(v3_builder_config()).unwrap();
    let d = b.add_dir(ROOT_INO, "d", 0o755, 0, 0).unwrap();
    let f = b.add_file(d, "f", 0o644, 0, 0, 0).unwrap();

    // Duplicate name in one directory.
    assert!(
        b.add_file(d, "f", 0o644, 0, 0, 0).is_err(),
        "duplicate name"
    );
    // Missing / non-directory parents.
    assert!(
        b.add_file(999, "x", 0o644, 0, 0, 0).is_err(),
        "missing parent"
    );
    assert!(
        b.add_file(f, "x", 0o644, 0, 0, 0).is_err(),
        "file as parent"
    );
    // Hard links to directories are refused.
    assert!(b.add_link(d, ROOT_INO, "dirlink").is_err(), "dir hard link");
    // A name over the 255-byte record limit.
    let long = "n".repeat(256);
    assert!(matches!(
        b.add_file(d, &long, 0o644, 0, 0, 0),
        Err(KvError::NameTooLong { .. })
    ));
    // A value over the per-volume record cap (64 KiB node ⇒ 16 KiB cap).
    assert!(matches!(
        b.set_xattr(f, "user.huge", &vec![0u8; 17 * 1024]),
        Err(KvError::ValueTooLarge { .. })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_xattr_capability_lift_beyond_v2_cap() {
    // 60,000 bytes — 7.3× the v2 8 KiB ceiling — round-trips on a
    // 256 KiB-node volume (cap = 65,536; §4.2 / §5.1 capability lift).
    let file = NamedTempFile::new().unwrap();
    file.as_file().set_len(V3_VOL_LEN).unwrap();
    let mut b = ImageBuilder::new(BuilderConfig {
        node_size: 256 * 1024,
        journal_len_override: Some(V3_RING_LEN),
        hash_seed: TEST_SEED,
        uuid: TEST_UUID,
    })
    .unwrap();
    let f = b.add_file(ROOT_INO, "big-layout", 0o644, 0, 0, 0).unwrap();
    let value: Vec<u8> = (0..60_000u32).map(|i| (i % 251) as u8).collect();
    b.set_xattr(f, "user.big-layout", &value).unwrap();
    b.build(file.path(), V3_VOL_LEN).await.unwrap();

    let be = KvMetaBackend::open(file.path()).await.unwrap();
    assert_eq!(
        be.getxattr(f, "user.big-layout").await.unwrap(),
        Some(value),
        "a 60 KB xattr value must round-trip on v3"
    );
}

// ---------------------------------------------------------------------------
// v3 readdir offset/max streaming (§5.1, backend half).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_readdir_pages_partition_exactly_by_cookie() {
    let file = NamedTempFile::new().unwrap();
    file.as_file().set_len(V3_VOL_LEN).unwrap();
    let mut b = ImageBuilder::new(v3_builder_config()).unwrap();
    let dir = b.add_dir(ROOT_INO, "many", 0o755, 0, 0).unwrap();
    let names: Vec<String> = (0..50).map(|i| format!("entry-{i:03}")).collect();
    for n in &names {
        b.add_file(dir, n, 0o644, 0, 0, 0).unwrap();
    }
    b.build(file.path(), V3_VOL_LEN).await.unwrap();
    let be = KvMetaBackend::open(file.path()).await.unwrap();

    // Expected order: ascending seeded hash (coll_seq 0 — 50 short names
    // cannot collide in 54 bits with the fixed test seed).
    let mut expected = names.clone();
    expected.sort_by_key(|n| dentry_name_hash54(n.as_bytes(), TEST_SEED));

    // Offsets 0, 1, and 2 all resume from the directory start (§5.1:
    // those slots belong to the FUSE layer's synthetic ./..).
    for offset in [0u64, 1, 2] {
        let first = be.readdir(dir, offset, 1).await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(
            first[0].name, expected[0],
            "offset {offset} resumes from the start"
        );
    }

    // Page through with max=7, resuming at each page's last cookie:
    // strictly-greater resume ⇒ no duplicates, no gaps, hash order.
    let mut seen: Vec<String> = Vec::new();
    let mut cookie = 0u64;
    loop {
        let page = be.readdir(dir, cookie, 7).await.unwrap();
        if page.is_empty() {
            break;
        }
        assert!(page.len() <= 7, "max must bound the page");
        let last = page.last().unwrap().name.clone();
        seen.extend(page.into_iter().map(|d| d.name));
        cookie = encode_readdir_cookie(dentry_name_hash54(last.as_bytes(), TEST_SEED), 0);
    }
    assert_eq!(
        seen, expected,
        "pages must partition the directory exactly, in hash order"
    );

    // max == 0 is an empty page, not an error.
    assert!(be.readdir(dir, 0, 0).await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Mount = SB → ledger → bitmap → replay (read-only, into the cache).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_mount_replays_journal_window_into_the_cache() {
    let file = NamedTempFile::new().unwrap();
    file.as_file().set_len(V3_VOL_LEN).unwrap();
    let (b, inos) = describe_v3();
    b.build(file.path(), V3_VOL_LEN).await.unwrap();

    // A clean image replays nothing.
    let (next_ino_before, sb, ledger) = {
        let be = KvMetaBackend::open(file.path()).await.unwrap();
        let stats = be.replay_stats();
        assert_eq!(
            (stats.entries, stats.dropped_torn),
            (0, 0),
            "clean image, empty window"
        );
        (
            be.next_ino(),
            be.superblock().clone(),
            be.mounted_ledger().clone(),
        )
    };

    // Write two committed transactions into the ring the way K6b will:
    // admit → reserve → write the committer's own bytes.
    let ring = JournalRing::new(
        file.path(),
        sb.journal.start,
        sb.journal_pages(),
        checkpoint_reserve_bytes(sb.journal.len),
    );
    let new_ino = next_ino_before; // §4.8: the watermark names the next free ino
    let hash54 = dentry_name_hash54(b"replayed.txt", sb.hash_seed);

    // Tx 1: create "replayed.txt" (inode Put + dentry Put) — record seqs
    // strictly above the built image's checkpoint-covered records.
    let tx1: Vec<(u8, Record)> = vec![
        (
            TREE_INODES,
            Record::put(
                inode_key(new_ino).to_vec(),
                ledger.seq + 1,
                InodeValue {
                    mode: libc::S_IFREG | 0o640,
                    uid: 7,
                    gid: 8,
                    nlink: 1,
                    size: 1234,
                    ..Default::default()
                }
                .encode(),
            ),
        ),
        (
            TREE_DENTRIES,
            Record::put(
                dentry_key(ROOT_INO, hash54, 0).to_vec(),
                ledger.seq + 1,
                DentryValue {
                    child_ino: new_ino,
                    file_type: (libc::S_IFREG >> 12) as u8,
                    name: b"replayed.txt".to_vec(),
                }
                .encode()
                .unwrap(),
            ),
        ),
    ];
    // Tx 2: a Δtime merge record against the PRE-EXISTING root inode —
    // replay folds it onto the built base (§4.4 pt 6 / §4.2 fold).
    let tx2: Vec<(u8, Record)> = vec![(
        TREE_INODES,
        Record::delta(
            inode_key(ROOT_INO).to_vec(),
            ledger.seq + 2,
            &InodeDelta::times(777_000, 888_000),
        ),
    )];
    for records in [&tx1, &tx2] {
        let len = entry_len_for(records).unwrap();
        let adm = ring
            .core()
            .try_admit(len, AdmissionClass::User)
            .expect("fresh ring admits");
        let res = ring.core().reserve(adm);
        ring.write_entry(&res, records).await.unwrap();
    }

    // Remount: the window replays read-only into the cache.
    let be = KvMetaBackend::open(file.path()).await.unwrap();
    let stats = be.replay_stats();
    assert_eq!(stats.entries, 2, "both committed entries recover");
    assert_eq!(
        stats.dropped_torn, 0,
        "clean shutdown ⇒ no confirmed drops (§10)"
    );

    let got = be
        .lookup(ROOT_INO, "replayed.txt")
        .await
        .expect("replayed file resolves");
    assert_eq!(got.ino, new_ino);
    assert_eq!(got.mode, libc::S_IFREG | 0o640);
    assert_eq!((got.uid, got.gid, got.size), (7, 8, 1234));

    let root = be.getattr(ROOT_INO).await.unwrap();
    assert_eq!(
        (root.mtime, root.ctime),
        (777_000, 888_000),
        "the replayed Δtime must fold onto the built root record"
    );

    // §4.8: the watermark clears every replayed ino.
    assert!(
        be.next_ino() > new_ino,
        "next_ino must advance past replayed inos: {} vs {new_ino}",
        be.next_ino()
    );
    // Pre-existing content still serves (replay never clobbers unrelated keys).
    assert_eq!(be.lookup(ROOT_INO, "docs").await.unwrap().ino, inos["docs"]);
}

// ---------------------------------------------------------------------------
// v3 format guards (the preflight contract carried over).
// ---------------------------------------------------------------------------

fn format_opts(force: bool) -> FormatV3Options {
    FormatV3Options {
        node_size: V3_NODE_SIZE,
        journal_len_override: Some(V3_RING_LEN),
        force,
        full_wipe: false,
        format_config_xattr: Some(b"{\"probe\":true}".to_vec()),
    }
}

/// A QUICK reformat (`--force` without `--full`) must bury the previous
/// generation's records. Quick format zeroes only `[0, heap.start)` — heap
/// extents keep the dead generation's bytes — so the fresh image's nodes
/// land on extents still carrying the dead generation's appended tail-bset
/// frames. Node seqs restart identically every generation, so those frames
/// satisfy the `node_seq_at_write == node_seq` chain check and the dead
/// tree's records RESURRECT into the fresh volume (field shape 2026-07-10:
/// a reformatted 1-meta/1-data volume mounted with `checked=176,
/// valid_inodes=57` on an empty ledger, bench failing ENOENT on phantom
/// metadata; the sibling of the stale-staging poisoning).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_quick_reformat_buries_previous_generation_records() {
    let file = NamedTempFile::new().unwrap();
    file.as_file().set_len(V3_VOL_LEN).unwrap();
    format_v3(file.path(), V3_VOL_LEN, &format_opts(false))
        .await
        .expect("generation 1 format");

    // Generation 1: populate through the live write path, then checkpoint
    // everything into node tail-bsets via clean shutdown.
    let be = KvMetaBackend::open(file.path()).await.expect("gen1 open");
    for i in 0..200 {
        be.create(ROOT_INO, &format!("gen1_{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("gen1 create");
    }
    be.shutdown().await.expect("gen1 clean shutdown");
    drop(be);

    // Generation 2: QUICK reformat — heap extents keep gen1 bytes.
    format_v3(file.path(), V3_VOL_LEN, &format_opts(true))
        .await
        .expect("generation 2 quick reformat");

    let be = KvMetaBackend::open(file.path()).await.expect("gen2 open");
    let entries = be.readdir(ROOT_INO, 0, 4096).await.expect("gen2 readdir");
    let ghosts: Vec<&str> = entries
        .iter()
        .map(|e| e.name.as_str())
        .filter(|n| n.starts_with("gen1_"))
        .collect();
    assert!(
        ghosts.is_empty(),
        "quick reformat resurrected {} dead-generation dentries (node_seq \
         tail-bset ABA across generations), e.g. {:?}",
        ghosts.len(),
        ghosts.first()
    );
    assert!(
        be.lookup(ROOT_INO, "gen1_0").await.is_err(),
        "dead generation's file served by the freshly formatted volume"
    );
    // Unmount gen2 before reformatting again: the single-writer guard's
    // writer_claim now (correctly) marks this backend as a LIVE mount,
    // and the preflight refuses to format under one — the gen3 format
    // below used to rip the volume out from under the still-open `be`,
    // which only "worked" because raw backends left no registration.
    be.shutdown().await.expect("gen2 clean shutdown");
    drop(be);
    // The fresh volume must also produce a fresh generation identity
    // (superblock uuid) — the staging generation binding depends on it.
    let g1 = match classify_volume(file.path()).await.unwrap() {
        VolumeFormat::V3(sb) => sb.uuid,
        other => panic!("expected v3, got {other:?}"),
    };
    format_v3(file.path(), V3_VOL_LEN, &format_opts(true))
        .await
        .expect("generation 3 quick reformat");
    let g2 = match classify_volume(file.path()).await.unwrap() {
        VolumeFormat::V3(sb) => sb.uuid,
        other => panic!("expected v3, got {other:?}"),
    };
    assert_ne!(g1, g2, "every format invocation mints a fresh uuid");
}

/// The field workaround behind the root-daemon EIO investigation
/// (2026-07-13): an operator dd'd 1 MiB of zeros over each meta volume's
/// HEAD only (to clear a refused superblock), leaving days of prior-
/// generation residue — journal-ring payloads, node frames, allocator
/// bitmap bytes — beyond 1 MiB. `format` then classifies the volume
/// **Blank** and takes the VIRGIN path. This pin proves the virgin path
/// buries every residue class exactly like the recognized-reformat path
/// (`v3_quick_reformat_buries_previous_generation_records` above),
/// because burial is unconditional in `ImageBuilder::build`:
/// `zero_range[0, heap.start)` re-zeroes the ledger + WHOLE journal ring
/// + bitmap from the freshly planned geometry, and heap frames are
/// refused by uuid-namespaced node-seq admission — never by wiping.
///
/// Mechanistic assertions, per residue class:
/// - **journal**: prior-generation ring bytes beyond the 1 MiB dd are
///   proven present pre-format and all-zero post-format (nothing to
///   replay — a fresh mount must see `journal_tail_seq = 0` over a
///   silent ring);
/// - **nodes**: prior-generation heap frames are proven to SURVIVE the
///   quick format (burial-by-admission, not by erasure) while no ghost
///   dentry/xattr/ino is served;
/// - **allocator/ino**: the first create on the reformatted volume mints
///   ino 2 (fresh `next_ino` watermark — no ledger/allocator residue).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_dd_zeroed_head_virgin_format_buries_all_residue_classes() {
    use std::os::unix::fs::FileExt;

    const DD_LEN: u64 = 1024 * 1024; // the operator's `dd bs=1M count=1`
                                     // A ring bigger than the dd so journal residue provably outlives it
                                     // (field volumes carry 32 MiB rings; the dd covered only their head).
    const RING_LEN: u64 = 2 * 1024 * 1024;
    let opts = |force: bool| FormatV3Options {
        node_size: V3_NODE_SIZE,
        journal_len_override: Some(RING_LEN),
        force,
        full_wipe: false,
        format_config_xattr: Some(b"{\"probe\":true}".to_vec()),
    };

    let file = NamedTempFile::new().unwrap();
    file.as_file().set_len(V3_VOL_LEN).unwrap();

    // Generation 1: real live-path activity, cleanly checkpointed.
    format_v3(file.path(), V3_VOL_LEN, &opts(false))
        .await
        .expect("gen1 format");
    let be = KvMetaBackend::open(file.path()).await.expect("gen1 open");
    for i in 0..200 {
        be.create(ROOT_INO, &format!("gen1_{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("gen1 create");
    }
    be.shutdown().await.expect("gen1 shutdown");
    drop(be);

    // Generation 2 (the user's history: multiple prior formats): a
    // RECOGNIZED-superblock quick reformat, then enough journal payload
    // that ring bytes provably extend past the dd horizon.
    format_v3(file.path(), V3_VOL_LEN, &opts(true))
        .await
        .expect("gen2 recognized reformat");
    let be = KvMetaBackend::open(file.path()).await.expect("gen2 open");
    let big = vec![0xa5u8; 8 * 1024];
    for i in 0..160 {
        let ino = be
            .create(ROOT_INO, &format!("gen2_{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("gen2 create")
            .ino;
        be.setxattr(ino, "user.residue", &big)
            .await
            .expect("gen2 xattr");
    }
    be.shutdown().await.expect("gen2 shutdown");
    drop(be);

    // Capture gen2 geometry + identity BEFORE the dd erases sector 0.
    let gen2_sb = match classify_volume(file.path()).await.unwrap() {
        VolumeFormat::V3(sb) => sb,
        other => panic!("expected v3 before the dd, got {other:?}"),
    };
    let ring = gen2_sb.journal;
    assert!(
        ring.end() > DD_LEN,
        "test geometry must put ring bytes beyond the dd horizon \
         (ring ends at {}, dd covers {DD_LEN})",
        ring.end()
    );
    let image = std::fs::read(file.path()).unwrap();
    let ring_past_dd = &image[DD_LEN as usize..ring.end() as usize];
    assert!(
        ring_past_dd.iter().any(|b| *b != 0),
        "aging must leave journal-ring residue beyond the dd horizon, \
         or this pin is vacuous — grow the gen2 payload"
    );

    // The operator's workaround: zero ONLY the first MiB.
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(file.path())
        .unwrap();
    f.write_all_at(&vec![0u8; DD_LEN as usize], 0).unwrap();
    f.sync_all().unwrap();
    drop(f);

    // Premise pin: the volume now classifies Blank — format takes the
    // VIRGIN path (no recognized superblock to reformat against), and
    // the virgin gate needs no --force.
    assert!(
        matches!(
            classify_volume(file.path()).await.unwrap(),
            VolumeFormat::Blank
        ),
        "a dd-zeroed head must classify Blank — the workaround's premise"
    );
    format_v3(file.path(), V3_VOL_LEN, &opts(false))
        .await
        .expect("the virgin path must format a dd-zeroed volume without --force");

    let gen3_sb = match classify_volume(file.path()).await.unwrap() {
        VolumeFormat::V3(sb) => sb,
        other => panic!("expected v3 after the virgin format, got {other:?}"),
    };
    assert_ne!(
        gen3_sb.uuid, gen2_sb.uuid,
        "the virgin format must mint a fresh generation identity"
    );
    assert_eq!(
        gen3_sb.journal, ring,
        "identical knobs must re-plan identical geometry (the burial \
         range covers the whole prior ring)"
    );

    // Journal burial: the WHOLE fresh ring is zero — nothing to replay.
    let image = std::fs::read(file.path()).unwrap();
    let ring_bytes = &image[ring.start as usize..ring.end() as usize];
    assert!(
        ring_bytes.iter().all(|b| *b == 0),
        "the virgin format must zero the whole journal ring exactly like \
         the recognized-reformat path (prior-generation entries would \
         otherwise replay into the fresh volume)"
    );

    // Node-frame residue SURVIVES the quick format beyond the freshly
    // written empty tree roots — burial here is by node-seq admission,
    // not erasure. (If a future format full-wipes by default this turns
    // vacuously true; the ghost assertions below still hold.)
    let heap = gen3_sb.heap;
    let node = gen3_sb.node_size as u64;
    let residue_scan_start = (heap.start + 8 * node) as usize;
    let residue_scan_end = (heap.start + 64 * node).min(heap.end()) as usize;
    let heap_residue = image[residue_scan_start..residue_scan_end]
        .iter()
        .any(|b| *b != 0);
    assert!(
        heap_residue,
        "expected prior-generation node frames to survive a quick format \
         (the design leaves the heap; admission buries it) — if this \
         fails the aging phases stopped writing enough nodes"
    );

    // Ghost checks across every tree.
    let be = KvMetaBackend::open(file.path())
        .await
        .expect("gen3 open (fresh mount over buried residue)");
    let entries = be.readdir(ROOT_INO, 0, 4096).await.expect("gen3 readdir");
    let ghosts: Vec<&str> = entries
        .iter()
        .map(|e| e.name.as_str())
        .filter(|n| n.starts_with("gen1_") || n.starts_with("gen2_"))
        .collect();
    assert!(
        ghosts.is_empty(),
        "virgin format resurrected {} prior-generation dentries, e.g. {:?}",
        ghosts.len(),
        ghosts.first()
    );
    assert!(
        be.lookup(ROOT_INO, "gen1_0").await.is_err(),
        "gen1 file served by the dd-then-virgin-formatted volume"
    );
    assert!(
        be.lookup(ROOT_INO, "gen2_0").await.is_err(),
        "gen2 file served by the dd-then-virgin-formatted volume"
    );

    // Fresh ino watermark: the first create mints ino 2 (ino 0 reserved,
    // 1 = root) — a leaked ledger/next_ino would mint above the dead
    // population instead.
    let probe = be
        .create(ROOT_INO, "probe", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("gen3 probe create");
    assert_eq!(
        probe.ino,
        ROOT_INO + 1,
        "the reformatted volume must mint inos from the fresh watermark"
    );
    // And the dead generation's xattrs are unreachable on the fresh ino
    // space: the probe ino carries no residue xattr.
    assert!(
        be.getxattr(probe.ino, "user.residue")
            .await
            .expect("gen3 getxattr")
            .is_none(),
        "prior-generation xattr served on a freshly minted ino"
    );
    be.shutdown().await.expect("gen3 shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_format_guards_match_the_preflight_contract() {
    let file = NamedTempFile::new().unwrap();
    file.as_file().set_len(V3_VOL_LEN).unwrap();

    // Blank volume formats without force.
    format_v3(file.path(), V3_VOL_LEN, &format_opts(false))
        .await
        .expect("a blank volume formats without --force");

    // The freshly formatted (v3) volume is refused without force…
    let err = format_v3(file.path(), V3_VOL_LEN, &format_opts(false))
        .await
        .expect_err("an already-formatted v3 volume must be refused without --force")
        .to_string();
    assert!(
        err.contains("--force"),
        "the refusal must mention --force, got: {err}"
    );
    // …and reformats with it.
    format_v3(file.path(), V3_VOL_LEN, &format_opts(true))
        .await
        .expect("--force reformats an idle v3 volume");

    // The recorded format config is readable through the mount bootstrap
    // path (ino 1 of the first volume's xattr tree).
    let vol = open_volume_for_mount(file.path().to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        vol.getxattr(ROOT_INO, "user.squeezefs.format_config")
            .await
            .unwrap(),
        Some(b"{\"probe\":true}".to_vec()),
        "format must record the config xattr in the v3 xattr tree"
    );

    // A legacy v2 volume (crafted superblock bytes) is protected by the
    // same guard: refused without force, REFORMATTED to v3 with it — the
    // only path forward for v2 volumes now that v2 support is gone.
    let v2 = NamedTempFile::new().unwrap();
    v2.as_file().set_len(V3_VOL_LEN).unwrap();
    let mut legacy_sb = Vec::with_capacity(12);
    legacy_sb.extend_from_slice(b"METALV01");
    legacy_sb.extend_from_slice(&2u32.to_le_bytes());
    squeezefs::uring_fs::write_at(v2.path(), 0, bytes::Bytes::from(legacy_sb))
        .await
        .unwrap();
    assert!(
        format_v3(v2.path(), V3_VOL_LEN, &format_opts(false))
            .await
            .is_err(),
        "a formatted (legacy v2) volume must be refused without --force"
    );
    format_v3(v2.path(), V3_VOL_LEN, &format_opts(true))
        .await
        .expect("--force reformats a legacy v2 volume to v3");
    assert!(
        open_volume_for_mount(v2.path().to_str().unwrap())
            .await
            .is_ok(),
        "the reformatted volume mounts as v3"
    );
}

// ---------------------------------------------------------------------------
// The mounted backend surfaces (mount-log inputs, §10).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_mount_surfaces_ledger_and_allocator_state() {
    let file = NamedTempFile::new().unwrap();
    file.as_file().set_len(V3_VOL_LEN).unwrap();
    let (b, inos) = describe_v3();
    let img = b.build(file.path(), V3_VOL_LEN).await.unwrap();

    let be = KvMetaBackend::open(file.path()).await.unwrap();
    assert_eq!(be.mounted_ledger().seq, img.ledger_seq);
    assert_eq!(be.next_ino(), img.next_ino);
    assert_eq!(
        SUPERBLOCK_V3_VERSION, 3,
        "the version constant is the on-disk gate value"
    );
    let total = be.superblock().total_extents();
    assert_eq!(
        be.free_extents(),
        total - img.extents_allocated,
        "free extents = total − built nodes"
    );
    assert_eq!(be.trees().len(), 3);

    // Builder-set timestamps round-trip; unset ones stay at the
    // deterministic 0 default.
    let readme = be.getattr(inos["readme.txt"]).await.unwrap();
    assert_eq!(
        (readme.atime, readme.mtime, readme.ctime),
        README_TIMES,
        "set_times must round-trip through the built image"
    );
    let hello = be.getattr(inos["hello.bin"]).await.unwrap();
    assert_eq!(
        (hello.atime, hello.mtime, hello.ctime),
        (0, 0, 0),
        "unset builder times default to 0 (the determinism contract)"
    );
}

// ===========================================================================
// PR K6b — the mutating `Metadata` conformance suite, the §4.4 pt 6
// Δtime shared-parent contract, v3 persistence across remounts, the §4.8
// monotonic-ino contract, the unknown-ro write gate, and the R10
// ring-full liveness storm (§4.4 pt 5). (These cases originally ran
// against both formats; the v2 leg was deleted with v2 support — the
// assertions are unchanged.)
// ===========================================================================

use squeezefs::error::SqueezefsError;
use squeezefs::meta_backend::kv::{META_KV_NODE_COMPACTIONS, META_KV_NODE_SPLITS};
use squeezefs::meta_backend::RoutedMetaBackend;
use std::sync::atomic::Ordering as AtomicOrdering;

/// A fresh, EMPTY, mutable volume (the mutating suite builds all content
/// through the trait surface).
async fn mutable_volume() -> (Arc<KvMetaBackend>, NamedTempFile) {
    let file = NamedTempFile::new().expect("temp volume");
    file.as_file().set_len(V3_VOL_LEN).unwrap();
    format_v3(
        file.path(),
        V3_VOL_LEN,
        &FormatV3Options {
            node_size: V3_NODE_SIZE,
            journal_len_override: Some(V3_RING_LEN),
            force: false,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .unwrap();
    let backend = open_volume_for_mount(file.path().to_str().unwrap())
        .await
        .expect("open_volume_for_mount");
    (backend, file)
}

/// The reference clock for "stamped at-or-after this" assertions — the
/// SAME domain the daemon stamps inode times from (`CLOCK_REALTIME_COARSE`
/// since the generic/423 fix; a fine-grained reference taken just before
/// an op legitimately LEADS the op's coarse stamp within a tick).
fn now_ns() -> u64 {
    squeezefs::coarse_realtime_ns()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutating_create_lookup_conformance() {
    let (b, _f) = mutable_volume().await;

    let t0 = now_ns();
    let dir = b
        .create(ROOT_INO, "dir", libc::S_IFDIR | 0o750, 1000, 1000)
        .await
        .expect("mkdir");
    assert_eq!(dir.mode, libc::S_IFDIR | 0o750);
    assert_eq!((dir.uid, dir.gid), (1000, 1000));
    assert!(dir.ino > ROOT_INO);

    let f = b
        .create(dir.ino, "file.txt", libc::S_IFREG | 0o644, 7, 8)
        .await
        .expect("create file");
    assert_eq!(f.mode, libc::S_IFREG | 0o644);
    assert_eq!((f.uid, f.gid), (7, 8));
    assert_eq!(f.nlink, 1, "a fresh regular file has nlink 1");
    assert_eq!(f.size, 0);

    // lookup and getattr agree with the create return.
    let by_lookup = b.lookup(dir.ino, "file.txt").await.expect("lookup");
    assert_eq!(by_lookup.ino, f.ino);
    let by_getattr = b.getattr(f.ino).await.expect("getattr");
    assert_eq!(
        (
            by_getattr.mode,
            by_getattr.uid,
            by_getattr.gid,
            by_getattr.nlink
        ),
        (f.mode, f.uid, f.gid, f.nlink)
    );

    // The parent's times were bumped by the create.
    let parent_after = b.getattr(dir.ino).await.unwrap();
    assert!(
        parent_after.mtime >= t0 && parent_after.ctime >= t0,
        "create must update parent mtime/ctime (mtime {} ctime {} vs t0 {t0})",
        parent_after.mtime,
        parent_after.ctime
    );

    // Duplicate name: refused, naming the conflict.
    let err = b
        .create(dir.ino, "file.txt", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect_err("duplicate create must fail")
        .to_string();
    assert!(
        err.contains("already exists"),
        "EEXIST shape must name the conflict, got: {err}"
    );

    // Create under a missing parent: loud.
    assert!(
        b.create(999_999, "x", libc::S_IFREG | 0o644, 0, 0)
            .await
            .is_err(),
        "create under a missing parent must error"
    );

    // setgid inheritance: a setgid parent stamps its gid on children and
    // propagates setgid to subdirectories (the v2 contract).
    let sg = b
        .create(
            ROOT_INO,
            "sgid",
            libc::S_IFDIR | libc::S_ISGID | 0o770,
            0,
            4242,
        )
        .await
        .unwrap();
    let child_f = b
        .create(sg.ino, "f", libc::S_IFREG | 0o600, 1, 1)
        .await
        .unwrap();
    assert_eq!(child_f.gid, 4242, "setgid dir stamps its gid on files");
    let child_d = b
        .create(sg.ino, "d", libc::S_IFDIR | 0o700, 1, 1)
        .await
        .unwrap();
    assert_eq!(child_d.gid, 4242);
    assert_ne!(
        child_d.mode & libc::S_ISGID,
        0,
        "setgid propagates to subdirectories"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutating_unlink_and_link_conformance() {
    let (b, _f) = mutable_volume().await;

    let f = b
        .create(ROOT_INO, "a.txt", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();

    // Hard link: nlink 2, both names resolve to one ino.
    let linked = b.link(f.ino, ROOT_INO, "b.txt").await.expect("link");
    assert_eq!(linked.ino, f.ino);
    assert_eq!(linked.nlink, 2);
    assert_eq!(b.lookup(ROOT_INO, "b.txt").await.unwrap().ino, f.ino);

    // Link to an existing name: refused.
    assert!(
        b.link(f.ino, ROOT_INO, "a.txt").await.is_err(),
        "link over an existing name must fail"
    );

    // Unlink one name: the other survives with nlink 1.
    let gone = b.unlink(ROOT_INO, "a.txt").await.expect("unlink");
    assert_eq!(gone, f.ino, "unlink returns the child ino");
    assert!(
        b.lookup(ROOT_INO, "a.txt").await.is_err(),
        "unlinked name must stop resolving"
    );
    let survivor = b.lookup(ROOT_INO, "b.txt").await.unwrap();
    assert_eq!(survivor.ino, f.ino);
    assert_eq!(survivor.nlink, 1, "nlink decremented by the unlink");

    // Unlink of a missing name: loud.
    let err = b
        .unlink(ROOT_INO, "never-existed")
        .await
        .expect_err("unlink of a missing name must fail")
        .to_string();
    assert!(
        err.to_lowercase().contains("not found"),
        "unlink ENOENT shape, got: {err}"
    );

    // Directory unlink zeroes the child's nlink (the rmdir shape).
    let d = b
        .create(ROOT_INO, "subdir", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    b.unlink(ROOT_INO, "subdir").await.expect("rmdir shape");
    assert!(
        b.lookup(ROOT_INO, "subdir").await.is_err(),
        "removed dir must stop resolving"
    );
    let _ = d;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutating_rename_conformance() {
    let (b, _f) = mutable_volume().await;

    let d1 = b
        .create(ROOT_INO, "d1", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    let d2 = b
        .create(ROOT_INO, "d2", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    let f = b
        .create(d1.ino, "orig", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();

    // Plain move across directories.
    b.rename(d1.ino, "orig", d2.ino, "moved", 0)
        .await
        .expect("plain rename");
    assert!(b.lookup(d1.ino, "orig").await.is_err(), "old name gone");
    assert_eq!(
        b.lookup(d2.ino, "moved").await.unwrap().ino,
        f.ino,
        "new name resolves to the same ino"
    );

    // NOREPLACE against an existing destination: EEXIST, raw os error.
    let blocker = b
        .create(d2.ino, "blocker", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let err = b
        .rename(d2.ino, "moved", d2.ino, "blocker", libc::RENAME_NOREPLACE)
        .await
        .expect_err("NOREPLACE over an existing name must fail");
    match &err {
        SqueezefsError::Io(io) => assert_eq!(
            io.raw_os_error(),
            Some(libc::EEXIST),
            "NOREPLACE must surface EEXIST, got {io:?}"
        ),
        other => panic!("NOREPLACE must be an Io(EEXIST) error, got {other:?}"),
    }

    // Replacing rename (no flags): the destination dentry is replaced,
    // the source moves in. (At this single-volume trait level rename is
    // pure dentry surgery — the v2 contract; destination-inode nlink
    // accounting is the routed layer's job.)
    b.rename(d2.ino, "moved", d2.ino, "blocker", 0)
        .await
        .expect("replacing rename");
    assert_eq!(b.lookup(d2.ino, "blocker").await.unwrap().ino, f.ino);
    assert!(b.lookup(d2.ino, "moved").await.is_err());
    let _ = blocker;

    // EXCHANGE swaps the two names' inos.
    let g = b
        .create(d1.ino, "swap-me", libc::S_IFREG | 0o600, 0, 0)
        .await
        .unwrap();
    b.rename(d1.ino, "swap-me", d2.ino, "blocker", libc::RENAME_EXCHANGE)
        .await
        .expect("exchange rename");
    assert_eq!(b.lookup(d1.ino, "swap-me").await.unwrap().ino, f.ino);
    assert_eq!(b.lookup(d2.ino, "blocker").await.unwrap().ino, g.ino);

    // EXCHANGE with a missing side: ENOENT.
    let err = b
        .rename(d1.ino, "no-src", d2.ino, "blocker", libc::RENAME_EXCHANGE)
        .await
        .expect_err("exchange with missing source must fail");
    match &err {
        SqueezefsError::Io(io) => assert_eq!(io.raw_os_error(), Some(libc::ENOENT)),
        other => panic!("exchange-ENOENT shape, got {other:?}"),
    }

    // Both flags together: EINVAL.
    let err = b
        .rename(
            d1.ino,
            "x",
            d2.ino,
            "y",
            libc::RENAME_NOREPLACE | libc::RENAME_EXCHANGE,
        )
        .await
        .expect_err("NOREPLACE+EXCHANGE must fail");
    match &err {
        SqueezefsError::Io(io) => assert_eq!(io.raw_os_error(), Some(libc::EINVAL)),
        other => panic!("EINVAL shape, got {other:?}"),
    }

    // Missing source, no flags: NotFound.
    assert!(
        b.rename(d1.ino, "ghost", d2.ino, "z", 0).await.is_err(),
        "rename of a missing source must fail"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutating_setattr_conformance() {
    let (b, _f) = mutable_volume().await;
    let f = b
        .create(ROOT_INO, "attrs", libc::S_IFREG | 0o644, 10, 20)
        .await
        .unwrap();

    let before = b.getattr(f.ino).await.unwrap();
    let t0 = now_ns();

    // chmod: ctime auto-bumps.
    let after = b
        .setattr(
            f.ino,
            Some(libc::S_IFREG | 0o600),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("chmod");
    assert_eq!(after.mode, libc::S_IFREG | 0o600);
    assert!(
        after.ctime >= t0,
        "ctime must auto-bump on chmod ({} vs {t0})",
        after.ctime
    );
    assert!(after.ctime >= before.ctime);

    // chown + size + explicit times honored verbatim.
    let after = b
        .setattr(
            f.ino,
            None,
            Some(0),
            Some(0),
            Some(4096),
            Some(111),
            Some(222),
            Some(333),
        )
        .await
        .expect("chown+truncate+times");
    assert_eq!((after.uid, after.gid, after.size), (0, 0, 4096));
    assert_eq!((after.atime, after.mtime, after.ctime), (111, 222, 333));

    // Persisted: getattr agrees.
    let got = b.getattr(f.ino).await.unwrap();
    assert_eq!((got.uid, got.gid, got.size), (0, 0, 4096));
    assert_eq!((got.atime, got.mtime, got.ctime), (111, 222, 333));

    // setattr on a missing ino: loud.
    assert!(
        b.setattr(999_999, Some(0o600), None, None, None, None, None, None)
            .await
            .is_err(),
        "setattr of a missing ino must error"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutating_xattr_conformance() {
    let (b, _f) = mutable_volume().await;
    let f = b
        .create(ROOT_INO, "x", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();

    // Set, read back, overwrite, list, remove.
    b.setxattr(f.ino, "user.one", b"v1").await.expect("set");
    assert_eq!(
        b.getxattr(f.ino, "user.one").await.unwrap().as_deref(),
        Some(b"v1".as_slice())
    );
    b.setxattr(f.ino, "user.one", b"v2-overwrite")
        .await
        .expect("overwrite");
    assert_eq!(
        b.getxattr(f.ino, "user.one").await.unwrap().as_deref(),
        Some(b"v2-overwrite".as_slice())
    );
    b.setxattr(f.ino, "user.two", b"22").await.unwrap();
    let mut names = b.listxattr(f.ino).await.unwrap();
    names.sort();
    assert_eq!(names, vec!["user.one".to_string(), "user.two".to_string()]);

    b.removexattr(f.ino, "user.one").await.expect("remove");
    assert_eq!(
        b.getxattr(f.ino, "user.one").await.unwrap(),
        None,
        "removed xattr reads as absent"
    );
    assert_eq!(b.listxattr(f.ino).await.unwrap(), vec!["user.two"]);

    // removexattr of an absent name: loud.
    assert!(
        b.removexattr(f.ino, "user.ghost").await.is_err(),
        "removexattr of an absent name must error"
    );

    // The capability boundary: a 12 KiB value sits inside the
    // `node_size/4` cap (§4.2 capability lift over the retired v2
    // format's fixed 8 KiB block).
    let big = vec![0x5A; 12 * 1024];
    b.setxattr(f.ino, "user.big", &big)
        .await
        .expect("a 12 KiB value fits (cap = node_size/4)");
    assert_eq!(b.getxattr(f.ino, "user.big").await.unwrap(), Some(big));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutating_destroy_and_layout_conformance() {
    let (b, _f) = mutable_volume().await;
    let f = b
        .create(ROOT_INO, "doomed", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();

    // The fsync/release writeback shape (§5.3): layout xattr + size in
    // one call; on v3 this is ONE two-record transaction.
    b.set_layout_and_size(f.ino, b"layout-bytes-0123", 8192)
        .await
        .expect("set_layout_and_size");
    assert_eq!(
        b.getxattr(f.ino, "layout").await.unwrap().as_deref(),
        Some(b"layout-bytes-0123".as_slice())
    );
    assert_eq!(b.getattr(f.ino).await.unwrap().size, 8192);

    // unlink + destroy: the reclaim path. The inode and its xattrs are
    // gone; the batch surface tolerates missing inos (v2 skip contract).
    b.unlink(ROOT_INO, "doomed").await.unwrap();
    b.destroy_inode(f.ino).await.expect("destroy");
    assert!(
        b.getattr(f.ino).await.is_err(),
        "destroyed ino must stop resolving"
    );
    assert_eq!(
        b.getxattr(f.ino, "layout").await.unwrap(),
        None,
        "destroy must reap the layout xattr"
    );
    b.destroy_inodes(&[f.ino, 987_654])
        .await
        .expect("destroy of missing inos is a no-op (v2 skip contract)");
}

/// §4.8: v3 inos are monotonic and never reused — destroy-then-create
/// yields a strictly larger ino.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_monotonic_ino_no_reuse() {
    let (b, _f) = mutable_volume().await;
    let a = b
        .create(ROOT_INO, "first", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    b.unlink(ROOT_INO, "first").await.unwrap();
    b.destroy_inode(a.ino).await.unwrap();
    let c = b
        .create(ROOT_INO, "second", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    assert!(
        c.ino > a.ino,
        "v3 must never reuse inos: {} then {}",
        a.ino,
        c.ino
    );
}

/// §4.4 pt 6: concurrent same-directory creates under the SHARED parent
/// lock (the routed layer's production shape) — all succeed, parent times
/// advance, every child resolves (Δtime merge records).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn routed_shared_parent_create_storm() {
    let (backend, _f) = mutable_volume().await;
    let routed = Arc::new(RoutedMetaBackend::new(vec![backend]));

    let dir = routed
        .create(ROOT_INO, "storm", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    let t0 = now_ns();

    let mut handles = Vec::new();
    for t in 0..4u32 {
        let r = routed.clone();
        let parent = dir.ino;
        handles.push(tokio::spawn(async move {
            for i in 0..25u32 {
                r.create(parent, &format!("f-{t}-{i}"), libc::S_IFREG | 0o644, 0, 0)
                    .await
                    .expect("storm create");
            }
        }));
    }
    for h in handles {
        h.await.expect("storm task");
    }

    for t in 0..4u32 {
        for i in 0..25u32 {
            assert!(
                routed.lookup(dir.ino, &format!("f-{t}-{i}")).await.is_ok(),
                "storm child f-{t}-{i} must resolve"
            );
        }
    }
    let after = routed.getattr(dir.ino).await.unwrap();
    assert!(
        after.mtime >= t0 && after.ctime >= t0,
        "parent times must reflect the storm (Δtime merge records, §4.4 pt 6)"
    );
    assert_eq!(
        routed.readdir(dir.ino, 0, usize::MAX).await.unwrap().len(),
        100,
        "readdir must list every storm child"
    );
}

/// v3 persistence — clean shutdown: mutations survive a full checkpoint +
/// task drain + remount with an EMPTY replay window (tail == head).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_mutations_survive_clean_shutdown_remount() {
    let (backend, file) = mutable_volume().await;
    let be = &backend;

    let d = backend
        .create(ROOT_INO, "keep", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    for i in 0..30 {
        let f = backend
            .create(d.ino, &format!("f{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        backend
            .setxattr(f.ino, "user.tag", format!("t{i}").as_bytes())
            .await
            .unwrap();
    }
    let d1 = digest_backend(be).await.unwrap();

    be.shutdown().await.expect("clean shutdown");
    be.shutdown().await.expect("shutdown is idempotent");
    drop(backend);

    let re = KvMetaBackend::open(file.path()).await.expect("remount");
    assert_eq!(
        re.replay_stats().entries,
        0,
        "a clean shutdown checkpoints everything: the replay window is empty"
    );
    assert_eq!(
        digest_backend(&re).await.unwrap(),
        d1,
        "post-fold digest must survive the shutdown/remount"
    );
    assert_eq!(re.lookup(ROOT_INO, "keep").await.unwrap().ino, d.ino);
    let f0 = re.lookup(d.ino, "f0").await.unwrap();
    assert_eq!(
        re.getxattr(f0.ino, "user.tag").await.unwrap().as_deref(),
        Some(b"t0".as_slice())
    );
}

/// v3 persistence — NO shutdown: acked (deferred-mode) mutations are in
/// the journal's page cache; a remount replays them (D0, §4.10). The
/// flush cadence is parked at 60 s so the window cannot be checkpointed
/// away before the drop (the assertion needs a non-empty window).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_mutations_survive_remount_via_replay() {
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let body = async {
        let (backend, file) = mutable_volume().await;
        let f = backend
            .create(ROOT_INO, "replayed", libc::S_IFREG | 0o640, 3, 4)
            .await
            .unwrap();
        backend
            .setattr(f.ino, None, None, None, Some(777), None, None, None)
            .await
            .unwrap();
        drop(backend); // no shutdown, no checkpoint requirement
        (file, f.ino)
    }
    .await;
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    let (file, ino) = body;

    let re = KvMetaBackend::open(file.path()).await.expect("remount");
    assert!(
        re.replay_stats().entries > 0,
        "the un-checkpointed window must replay"
    );
    let got = re.lookup(ROOT_INO, "replayed").await.unwrap();
    assert_eq!((got.ino, got.uid, got.gid, got.size), (ino, 3, 4, 777));
    assert!(
        re.next_ino() > ino,
        "§4.8: next_ino must clear replayed inos"
    );
}

/// K6a hand-off: unknown `features_ro` bits mount read-only — K6b's write
/// path must withhold mutations (§4.11) while reads keep serving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_unknown_ro_bit_withholds_mutations() {
    let (backend, file) = mutable_volume().await;
    backend
        .create(ROOT_INO, "pre-ro", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let be = &backend;
    be.shutdown().await.unwrap();
    let sb = be.superblock().clone();
    drop(backend);

    let mut ro = sb;
    ro.features_ro |= 1 << 5;
    write_superblock_v3(file.path(), &ro).await.unwrap();

    let re = KvMetaBackend::open(file.path()).await.expect("ro mount");
    assert_eq!(re.superblock().unknown_ro(), 1 << 5);
    // Reads serve.
    assert!(re.lookup(ROOT_INO, "pre-ro").await.is_ok());
    // Every mutation is withheld.
    assert!(
        Metadata::create(
            re.as_ref(),
            ROOT_INO,
            "post-ro",
            libc::S_IFREG | 0o644,
            0,
            0
        )
        .await
        .is_err(),
        "unknown ro bits must withhold create"
    );
    assert!(
        Metadata::setxattr(re.as_ref(), ROOT_INO, "user.x", b"v")
            .await
            .is_err(),
        "unknown ro bits must withhold setxattr"
    );
}

/// v3 strict mode (`SQUEEZEFS_META_FLUSH_INTERVAL_MS=0`): every commit
/// barriers through the SyncCoalescer before acking (§4.6 pt 4).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_strict_mode_commits_barrier_per_commit() {
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "0");
    let result = async {
        let (backend, file) = mutable_volume().await;
        let f = backend
            .create(ROOT_INO, "strict", libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("strict-mode create");
        drop(backend);
        // Strict mode already barriered: a power-cut style reopen (page
        // cache is shared for a file, so this is a replay check, not a
        // physical-durability one — the crash harness owns that half).
        let re = KvMetaBackend::open(file.path()).await.unwrap();
        assert_eq!(re.lookup(ROOT_INO, "strict").await.unwrap().ino, f.ino);
    };
    let out = tokio::time::timeout(std::time::Duration::from_secs(60), result).await;
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    out.expect("strict-mode ops must not hang");
}

/// **R10 — the ring-full liveness storm (§4.4 pt 5).** A commit storm
/// against a TINY `--meta-journal-mb`-class ring (512 KiB — barely above
/// the reserve + max-entry floor) with concurrent SMO pressure (64 KiB
/// nodes split under load). Must drain and complete — never deadlock:
/// admission parks hold no node locks, SMOs draw from the checkpoint-task
/// reserve, and the minimal checkpoint consumes zero ring bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn v3_ring_full_liveness_storm_drains() {
    let file = NamedTempFile::new().unwrap();
    file.as_file().set_len(V3_VOL_LEN).unwrap();
    format_v3(
        file.path(),
        V3_VOL_LEN,
        &FormatV3Options {
            node_size: 64 * 1024,
            journal_len_override: Some(512 * 1024), // floor is 384 KiB
            force: false,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .unwrap();
    let backend = open_volume_for_mount(file.path().to_str().unwrap())
        .await
        .unwrap();
    let stalls_probe = backend.clone();
    let routed = Arc::new(RoutedMetaBackend::new(vec![backend]));

    let smo_before = META_KV_NODE_SPLITS.load(AtomicOrdering::Relaxed)
        + META_KV_NODE_COMPACTIONS.load(AtomicOrdering::Relaxed);

    // 8 directories, 16 writer tasks, 2 KiB xattr payloads: ~4,800
    // creates + 4,800 setxattrs ≈ 12 MB of journal entries against a
    // ~250 KiB user budget — the ring must wrap under sustained
    // admission pressure. (PR K7's §4.6 pt 1 threshold wakes made
    // draining aggressive enough that the original 200 B/150-iteration
    // shape never parked: reserve-exhaustion checkpoints inside the
    // maintenance passes advanced `reusable_upto` ahead of admission —
    // strictly better liveness, so the storm grows to keep the PARK
    // path exercised, which is the R10 point.)
    let mut dirs = Vec::new();
    for i in 0..8 {
        dirs.push(
            routed
                .create(ROOT_INO, &format!("dir{i}"), libc::S_IFDIR | 0o755, 0, 0)
                .await
                .unwrap()
                .ino,
        );
    }
    let storm = async {
        let mut handles = Vec::new();
        for t in 0..16u32 {
            let r = routed.clone();
            let parent = dirs[(t % 8) as usize];
            handles.push(tokio::spawn(async move {
                for i in 0..300u32 {
                    let f = r
                        .create(parent, &format!("s{t}-{i}"), libc::S_IFREG | 0o644, 0, 0)
                        .await
                        .expect("storm create must eventually admit");
                    r.setxattr(f.ino, "user.payload", &[0xEE; 2048])
                        .await
                        .expect("storm setxattr");
                }
            }));
        }
        for h in handles {
            h.await.expect("storm task must finish (no deadlock)");
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(180), storm)
        .await
        .expect("R10: the storm must drain — a hang here is the ring-full deadlock class");

    // The ring actually filled (admission parks happened)…
    assert!(
        stalls_probe.journal_full_stalls() > 0,
        "the storm never filled the 512 KiB ring — grow the storm or shrink the ring"
    );
    // …and SMOs ran concurrently on the checkpoint task.
    let smo_after = META_KV_NODE_SPLITS.load(AtomicOrdering::Relaxed)
        + META_KV_NODE_COMPACTIONS.load(AtomicOrdering::Relaxed);
    assert!(
        smo_after > smo_before,
        "64 KiB nodes under a 2,400-file storm must split/compact (SMO pressure)"
    );

    // Everything the storm acked is present and consistent.
    for (d, dir) in dirs.iter().enumerate() {
        let n = routed.readdir(*dir, 0, usize::MAX).await.unwrap().len();
        assert_eq!(n, 600, "dir{d} must list every storm child");
    }

    // And the volume survives a clean remount with a matching digest.
    let d_live = digest_backend(&stalls_probe).await.unwrap();
    stalls_probe.shutdown().await.unwrap();
    drop(routed);
    let re = KvMetaBackend::open(file.path()).await.unwrap();
    assert_eq!(
        digest_backend(&re).await.unwrap(),
        d_live,
        "storm state must survive remount byte-for-byte (post-fold digest)"
    );
}

/// fstests generic/020 repro-port (VL10 release gate): a full
/// `XATTR_SIZE_MAX` (65,536-byte) VALUE must round-trip on a
/// default-node-size volume — the Linux cap governs the VALUE; the
/// record envelope (name + length framing) must NOT eat into it. The
/// pre-fix check charged `value + name + 8` against the 65,536 record
/// cap, so the advertised "value ≤ min(65536, node_size/4)" contract
/// (AGENTS + design-cow-kv-metadata §4.2) was never actually reachable.
/// Over-cap values still refuse loud.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xattr_value_cap_is_the_full_xattr_size_max() {
    // Default 256 KiB nodes: value cap = min(65536, 65536) = 65536.
    let file = NamedTempFile::new().expect("temp volume");
    file.as_file().set_len(V3_VOL_LEN).unwrap();
    format_v3(
        file.path(),
        V3_VOL_LEN,
        &FormatV3Options {
            node_size: 256 * 1024,
            journal_len_override: Some(8 * 1024 * 1024),
            force: false,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .unwrap();
    let b = open_volume_for_mount(file.path().to_str().unwrap())
        .await
        .expect("open_volume_for_mount");

    let f = b
        .create(ROOT_INO, "xmax", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();

    // The generic/020 shape: name "user.long_attr", value 65,536 bytes.
    let val = vec![0x00u8; 65_536];
    b.setxattr(f.ino, "user.long_attr", &val)
        .await
        .expect("a full XATTR_SIZE_MAX value must fit (generic/020)");
    assert_eq!(
        b.getxattr(f.ino, "user.long_attr").await.unwrap(),
        Some(val),
        "the 64 KiB value must round-trip byte-exact"
    );

    // One byte over the value cap refuses loud.
    let over = vec![0x00u8; 65_537];
    assert!(
        b.setxattr(f.ino, "user.long_attr", &over).await.is_err(),
        "65,537-byte value must refuse (cap is the VALUE cap)"
    );

    // Small-node volumes keep the node_size/4 VALUE cap (the documented
    // sub-256-KiB drop) — 16 KiB value cap at 64 KiB nodes.
    let (small, _sf) = mutable_volume().await;
    let g = small
        .create(ROOT_INO, "xsmall", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let exact = vec![0x11u8; 16 * 1024];
    small
        .setxattr(g.ino, "user.exactly_a_quarter_node", &exact)
        .await
        .expect("a full node_size/4 VALUE must fit regardless of name length");
    assert_eq!(
        small
            .getxattr(g.ino, "user.exactly_a_quarter_node")
            .await
            .unwrap(),
        Some(exact)
    );
    assert!(
        small
            .setxattr(g.ino, "user.over", &vec![0x22u8; 16 * 1024 + 1])
            .await
            .is_err(),
        "one byte over node_size/4 must refuse on small-node volumes"
    );
}
