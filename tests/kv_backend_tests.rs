//! PR K6a integration tests: superblock v3 + the dual-format version gate,
//! the offline bulk image builder, and the `KvMetaBackend` read side —
//! design `docs/design-cow-kv-metadata.md` §4.1/§4.5/§5.1/§6.1/§6.2 and the
//! pre-resolved OQ 1 (ring clamp) / OQ 2 (dual atomicity fields) decisions.
//!
//! Contracts pinned:
//! - **Read-side conformance over BOTH formats** (`rstest` over the
//!   `VolumeBackend` dispatch enum): the same population, described once,
//!   is built on a v2 volume (the test-surface-only v2 formatter + the
//!   `Metadata` trait) and as a builder-produced v3 image; every
//!   lookup/getattr/readdir/getxattr/listxattr assertion runs verbatim
//!   against both. v2 keeps its byte-identical behavior (its own suites
//!   pin that); these cases pin that v3 *agrees* with it.
//! - **The §6.1 version gate**: blank, v2, v3, foreign-magic, and
//!   future-version sector 0s classify loudly and distinctly; unknown
//!   incompat feature bits refuse naming the bits; the v3 checksum covers
//!   the whole sector.
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
//! - **v3 format guards**: the same preflight policy as v2 (already
//!   formatted ⇒ refused without `--force`).
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
use squeezefs::meta_backend::storage::MetaLvStorage;
use squeezefs::meta_backend::{MetaLvBackend, Metadata, VolumeBackend};
use std::collections::HashMap;
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// Harness: one population, two formats.
// ---------------------------------------------------------------------------

/// v2 volumes need the 72 MiB xattr region + table headroom.
const V2_VOL_LEN: u64 = 128 * 1024 * 1024;
/// v3 test volumes: small node size + overridden 1 MiB ring keep them tiny.
const V3_VOL_LEN: u64 = 64 * 1024 * 1024;
const V3_NODE_SIZE: usize = 64 * 1024;
const V3_RING_LEN: u64 = 1024 * 1024;
/// Fixed identity for deterministic images.
const TEST_SEED: u64 = 0x5EED_CAFE_F00D_D00D;
const TEST_UUID: [u8; 16] = *b"kv-backend-test!";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    V2,
    V3,
}

struct Population {
    backend: VolumeBackend,
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

/// The shared description: /docs (0750 1000:1000), /docs/readme.txt (0644
/// 1000:1000, 4096 B, two xattrs), /hello.bin (0600 0:0, 0 B), /empty
/// (0755 0:0), /hard.lnk = hard link to hello.bin.
async fn populate_v2(file: &NamedTempFile) -> HashMap<&'static str, u64> {
    let storage = MetaLvStorage::open(file.path(), V2_VOL_LEN).unwrap();
    MetaLvBackend::format_v2_for_tests(&storage, true, true, None)
        .await
        .unwrap();
    let be = MetaLvBackend::new(storage);

    let mut inos = HashMap::new();
    let docs = be
        .create(ROOT_INO, "docs", libc::S_IFDIR | 0o750, 1000, 1000)
        .await
        .unwrap();
    inos.insert("docs", docs.ino);
    let readme = be
        .create(docs.ino, "readme.txt", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap();
    inos.insert("readme.txt", readme.ino);
    be.setattr(readme.ino, None, None, None, Some(4096), None, None, None)
        .await
        .unwrap();
    be.setxattr(readme.ino, "user.color", b"blue")
        .await
        .unwrap();
    be.setxattr(readme.ino, "user.big", &big_xattr())
        .await
        .unwrap();
    let hello = be
        .create(ROOT_INO, "hello.bin", libc::S_IFREG | 0o600, 0, 0)
        .await
        .unwrap();
    inos.insert("hello.bin", hello.ino);
    let empty = be
        .create(ROOT_INO, "empty", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    inos.insert("empty", empty.ino);
    be.link(hello.ino, ROOT_INO, "hard.lnk").await.unwrap();
    inos
}

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

async fn population(kind: Kind) -> Population {
    let file = NamedTempFile::new().expect("temp volume");
    let inos = match kind {
        Kind::V2 => populate_v2(&file).await,
        Kind::V3 => {
            file.as_file().set_len(V3_VOL_LEN).unwrap();
            let (b, inos) = describe_v3();
            b.build(file.path(), V3_VOL_LEN)
                .await
                .expect("build v3 image");
            inos
        }
    };
    let backend = VolumeBackend::open_for_mount(file.path().to_str().unwrap())
        .await
        .expect("open_for_mount");
    assert_eq!(
        backend.format_version(),
        match kind {
            Kind::V2 => 2,
            Kind::V3 => 3,
        },
        "the version gate must dispatch by superblock version"
    );
    Population {
        backend,
        inos,
        _file: file,
    }
}

// ---------------------------------------------------------------------------
// Read-side conformance: the same assertions over both backends.
// ---------------------------------------------------------------------------

#[rstest::rstest]
#[case::v2(Kind::V2)]
#[case::v3(Kind::V3)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_lookup_and_getattr(#[case] kind: Kind) {
    let p = population(kind).await;
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

#[rstest::rstest]
#[case::v2(Kind::V2)]
#[case::v3(Kind::V3)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_readdir_sets(#[case] kind: Kind) {
    let p = population(kind).await;
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

#[rstest::rstest]
#[case::v2(Kind::V2)]
#[case::v3(Kind::V3)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_xattr_surface(#[case] kind: Kind) {
    let p = population(kind).await;
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

#[rstest::rstest]
#[case::v2(Kind::V2)]
#[case::v3(Kind::V3)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_hard_links(#[case] kind: Kind) {
    let p = population(kind).await;
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
    let err = VolumeBackend::open_for_mount(blank.path().to_str().unwrap())
        .await
        .expect_err("mounting a blank volume must fail loud")
        .to_string();
    assert!(
        err.contains("format"),
        "the blank-volume refusal must point at `squeezefs format`, got: {err}"
    );

    // v2: classifies as V2 with the superblock parsed.
    let v2 = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(v2.path(), V2_VOL_LEN).unwrap();
    MetaLvBackend::format_v2_for_tests(&storage, true, true, None)
        .await
        .unwrap();
    match classify_volume(v2.path()).await.expect("v2 classifies") {
        VolumeFormat::V2(sb) => assert_eq!(sb.version, 2),
        other => panic!("a v2 volume must classify V2, got {other:?}"),
    }

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
    squeezefs::uring_fs::write_at(
        future.path(),
        8,
        bytes::Bytes::copy_from_slice(&99u32.to_le_bytes()),
    )
    .await
    .unwrap();
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
    let p = population(Kind::V3).await;
    let be = match &p.backend {
        VolumeBackend::V3(be) => be.clone(),
        _ => unreachable!(),
    };
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
    // path (VolumeBackend read dispatch on ino 1).
    let vol = VolumeBackend::open_for_mount(file.path().to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(vol.format_version(), 3);
    assert_eq!(
        vol.getxattr(ROOT_INO, "user.squeezefs.format_config")
            .await
            .unwrap(),
        Some(b"{\"probe\":true}".to_vec()),
        "format must record the config xattr in the v3 xattr tree"
    );

    // A v2 volume is protected by the same guard: refused without force,
    // converted with it.
    let v2 = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(v2.path(), V2_VOL_LEN).unwrap();
    MetaLvBackend::format_v2_for_tests(&storage, true, true, None)
        .await
        .unwrap();
    assert!(
        format_v3(v2.path(), V2_VOL_LEN, &format_opts(false))
            .await
            .is_err(),
        "a formatted v2 volume must be refused without --force"
    );
    format_v3(v2.path(), V2_VOL_LEN, &format_opts(true))
        .await
        .expect("--force reformats a v2 volume to v3");
    assert_eq!(
        VolumeBackend::open_for_mount(v2.path().to_str().unwrap())
            .await
            .unwrap()
            .format_version(),
        3
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
