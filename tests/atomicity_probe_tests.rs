//! Sector-atomicity probe + strict mount mode (design-wal-crash-consistency
//! §4.6, PR 6, Key Decision 2).
//!
//! D1's guarantee ("per-sector consistency under power loss") is only as
//! good as the storage stack, so stop assuming and start probing: classify
//! each meta volume at mount from sysfs block attributes, surface the
//! classification on the stats inode, and let operators fail the mount
//! loud (`--strict-meta-atomicity`) when the volume cannot promise 4 KiB
//! atomic writes.
//!
//! Probe scope is sysfs-only (resolved Open Question 5): the NVMe identify
//! (AWUPF) ioctl needs CAP_SYS_ADMIN paths this daemon otherwise avoids;
//! on pre-6.11 kernels (no `atomic_write_unit_max_bytes`) classification
//! honestly tops out at `likely`.

use squeezefs::meta_backend::atomicity::{
    classify_block_attrs, enforce_strict, probe_meta_volume, AtomicityClass,
};
use tempfile::NamedTempFile;

/// Classification matrix (§4.6): `atomic4k` iff the logical block size or
/// the advertised atomic-write unit covers a whole 4 KiB sector; `likely`
/// for 512e physical-4K stacks; `unknown` when the attributes give no
/// 4 KiB-atomicity evidence (512n) or cannot be read.
#[test]
fn test_classify_block_attrs_matrix() {
    // 4 KiB-LBA device.
    assert_eq!(
        classify_block_attrs(Some(4096), Some(4096), None),
        AtomicityClass::Atomic4k
    );
    // 512e with a >= 4 KiB atomic-write unit (kernels >= 6.11).
    assert_eq!(
        classify_block_attrs(Some(512), Some(512), Some(4096)),
        AtomicityClass::Atomic4k
    );
    assert_eq!(
        classify_block_attrs(Some(512), Some(512), Some(8192)),
        AtomicityClass::Atomic4k
    );
    // 512e / physical 4K, no atomic-unit attribute: physically plausible.
    assert_eq!(
        classify_block_attrs(Some(512), Some(4096), None),
        AtomicityClass::Likely
    );
    // Sub-4K atomic unit does not help a 512e stack.
    assert_eq!(
        classify_block_attrs(Some(512), Some(4096), Some(512)),
        AtomicityClass::Likely
    );
    // 512n: no evidence at all.
    assert_eq!(
        classify_block_attrs(Some(512), Some(512), None),
        AtomicityClass::Unknown
    );
    // Missing attributes: unknown, never a guess.
    assert_eq!(
        classify_block_attrs(None, None, None),
        AtomicityClass::Unknown
    );
}

/// A regular file (the dev/test substrate) classifies as `file-backed` —
/// the D2 row, stated instead of implied.
#[test]
fn test_probe_file_backed_classification() {
    let tmp = NamedTempFile::new().unwrap();
    let class = probe_meta_volume(tmp.path());
    assert_eq!(class, AtomicityClass::FileBacked);
    assert_eq!(class.as_str(), "file-backed");
}

/// A missing path must probe as `unknown` (never panic, never guess).
#[test]
fn test_probe_missing_path_is_unknown() {
    assert_eq!(
        probe_meta_volume(std::path::Path::new("/nonexistent/squeezefs/meta.bin")),
        AtomicityClass::Unknown
    );
}

/// Strict mode: classification below `atomic4k` fails loud, naming the
/// classification and the flag; `atomic4k` passes. Default-off behavior is
/// the caller's (mount) concern — the gate itself is absolute.
#[test]
fn test_enforce_strict_rejects_below_atomic4k() {
    let path = std::path::Path::new("/tmp/meta.bin");
    enforce_strict(AtomicityClass::Atomic4k, path).expect("atomic4k must pass strict mode");
    for class in [
        AtomicityClass::Likely,
        AtomicityClass::Unknown,
        AtomicityClass::FileBacked,
    ] {
        let err = enforce_strict(class, path)
            .expect_err("below-atomic4k classification must fail strict mode");
        let msg = err.to_string();
        assert!(
            msg.contains(class.as_str()),
            "strict error must name the classification, got: {msg}"
        );
        assert!(
            msg.contains("strict-meta-atomicity"),
            "strict error must name the flag, got: {msg}"
        );
    }
}

/// Display strings are the stats-surface contract (§Observability).
#[test]
fn test_classification_strings_are_stable() {
    assert_eq!(AtomicityClass::Atomic4k.as_str(), "atomic4k");
    assert_eq!(AtomicityClass::Likely.as_str(), "likely");
    assert_eq!(AtomicityClass::Unknown.as_str(), "unknown");
    assert_eq!(AtomicityClass::FileBacked.as_str(), "file-backed");
    assert_eq!(format!("{}", AtomicityClass::Likely), "likely");
}
