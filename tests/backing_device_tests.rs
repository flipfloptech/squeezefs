//! Contract tests for `storage::validate_backing_device`.
//!
//! Policy: a backing device is any existing **regular file** or **block
//! device** (raw NVMe namespace, partition, dm/LVM volume, loop device).
//! Nothing else. No LVM/VG/NQN interrogation, no path allow-lists.

use squeezefs::storage::validate_backing_device;
use squeezefs_testkit::skip;

/// Find the first raw NVMe namespace block node (e.g. /dev/nvme0n1), if any.
fn first_nvme_namespace() -> Option<String> {
    let entries = std::fs::read_dir("/dev").ok()?;
    let mut candidates: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            // nvme<ctrl>n<ns> exactly (partition suffixes also fine)
            if name.starts_with("nvme") && name.contains('n') && name != "nvme-fabrics" {
                let ft = e.file_type().ok()?;
                if std::os::unix::fs::FileTypeExt::is_block_device(&ft) {
                    return Some(format!("/dev/{name}"));
                }
            }
            None
        })
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

/// Sysfs-reported capacity (in 512-byte sectors) of a block device node name.
fn block_device_sectors(name: &str) -> Option<u64> {
    std::fs::read_to_string(format!("/sys/class/block/{name}/size"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Any block device with nonzero capacity (sd*, vd*, dm-*, loop*, nvme*),
/// for boxes without NVMe.
fn first_nonzero_block_device() -> Option<String> {
    let entries = std::fs::read_dir("/dev").ok()?;
    let mut candidates: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let ft = e.file_type().ok()?;
            let name = e.file_name().to_string_lossy().into_owned();
            if std::os::unix::fs::FileTypeExt::is_block_device(&ft)
                && block_device_sectors(&name).unwrap_or(0) > 0
            {
                Some(format!("/dev/{name}"))
            } else {
                None
            }
        })
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

/// A block device whose sysfs capacity is zero (e.g. an unattached loop node).
fn first_zero_capacity_block_device() -> Option<String> {
    let entries = std::fs::read_dir("/dev").ok()?;
    let mut candidates: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let ft = e.file_type().ok()?;
            let name = e.file_name().to_string_lossy().into_owned();
            if std::os::unix::fs::FileTypeExt::is_block_device(&ft)
                && block_device_sectors(&name) == Some(0)
            {
                Some(format!("/dev/{name}"))
            } else {
                None
            }
        })
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

#[test]
fn raw_nvme_namespace_block_device_is_accepted() {
    let Some(dev) = first_nvme_namespace() else {
        skip!(Hardware, "no NVMe namespace block device on this machine");
    };
    validate_backing_device(&dev)
        .unwrap_or_else(|e| panic!("raw NVMe namespace '{dev}' must be accepted, got: {e}"));
}

#[test]
fn any_block_device_is_accepted() {
    let Some(dev) = first_nonzero_block_device() else {
        skip!(Hardware, "no block devices visible on this machine");
    };
    validate_backing_device(&dev)
        .unwrap_or_else(|e| panic!("block device '{dev}' must be accepted, got: {e}"));
}

#[test]
fn zero_capacity_block_device_is_rejected() {
    // e.g. an unattached /dev/loopN node: a block device, but 0 sectors.
    let Some(dev) = first_zero_capacity_block_device() else {
        skip!(Hardware, "no zero-capacity block device on this machine");
    };
    let err = validate_backing_device(&dev)
        .expect_err("zero-capacity block device must be rejected")
        .to_string();
    assert!(
        err.contains("zero"),
        "error must call out the zero capacity, got: {err}"
    );
}

#[test]
fn regular_file_is_accepted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("backing.img");
    std::fs::write(&path, b"x").expect("create file");
    let path = path.to_string_lossy().into_owned();
    validate_backing_device(&path)
        .unwrap_or_else(|e| panic!("regular file '{path}' must be accepted, got: {e}"));
}

#[test]
fn empty_regular_file_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("empty.img");
    std::fs::File::create(&path).expect("create empty file");
    let path = path.to_string_lossy().into_owned();
    let err = validate_backing_device(&path)
        .expect_err("zero-length regular file must be rejected")
        .to_string();
    assert!(
        err.contains("zero"),
        "error must call out the zero size, got: {err}"
    );
}

#[test]
fn symlink_to_regular_file_is_accepted() {
    // Canonicalization must see through symlinks (e.g. /dev/disk/by-id/*).
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("backing.img");
    std::fs::write(&target, b"x").expect("create file");
    let link = dir.path().join("backing-link");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let link = link.to_string_lossy().into_owned();
    validate_backing_device(&link)
        .unwrap_or_else(|e| panic!("symlink to regular file must be accepted, got: {e}"));
}

#[test]
fn nvme_controller_char_device_is_rejected_with_namespace_hint() {
    // /dev/nvme0 (controller) is a char device — passing it instead of the
    // namespace block node must fail loudly and point at the fix.
    let ctrl = "/dev/nvme0";
    match std::fs::metadata(ctrl) {
        Ok(m) if std::os::unix::fs::FileTypeExt::is_char_device(&m.file_type()) => {}
        _ => {
            skip!(
                Hardware,
                "no /dev/nvme0 controller char device on this machine"
            );
        }
    }
    let err = validate_backing_device(ctrl)
        .expect_err("controller char device must be rejected")
        .to_string();
    assert!(
        err.contains("character device") && err.contains("nvme0n1"),
        "error must explain char-device rejection and hint at the namespace node, got: {err}"
    );
}

#[test]
fn char_device_is_rejected() {
    let err = validate_backing_device("/dev/null")
        .expect_err("/dev/null (char device) must be rejected")
        .to_string();
    assert!(
        err.contains("character device"),
        "error must name the node type, got: {err}"
    );
}

#[test]
fn directory_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_string_lossy().into_owned();
    let err = validate_backing_device(&path)
        .expect_err("directory must be rejected as a backing device")
        .to_string();
    assert!(
        err.contains(&path),
        "error must include the offending path, got: {err}"
    );
}

#[test]
fn fifo_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pipe");
    let status = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .expect("run mkfifo");
    assert!(status.success(), "mkfifo failed");
    let path = path.to_string_lossy().into_owned();
    validate_backing_device(&path).expect_err("FIFO must be rejected as a backing device");
}

#[test]
fn nonexistent_path_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir
        .path()
        .join("missing.img")
        .to_string_lossy()
        .into_owned();
    let err = validate_backing_device(&path)
        .expect_err("nonexistent path must be rejected")
        .to_string();
    assert!(
        err.contains("does not exist"),
        "error must say the path is missing, got: {err}"
    );
}
