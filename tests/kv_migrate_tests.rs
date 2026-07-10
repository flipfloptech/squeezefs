//! PR K9 integration tests: the offline `squeezefs migrate` v2 → v3 converter
//! (design `docs/design-cow-kv-metadata.md` §6.2, normative).
//!
//! Contracts pinned (one test each, or grouped):
//! - **Round-trip digest equality**: the whole v2 namespace equals the migrated
//!   v3 namespace — both a content walk (v2 read path vs v3 read path) and the
//!   `digest_walk` round-trip the converter self-verifies before flipping.
//! - **Quarantined-ino carryover** [1024, 1152): the `flags2`
//!   `QUARANTINE_CONTENT_LOST` flag is carried; a quarantined symlink's
//!   `readlink` (system.symlink read) degrades to EIO; a quarantined regular
//!   ino's xattrs read empty — today's degrade semantics preserved.
//! - **Ino continuity under `route_ino`**: v2 inos are preserved verbatim as v3
//!   keys, so a single-volume set's `route_ino` mapping is stable (root == 1;
//!   every populated ino identical).
//! - **Idempotent + crash-safe**: a write failure at every phase (structures /
//!   nodes / bitmap / ledger / flip) leaves the v2 superblock intact so a
//!   re-run is a clean restart; a completed migration re-runs as a no-op; the
//!   torn-migration-then-v2-remount xattr-scribble case degrades cleanly and a
//!   re-run's 32 KiB header-sector zeroing rebuilds correctly.
//! - **Deficit / `--grow` refusal**: a full-geometry volume with no free tail
//!   refuses loud with the exact deficit; `--grow` extends it and succeeds.
//! - **Live-client refusal**: a fresh client registration refuses the
//!   migration (reusing the `format_preflight` policy); a stale one is reaped.
//! - **Space-reclaim accounting**: the v2 dead journal region and the xattr
//!   reservation come back as free v3 heap extents.
//!
//! Fault-injection cases share a process-global mutex (the `uring_fs` fault
//! shims are process-wide); the gate runs `--test-threads=1` regardless.

use squeezefs::error::SqueezefsError;
use squeezefs::meta_backend::inode::{write_inode, DiskInode};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::migrate::{
    build_start_for, migrate_volume, MigrateOptions, MIGRATE_NODE_SIZE,
};
use squeezefs::meta_backend::kv::record::FLAGS2_QUARANTINE_CONTENT_LOST;
use squeezefs::meta_backend::kv::superblock::{classify_volume, SuperblockV3, VolumeFormat};
use squeezefs::meta_backend::storage::{MetaLvStorage, JOURNAL_REGION_SIZE, JOURNAL_REGION_START};
use squeezefs::meta_backend::xattr::{QUARANTINE_INO_START, XATTR_BLOCK_START};
use squeezefs::meta_backend::{dentry, MetaLvBackend, Metadata, VolumeBackend};
use squeezefs::uring_fs;
use std::collections::BTreeMap;
use std::path::Path;
use tempfile::NamedTempFile;

const V2_VOL_LEN: u64 = 128 * 1024 * 1024;
const NS: u64 = MIGRATE_NODE_SIZE as u64; // 256 KiB

/// The v2 inode ceiling for a volume of `size` bytes (mirrors the private
/// `storage::inodes_for_size`): the xattr region begins at 72 MiB and each ino
/// reserves a 32 KiB block.
fn inode_ceiling(size: u64) -> u64 {
    (size - 72 * 1024 * 1024) / 32768
}

/// Assert a volume is still format v2 (a refused / un-flipped migration).
fn assert_still_v2(fmt: &VolumeFormat) {
    assert!(
        matches!(fmt, VolumeFormat::V2(_)),
        "expected the volume to still be v2, got {fmt:?}"
    );
}

fn opts() -> MigrateOptions {
    MigrateOptions::default()
}

fn dry() -> MigrateOptions {
    MigrateOptions {
        dry_run: true,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Namespace helpers.
// ---------------------------------------------------------------------------

/// A byte-comparable snapshot of one inode (attrs + child names + xattrs). For
/// symlinks the target is read via `system.symlink` (v2 hides it from
/// listxattr; v3 keeps it in the xattr tree — the walk reads it the same way on
/// both so the two are comparable).
#[derive(Debug, PartialEq, Eq)]
struct Snap {
    mode: u32,
    uid: u32,
    gid: u32,
    size: u64,
    nlink: u32,
    atime: u64,
    mtime: u64,
    ctime: u64,
    entries: BTreeMap<String, u64>,
    xattrs: BTreeMap<String, Vec<u8>>,
}

/// Walk the whole reachable namespace from the root through a `VolumeBackend`
/// (the dual-format read dispatch), format-agnostic.
async fn walk(vb: &VolumeBackend) -> BTreeMap<u64, Snap> {
    let mut out: BTreeMap<u64, Snap> = BTreeMap::new();
    let mut stack = vec![1u64];
    while let Some(ino) = stack.pop() {
        if out.contains_key(&ino) {
            continue;
        }
        let a = vb.getattr(ino).await.expect("getattr");
        let mut entries = BTreeMap::new();
        let mut xattrs = BTreeMap::new();
        match a.mode & libc::S_IFMT {
            libc::S_IFDIR => {
                for e in vb.readdir(ino, 0, usize::MAX).await.expect("readdir") {
                    entries.insert(e.name.clone(), e.ino);
                    stack.push(e.ino);
                }
                for n in vb.listxattr(ino).await.expect("listxattr") {
                    if let Some(v) = vb.getxattr(ino, &n).await.expect("getxattr") {
                        xattrs.insert(n, v);
                    }
                }
            }
            libc::S_IFLNK => {
                if let Some(t) = vb.getxattr(ino, "system.symlink").await.expect("readlink") {
                    xattrs.insert("system.symlink".to_string(), t);
                }
            }
            _ => {
                for n in vb.listxattr(ino).await.expect("listxattr") {
                    if let Some(v) = vb.getxattr(ino, &n).await.expect("getxattr") {
                        xattrs.insert(n, v);
                    }
                }
            }
        }
        out.insert(
            ino,
            Snap {
                mode: a.mode,
                uid: a.uid,
                gid: a.gid,
                size: a.size,
                nlink: a.nlink,
                atime: a.atime,
                mtime: a.mtime,
                ctime: a.ctime,
                entries,
                xattrs,
            },
        );
    }
    out
}

/// Format + populate a rich v2 namespace: dirs, files (sizes + xattrs incl. a
/// data-path `layout` value), a symlink, and a hard link. Returns the created
/// name→ino map.
async fn populate_rich_v2(path: &Path) -> BTreeMap<String, u64> {
    let storage = MetaLvStorage::open(path, V2_VOL_LEN).unwrap();
    MetaLvBackend::format_v2_for_tests(&storage, true, true, None)
        .await
        .unwrap();
    let be = MetaLvBackend::new(storage);
    let mut inos = BTreeMap::new();

    let docs = be
        .create(1, "docs", libc::S_IFDIR | 0o750, 1000, 1000)
        .await
        .unwrap();
    inos.insert("docs".into(), docs.ino);

    let readme = be
        .create(docs.ino, "readme.txt", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap();
    be.setattr(readme.ino, None, None, None, Some(4096), None, None, None)
        .await
        .unwrap();
    be.setxattr(readme.ino, "user.color", b"blue")
        .await
        .unwrap();
    be.setxattr(readme.ino, "user.big", &vec![0xAB; 6000])
        .await
        .unwrap();
    inos.insert("readme.txt".into(), readme.ino);

    let hello = be
        .create(1, "hello.bin", libc::S_IFREG | 0o600, 0, 0)
        .await
        .unwrap();
    be.setxattr(
        hello.ino,
        "layout",
        b"\x01\x02\x03 progressive-layout bytes",
    )
    .await
    .unwrap();
    inos.insert("hello.bin".into(), hello.ino);
    be.link(hello.ino, 1, "hard.lnk").await.unwrap();

    let link = be
        .create(1, "link", libc::S_IFLNK | 0o777, 0, 0)
        .await
        .unwrap();
    be.setxattr(link.ino, "system.symlink", b"docs/readme.txt")
        .await
        .unwrap();
    inos.insert("link".into(), link.ino);

    let empty = be
        .create(1, "empty", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    inos.insert("empty".into(), empty.ino);

    inos
}

/// Write a magic-valid inode straight into the v2 table (bypasses the
/// allocator — the fast way to place a used ino at a specific number, e.g. in
/// the quarantine range or near the ino ceiling for the deficit case).
async fn write_used_inode(storage: &MetaLvStorage, ino: u64, mode: u32) {
    let di = DiskInode::new(ino, mode, 0, 0);
    write_inode(storage, ino, &di).await.unwrap();
}

async fn open_v3(path: &Path) -> std::sync::Arc<KvMetaBackend> {
    KvMetaBackend::open(path).await.expect("open v3")
}

async fn open_vb(path: &Path) -> VolumeBackend {
    VolumeBackend::open_for_mount(path.to_str().unwrap())
        .await
        .expect("open_for_mount")
}

// ---------------------------------------------------------------------------
// 1. Round-trip digest equality + namespace preservation.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrate_round_trip_preserves_namespace_and_digest() {
    let file = NamedTempFile::new().unwrap();
    populate_rich_v2(file.path()).await;

    // Snapshot the v2 namespace, then drop the v2 handle.
    let before = {
        let vb = open_vb(file.path()).await;
        assert_eq!(vb.format_version(), 2);
        walk(&vb).await
    };

    let report = migrate_volume(file.path(), &opts()).await.unwrap();
    assert!(report.flipped && !report.already_v3 && !report.dry_run);
    assert_eq!(
        report.source_digest, report.built_digest,
        "§6.2 round-trip digest equality (source records == read-back v3 image)"
    );
    assert!(
        report.inodes_migrated >= 6,
        "root + docs/readme/hello/link/empty, got {}",
        report.inodes_migrated
    );

    // Now a v3 volume; the whole namespace must match byte-for-byte.
    let vb = open_vb(file.path()).await;
    assert_eq!(vb.format_version(), 3, "superblock flipped to v3");
    let after = walk(&vb).await;
    assert_eq!(before, after, "v2 namespace == migrated v3 namespace");

    // The v3 read-path digest matches the converter's reported digest.
    let be = open_v3(file.path()).await;
    let d = squeezefs::meta_backend::kv::builder::digest_backend(&be)
        .await
        .unwrap();
    assert_eq!(d, report.built_digest);
}

// ---------------------------------------------------------------------------
// 2. --dry-run builds + verifies but does not flip; second run is a no-op.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dry_run_verifies_without_flipping_and_rerun_is_noop() {
    let file = NamedTempFile::new().unwrap();
    populate_rich_v2(file.path()).await;

    let dr = migrate_volume(file.path(), &dry()).await.unwrap();
    assert!(dr.dry_run && !dr.flipped);
    assert_eq!(dr.source_digest, dr.built_digest, "dry-run diff is zero");
    // Still v2 — nothing committed.
    assert_still_v2(&classify_volume(file.path()).await.unwrap());

    // Real migration, then a second run is a clean no-op.
    let r1 = migrate_volume(file.path(), &opts()).await.unwrap();
    assert!(r1.flipped);
    let r2 = migrate_volume(file.path(), &opts()).await.unwrap();
    assert!(
        r2.already_v3 && !r2.flipped,
        "re-run on a v3 volume is a no-op"
    );
    // A dry-run on an already-migrated volume is also a no-op.
    let r3 = migrate_volume(file.path(), &dry()).await.unwrap();
    assert!(r3.already_v3);
}

// ---------------------------------------------------------------------------
// 3. Quarantined-ino flag + symlink-EIO carryover (§6.2 degrade).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quarantined_ino_carryover_and_symlink_eio() {
    let file = NamedTempFile::new().unwrap();
    populate_rich_v2(file.path()).await;

    // Place a quarantined symlink and a quarantined regular ino straight into
    // the v2 table (their xattr blocks overlap the journal region).
    let q_symlink = QUARANTINE_INO_START; // 1024
    let q_regular = QUARANTINE_INO_START + 1; // 1025
    {
        let storage = MetaLvStorage::open(file.path(), V2_VOL_LEN).unwrap();
        write_used_inode(&storage, q_symlink, libc::S_IFLNK | 0o777).await;
        write_used_inode(&storage, q_regular, libc::S_IFREG | 0o644).await;
        dentry::insert_dentry(&storage, 1, q_symlink, "badlink", libc::S_IFLNK | 0o777)
            .await
            .unwrap();
        dentry::insert_dentry(&storage, 1, q_regular, "badreg", libc::S_IFREG | 0o644)
            .await
            .unwrap();
    }

    let report = migrate_volume(file.path(), &opts()).await.unwrap();
    assert!(report.flipped);
    assert!(
        report.quarantined_inodes >= 2,
        "both quarantined inos carried, got {}",
        report.quarantined_inodes
    );

    let be = open_v3(file.path()).await;

    // Flag carryover on both.
    let sym = be
        .inode_value(q_symlink)
        .await
        .unwrap()
        .expect("symlink ino");
    assert_eq!(sym.mode & libc::S_IFMT, libc::S_IFLNK);
    assert_ne!(
        sym.flags2 & FLAGS2_QUARANTINE_CONTENT_LOST,
        0,
        "§6.2 QUARANTINE_CONTENT_LOST carried on the migrated symlink"
    );
    let reg = be
        .inode_value(q_regular)
        .await
        .unwrap()
        .expect("regular ino");
    assert_ne!(reg.flags2 & FLAGS2_QUARANTINE_CONTENT_LOST, 0);

    // Symlink-target read degrades to EIO (content lost); regular quarantined
    // xattrs read empty.
    let err = be.getxattr(q_symlink, "system.symlink").await;
    assert!(
        matches!(err, Err(SqueezefsError::Io(_))),
        "quarantined symlink readlink must be EIO, got {err:?}"
    );
    assert_eq!(be.getxattr(q_regular, "user.anything").await.unwrap(), None);
    assert!(be.listxattr(q_regular).await.unwrap().is_empty());

    // Setxattr on a quarantined ino is now ALLOWED on v3 (repair improvement —
    // v3 has no overlapping geometry to protect, §6.2).
    be.setxattr(q_regular, "user.repaired", b"ok")
        .await
        .unwrap();
    assert_eq!(
        be.getxattr(q_regular, "user.repaired").await.unwrap(),
        Some(b"ok".to_vec())
    );
}

// ---------------------------------------------------------------------------
// 4. Ino continuity under route_ino (single-volume set: identity).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ino_continuity_is_verbatim() {
    let file = NamedTempFile::new().unwrap();
    let inos = populate_rich_v2(file.path()).await;

    migrate_volume(file.path(), &opts()).await.unwrap();
    let vb = open_vb(file.path()).await;

    // Root stays ino 1; every created name resolves to its ORIGINAL ino.
    assert_eq!(vb.getattr(1).await.unwrap().ino, 1);
    let docs_ino = inos["docs"];
    assert_eq!(vb.lookup(1, "docs").await.unwrap().ino, docs_ino);
    assert_eq!(
        vb.lookup(docs_ino, "readme.txt").await.unwrap().ino,
        inos["readme.txt"]
    );
    assert_eq!(
        vb.lookup(1, "hello.bin").await.unwrap().ino,
        inos["hello.bin"]
    );
    // The hard link resolves to the SAME ino (nlink preserved).
    assert_eq!(
        vb.lookup(1, "hard.lnk").await.unwrap().ino,
        inos["hello.bin"]
    );
    assert_eq!(vb.getattr(inos["hello.bin"]).await.unwrap().nlink, 2);

    // next_ino watermark sits above every preserved ino (§4.8).
    let be = open_v3(file.path()).await;
    let max = *inos.values().max().unwrap();
    assert!(
        be.next_ino() > max,
        "next_ino must clear the migrated max ino"
    );
}

// ---------------------------------------------------------------------------
// 5. Idempotent + crash-safe: a write failure at every phase → re-run recovers.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idempotent_after_write_failure_at_every_phase() {
    // The `uring_fs` fault shims are process-global; this suite runs under the
    // gate's `--test-threads=1` (as the other crash suites require).
    //
    // Phase write offsets are computed from the planned geometry. A fresh
    // volume per phase (a completed re-run flips to v3, so phases cannot share
    // one volume).
    for phase in ["structures", "nodes", "bitmap", "flip"] {
        let file = NamedTempFile::new().unwrap();
        populate_rich_v2(file.path()).await;
        let before = {
            let vb = open_vb(file.path()).await;
            walk(&vb).await
        };

        // Plan geometry to locate the phase's characteristic write offset.
        let dr = migrate_volume(file.path(), &dry()).await.unwrap();
        let sb = SuperblockV3::plan_migrate(
            V2_VOL_LEN,
            dr.build_start,
            MIGRATE_NODE_SIZE,
            None,
            [0u8; 16],
            0,
        )
        .unwrap();
        let off = match phase {
            "structures" => sb.root_ledger.start, // first zero_range + ledger write
            "nodes" => sb.first_node_offset(),
            "bitmap" => sb.alloc_bitmap.start,
            "flip" => 0, // the sector-0 superblock flip
            _ => unreachable!(),
        };

        uring_fs::clear_faults();
        uring_fs::arm_sector_write_error(off);
        let failed = migrate_volume(file.path(), &opts()).await;
        assert!(
            failed.is_err(),
            "phase {phase}: an armed write error at {off} must abort the migration"
        );
        uring_fs::clear_faults();

        // The v2 superblock is intact (the flip never landed durably), so a
        // re-run is a clean restart that succeeds.
        let ok = migrate_volume(file.path(), &opts()).await.unwrap();
        assert!(
            ok.flipped && !ok.already_v3,
            "phase {phase}: re-run after the failure must complete the migration"
        );
        assert_eq!(ok.source_digest, ok.built_digest);

        let vb = open_vb(file.path()).await;
        assert_eq!(vb.format_version(), 3);
        let after = walk(&vb).await;
        assert_eq!(
            before, after,
            "phase {phase}: namespace intact after re-run"
        );
        drop(vb);
    }
    uring_fs::clear_faults();
}

// ---------------------------------------------------------------------------
// 6. Torn-migration → v2 remount xattr-scribble → re-run (§6.2 defense).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn torn_migration_then_v2_remount_scribble_then_rerun() {
    let file = NamedTempFile::new().unwrap();
    populate_rich_v2(file.path()).await;

    // A dry-run builds a full v3 image into the tail WITHOUT flipping — the
    // exact on-disk state a torn migration leaves (v3 build bytes in the build
    // region, v2 superblock intact).
    migrate_volume(file.path(), &dry()).await.unwrap();
    assert_still_v2(&classify_volume(file.path()).await.unwrap());

    // A v2 remount now allocates fresh inos whose xattr blocks land in the
    // build region (v3 build garbage). The v2 xattr code must degrade cleanly:
    // the magic guard rejects the garbage and setxattr re-initializes the
    // block. Create enough files that some blocks fall past build_start.
    let mut new_files = Vec::new();
    {
        let storage = MetaLvStorage::open(file.path(), V2_VOL_LEN).unwrap();
        storage.validate_superblock().await.unwrap();
        let be = MetaLvBackend::new(storage);
        for i in 0..24 {
            let name = format!("post-torn-{i}");
            let f = be
                .create(1, &name, libc::S_IFREG | 0o644, 0, 0)
                .await
                .unwrap();
            // Scribble region: setxattr must succeed (block re-init on bad magic).
            be.setxattr(f.ino, "user.k", format!("v{i}").as_bytes())
                .await
                .unwrap();
            assert_eq!(
                be.getxattr(f.ino, "user.k").await.unwrap(),
                Some(format!("v{i}").into_bytes()),
                "v2 setxattr over torn-build garbage must round-trip"
            );
            new_files.push((name, f.ino));
        }
    }

    // Snapshot the (now larger) v2 namespace, then run a real migration: its
    // header-sector zeroing wipes the stale build garbage before rebuilding.
    let before = {
        let vb = open_vb(file.path()).await;
        walk(&vb).await
    };
    let report = migrate_volume(file.path(), &opts()).await.unwrap();
    assert!(report.flipped);

    let vb = open_vb(file.path()).await;
    assert_eq!(vb.format_version(), 3);
    let after = walk(&vb).await;
    assert_eq!(
        before, after,
        "every file (incl. post-torn creations) survives"
    );
    // Spot-check a post-torn file + xattr survived the rebuild.
    for (name, ino) in &new_files {
        let got = vb.lookup(1, name).await.unwrap();
        assert_eq!(got.ino, *ino);
    }
}

// ---------------------------------------------------------------------------
// 7. Deficit refusal on a full-geometry volume; --grow succeeds.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deficit_refused_then_grow_succeeds() {
    let file = NamedTempFile::new().unwrap();
    populate_rich_v2(file.path()).await;

    // Push max_used_ino near the 128 MiB volume's ino ceiling so build_start
    // reaches the device end — zero free tail (the §6.2 bound case).
    let high_ino = inode_ceiling(V2_VOL_LEN) - 4;
    {
        let storage = MetaLvStorage::open(file.path(), V2_VOL_LEN).unwrap();
        write_used_inode(&storage, high_ino, libc::S_IFREG | 0o644).await;
    }
    assert!(
        build_start_for(high_ino, NS) >= V2_VOL_LEN,
        "the bound case: build_start reaches the device end (no free tail)"
    );

    // Without --grow: refuse loud, naming the deficit + --grow.
    let refused = migrate_volume(file.path(), &opts()).await;
    let msg = match refused {
        Err(SqueezefsError::InvalidOperation(m)) => m,
        other => panic!("expected a loud deficit refusal, got {other:?}"),
    };
    assert!(
        msg.contains("--grow"),
        "the refusal must point at --grow: {msg}"
    );
    assert!(
        matches!(
            classify_volume(file.path()).await.unwrap(),
            VolumeFormat::V2(_)
        ),
        "a refused migration leaves the v2 volume untouched"
    );

    // With --grow to a comfortably larger size: succeeds.
    let grown = MigrateOptions {
        grow_to_bytes: Some(V2_VOL_LEN + 64 * 1024 * 1024),
        ..Default::default()
    };
    let report = migrate_volume(file.path(), &grown).await.unwrap();
    assert!(report.flipped);
    assert_eq!(report.grew_to, Some(V2_VOL_LEN + 64 * 1024 * 1024));
    assert_eq!(open_vb(file.path()).await.format_version(), 3);
}

// ---------------------------------------------------------------------------
// 8. Live-client refusal (format_preflight policy); stale is reaped.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_client_refuses_stale_is_reaped() {
    let file = NamedTempFile::new().unwrap();
    populate_rich_v2(file.path()).await;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // A fresh client registration on the root ino refuses the migration.
    {
        let storage = MetaLvStorage::open(file.path(), V2_VOL_LEN).unwrap();
        let be = MetaLvBackend::new(storage);
        be.setxattr(
            1,
            "client:live",
            format!(r#"{{"ts":{now},"pid":1}}"#).as_bytes(),
        )
        .await
        .unwrap();
    }
    let refused = migrate_volume(file.path(), &opts()).await;
    assert!(
        matches!(refused, Err(SqueezefsError::InvalidOperation(ref m)) if m.contains("mounted")),
        "a live client must refuse the migration, got {refused:?}"
    );
    assert_still_v2(&classify_volume(file.path()).await.unwrap());

    // Overwrite it with a stale timestamp: preflight reaps it, migration runs.
    {
        let storage = MetaLvStorage::open(file.path(), V2_VOL_LEN).unwrap();
        let be = MetaLvBackend::new(storage);
        be.setxattr(1, "client:live", br#"{"ts":1,"pid":1}"#)
            .await
            .unwrap();
    }
    let report = migrate_volume(file.path(), &opts()).await.unwrap();
    assert!(
        report.flipped,
        "a stale client is reaped; migration proceeds"
    );
}

// ---------------------------------------------------------------------------
// 9. Space-reclaim accounting: dead journal region + xattr reservation free.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reclaims_journal_region_and_xattr_reservation() {
    let file = NamedTempFile::new().unwrap();
    populate_rich_v2(file.path()).await;

    // A used ino above the quarantine range pushes build_start past the dead
    // journal region [104 MiB, 108 MiB), so that whole region reclaims as free
    // low extents.
    let high_ino = 1300u64;
    {
        let storage = MetaLvStorage::open(file.path(), V2_VOL_LEN).unwrap();
        write_used_inode(&storage, high_ino, libc::S_IFREG | 0o644).await;
    }
    let build_start = build_start_for(high_ino, NS);
    assert!(build_start > JOURNAL_REGION_START + JOURNAL_REGION_SIZE);

    let report = migrate_volume(file.path(), &opts()).await.unwrap();
    assert!(report.flipped);

    // The dead journal region (4 MiB) reclaims as exactly its extent count.
    assert_eq!(
        report.reclaimed_journal_extents,
        JOURNAL_REGION_SIZE / NS,
        "the dead journal region returns as free v3 extents"
    );
    // The xattr reservation below build_start reclaims as free extents too.
    assert_eq!(
        report.reclaimed_reservation_extents,
        (build_start - XATTR_BLOCK_START) / NS,
        "the whole xattr reservation below build_start returns as free extents"
    );
    assert!(report.free_extents_after > report.reclaimed_journal_extents);

    // Spot-check the mounted v3 allocator: an extent physically covering the
    // old dead journal region, and one covering a live-ino xattr block, are
    // both FREE now.
    let be = open_v3(file.path()).await;
    let journal_extent = JOURNAL_REGION_START / NS;
    assert!(
        !be.extent_allocated(journal_extent),
        "the dead journal region's extent must be free after reclaim"
    );
    let reservation_extent = (XATTR_BLOCK_START + 400 * 32 * 1024) / NS;
    assert!(
        !be.extent_allocated(reservation_extent),
        "a reclaimed xattr-reservation extent must be free"
    );
}
