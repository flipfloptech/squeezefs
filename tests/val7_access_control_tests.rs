//! VAL-7a/7b/7e access-control + scan-bound contracts (pre-RC spec §3
//! item VAL-7).
//!
//! * **7a** — the `.stats` / `.config` virtual inodes were mode `0444`
//!   (world-readable on a shared mount) and unconditionally emitted the
//!   full cached-block key census, the read-cache census, `active_writes`
//!   keyed by inode, every device path and every staging directory. Mode
//!   is now `0400` owned by the mount uid, and the key census is
//!   OPT-IN (`SQUEEZEFS_STATS_KEY_CENSUS=1`); the counts the umount CLI
//!   actually consumes stay unconditional.
//! * **7b** — staging and read-cache segment files/dirs were created with
//!   default modes (0755/0644). On passthrough volumes those bytes are
//!   plaintext user file data: dirs are now `0700`, segment files `0600`,
//!   and the format-grade stamp chowns through a `O_NOFOLLOW` dirfd
//!   (`fchown`) instead of a symlink-following path call.
//! * **7e** — `copy_file_range`'s staged-sibling probe walked every block
//!   of the SOURCE FILE four times per call with an unbounded result
//!   vector; it is now bounded to the copied range.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn mode_of(p: &Path) -> u32 {
    std::fs::metadata(p)
        .unwrap_or_else(|e| panic!("stat {}: {e}", p.display()))
        .permissions()
        .mode()
        & 0o7777
}

/// VAL-7a: the virtual inodes are owner-read-only, owned by the mount
/// identity. With `-o default_permissions` the kernel enforces the mode,
/// so `0400` + the mount uid is what actually keeps a co-tenant out.
#[test]
fn stats_and_config_inodes_are_owner_only() {
    assert_eq!(
        squeezefs::fuse_client::VIRTUAL_INODE_MODE,
        0o400,
        "the .stats/.config payload names every cached block key, device \
         path and staging dir — it must never be world-readable"
    );
}

/// VAL-7a: the key census is opt-in. Default payload carries the COUNTS
/// (what `squeezefs umount` consumes) but not the key lists; the census
/// knob restores the arrays verbatim for debugging.
#[test]
fn stats_key_census_is_opt_in() {
    // Default: gated off.
    std::env::remove_var("SQUEEZEFS_STATS_KEY_CENSUS");
    assert!(
        !squeezefs::fuse_client::stats_key_census_enabled(),
        "the key census must be OFF by default (VAL-7a)"
    );
    std::env::set_var("SQUEEZEFS_STATS_KEY_CENSUS", "1");
    assert!(
        squeezefs::fuse_client::stats_key_census_enabled(),
        "SQUEEZEFS_STATS_KEY_CENSUS=1 must re-arm the census"
    );
    std::env::remove_var("SQUEEZEFS_STATS_KEY_CENSUS");
    assert!(
        !squeezefs::fuse_client::stats_key_census_enabled(),
        "the gate must be read live (cold `.stats` path), never memoized"
    );
}

/// VAL-7b: the format-grade staging stamp leaves the root `0700`. A
/// staging root at 0755 exposes every staged plaintext payload name (and,
/// with a 0644 segment file, the bytes) to every local user.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stamped_staging_root_is_private() {
    let base = std::env::temp_dir().join(format!("sqz-val7b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("staging");
    squeezefs::config_ops::stamp_staging_dir(&root, false)
        .await
        .expect("stamp a fresh staging root");
    assert_eq!(
        mode_of(&root),
        0o700,
        "a stamped staging root must be owner-only — it holds plaintext \
         staged payloads on passthrough volumes"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// VAL-7b: `create_private_dir_all` is the one policy point for staging
/// and read-cache directories — 0700 regardless of the process umask,
/// and idempotent on an existing dir (it re-asserts the mode).
#[test]
fn private_dir_helper_is_umask_independent_and_idempotent() {
    let base = std::env::temp_dir().join(format!("sqz-val7b-dir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let leaf = base.join("a/b/c");
    squeezefs::config_ops::create_private_dir_all(&leaf).expect("create private dir");
    assert_eq!(mode_of(&leaf), 0o700, "leaf staging dir must be 0700");

    // Pre-existing at a permissive mode: the helper tightens it.
    std::fs::set_permissions(&leaf, std::fs::Permissions::from_mode(0o755)).unwrap();
    squeezefs::config_ops::create_private_dir_all(&leaf).expect("re-assert private dir");
    assert_eq!(
        mode_of(&leaf),
        0o700,
        "an existing staging dir must be tightened, not left as found"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// VAL-7b: segment files (the mmapped staging/read-cache rings — literal
/// plaintext user data on passthrough volumes) are created `0600`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_and_read_cache_segments_are_private() {
    let base = std::env::temp_dir().join(format!("sqz-val7b-seg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let alloc = std::sync::Arc::new(
        squeezefs::block_allocator::BlockAllocator::new("val7b")
            .await
            .expect("allocator"),
    );
    let dev = std::sync::Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new("/dev/null"));
    let cache = squeezefs::cache::TieredCache::new(
        vec![base.clone()],
        Some("8MB"),
        Some("8MB"),
        Some("8MB"),
        Some("8MB"),
        alloc,
        dev,
        None,
    )
    .await
    .expect("tiered cache over a private staging root");
    // Keep the cache alive while the tree is inspected.
    assert!(!cache.nvme.staging_dirs().is_empty());

    let mut checked_dirs = 0usize;
    let mut checked_files = 0usize;
    for seg in ["cache_segment", "staging_segment"] {
        let d = base.join(seg);
        assert!(d.is_dir(), "{} must exist", d.display());
        assert_eq!(
            mode_of(&d),
            0o700,
            "{} must be owner-only (VAL-7b)",
            d.display()
        );
        checked_dirs += 1;
        for ent in std::fs::read_dir(&d).unwrap().flatten() {
            if ent.file_type().map(|t| t.is_file()).unwrap_or(false) {
                assert_eq!(
                    mode_of(&ent.path()) & 0o077,
                    0,
                    "{} is a plaintext segment ring — group/other must have \
                     no access (VAL-7b)",
                    ent.path().display()
                );
                checked_files += 1;
            }
        }
    }
    assert_eq!(checked_dirs, 2, "both segment dirs must be inspected");
    assert!(
        checked_files > 0,
        "the harness must have observed at least one segment file"
    );
    drop(cache);
    let _ = std::fs::remove_dir_all(&base);
}

/// VAL-7e: the staged-sibling probe is bounded to the COPIED range, not
/// the source file's size. The pure range function is the contract: a
/// 4 KiB copy at offset 0 of a 1 TiB file probes ONE block, and a
/// whole-file copy still covers every block (the clone fast path depends
/// on that).
#[test]
fn copy_probe_range_is_bounded_to_the_copied_extent() {
    use squeezefs::fuse_client::copy_probe_block_range;
    let bs = 4 * 1024 * 1024u64;
    let blocks_1tib = (1u64 << 40).div_ceil(bs) as u32;

    // Small copy at the head of a huge file: one block, not 262 144.
    let (lo, hi) = copy_probe_block_range(0, 4096, blocks_1tib, bs);
    assert_eq!((lo, hi), (0, 1), "a 4 KiB head copy must probe one block");

    // Small copy deep in the file: exactly its own block.
    let off = 900 * bs + 17;
    let (lo, hi) = copy_probe_block_range(off, 4096, blocks_1tib, bs);
    assert_eq!(
        (lo, hi),
        (900, 901),
        "a mid-file copy must probe only the block it touches"
    );

    // Whole-file copy: full coverage (the clone fast path's precondition).
    let (lo, hi) = copy_probe_block_range(0, 1u64 << 40, blocks_1tib, bs);
    assert_eq!(
        (lo, hi),
        (0, blocks_1tib),
        "a whole-file copy must still probe every block"
    );

    // Length beyond EOF clamps to the block count (never a huge empty loop).
    let (lo, hi) = copy_probe_block_range(0, u64::MAX, 3, bs);
    assert_eq!(
        (lo, hi),
        (0, 3),
        "the scan clamps to the file's block count"
    );

    // A copy starting past the last block probes nothing.
    let (lo, hi) = copy_probe_block_range(10 * bs, 4096, 3, bs);
    assert!(lo >= hi, "a past-EOF source offset must probe nothing");

    // Zero length probes nothing.
    let (lo, hi) = copy_probe_block_range(0, 0, 10, bs);
    assert!(lo >= hi, "a zero-length copy must probe nothing");
}
