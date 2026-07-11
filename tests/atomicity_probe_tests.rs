//! Sector-atomicity probe (design-wal-crash-consistency §4.6, PR 6, Key
//! Decision 2).
//!
//! Stop assuming and start probing: classify each meta volume at mount
//! from sysfs block attributes and surface the classification on the
//! stats inode as `meta_volume_atomicity_physical`. Informational only —
//! the v3 metadata contract (`cow-checksummed`) holds by construction,
//! which is why the retired `--strict-meta-atomicity` gate (v2-only by
//! design) was deleted with v2 support.
//!
//! Probe scope is sysfs-only (resolved Open Question 5): the NVMe identify
//! (AWUPF) ioctl needs CAP_SYS_ADMIN paths this daemon otherwise avoids;
//! on pre-6.11 kernels (no `atomic_write_unit_max_bytes`) classification
//! honestly tops out at `likely`.

use squeezefs::meta_backend::atomicity::{
    classify_block_attrs, probe_meta_volume, AtomicityClass, META_VOLUME_ATOMICITY_COW,
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

/// Display strings are the stats-surface contract (§Observability) — the
/// physical probe classes plus the constant v3 contract class.
#[test]
fn test_classification_strings_are_stable() {
    assert_eq!(AtomicityClass::Atomic4k.as_str(), "atomic4k");
    assert_eq!(AtomicityClass::Likely.as_str(), "likely");
    assert_eq!(AtomicityClass::Unknown.as_str(), "unknown");
    assert_eq!(AtomicityClass::FileBacked.as_str(), "file-backed");
    assert_eq!(format!("{}", AtomicityClass::Likely), "likely");
    assert_eq!(META_VOLUME_ATOMICITY_COW, "cow-checksummed");
}
