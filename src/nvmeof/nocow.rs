/*
 * JuiceFS, Copyright 2026 Juicedata, Inc.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! CoW-filesystem guard for file-backed NVMe-oF targets — the
//! battle-tested part of the pre-rebuild module (born from a live fabric
//! wedge, commit `eafab6c`), kept verbatim by the N1 module split
//! (`docs/design-nvmeof-target-management.md` §6.1: retained/derived
//! code keeps the existing Apache-2.0 header).

use std::fs;
use std::path::Path;

const BTRFS_SUPER_MAGIC: i64 = 0x9123_683E;
const FS_NOCOW_FL: libc::c_long = 0x0080_0000;
// ioctl codes for FS_IOC_GETFLAGS / FS_IOC_SETFLAGS (_IOR/_IOW('f', 1/2, long)).
const FS_IOC_GETFLAGS: libc::c_ulong = 0x8008_6601;
const FS_IOC_SETFLAGS: libc::c_ulong = 0x4008_6602;

/// Ensure a regular-file backing path is safe to serve as an NVMe-oF target.
///
/// On btrfs, `O_DIRECT` (SPDK's AIO bdev) **silently degrades to buffered
/// I/O on copy-on-write files**. The single-threaded `nvmf_tgt` reactor then
/// parks in the kernel dirty-page throttle (`balance_dirty_pages`) under
/// write load, stops polling the fabric sockets, keepalives blow past their
/// budget, controllers flap between `live`/`connecting`, and every consumer
/// of the target wedges in D-state — observed live as "format hangs".
///
/// Policy:
/// - non-btrfs filesystem: nothing to do.
/// - btrfs + flag already set, or file still EMPTY: set `FS_NOCOW_FL`
///   (the flag only takes effect on empty files).
/// - btrfs + data already written + no flag: **fail loud** with the exact
///   remediation — silently proceeding is how the fabric wedged.
pub fn ensure_nocow_backing(path: &Path) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    let meta = fs::metadata(path)?;
    if !meta.is_file() {
        return Ok(()); // block devices etc. — not our concern here
    }

    // Filesystem probe.
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut sfs) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if sfs.f_type as i64 != BTRFS_SUPER_MAGIC {
        return Ok(());
    }

    let f = fs::OpenOptions::new().read(true).open(path)?;
    let mut flags: libc::c_long = 0;
    if unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_GETFLAGS, &mut flags) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if flags & FS_NOCOW_FL != 0 {
        return Ok(());
    }
    if meta.len() > 0 && !file_is_all_hole(path, &meta) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "Backing file '{}' lives on btrfs WITHOUT the NoCOW attribute and already \
                 carries data. SPDK's O_DIRECT silently degrades to buffered I/O on CoW \
                 files, which wedges the whole fabric under write load. Recreate it: \
                 `rm {p} && touch {p} && chattr +C {p} && truncate -s <size> {p}` \
                 (or place it on a non-CoW filesystem).",
                path.display(),
                p = path.display()
            ),
        ));
    }
    // Empty (or hole-only) file: btrfs only honors +C at i_size == 0 (it
    // silently masks the bit otherwise), so shrink → flag → re-extend —
    // safe for hole-only files, which have no data blocks to lose — and
    // VERIFY the flag stuck.
    drop(f);
    let f = fs::OpenOptions::new().read(true).write(true).open(path)?;
    let original_len = meta.len();
    f.set_len(0)?;
    flags |= FS_NOCOW_FL;
    let set_res = unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_SETFLAGS, &flags) };
    let set_err = std::io::Error::last_os_error();
    f.set_len(original_len)?;
    if set_res != 0 {
        return Err(set_err);
    }
    let mut verify: libc::c_long = 0;
    if unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_GETFLAGS, &mut verify) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if verify & FS_NOCOW_FL == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!(
                "btrfs did not accept the NoCOW attribute on '{}' — recreate it with \
                 `touch`+`chattr +C` before sizing, or use a non-CoW filesystem",
                path.display()
            ),
        ));
    }
    log::info!(
        "Applied btrfs NoCOW (+C) to NVMe-oF backing file {}",
        path.display()
    );
    Ok(())
}

/// Whether a file allocates no data blocks (truncate-only sparse file):
/// btrfs accepts `FS_NOCOW_FL` on these exactly like on empty files.
fn file_is_all_hole(path: &Path, meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    let _ = path;
    meta.blocks() == 0
}

#[cfg(test)]
mod nocow_tests {
    use super::*;
    use std::io::Write;

    fn fs_is_btrfs(dir: &Path) -> bool {
        let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).unwrap();
        let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
        unsafe { libc::statfs(c.as_ptr(), &mut sfs) == 0 && sfs.f_type as i64 == BTRFS_SUPER_MAGIC }
    }

    fn get_flags(path: &Path) -> libc::c_long {
        use std::os::unix::io::AsRawFd;
        let f = fs::File::open(path).unwrap();
        let mut flags: libc::c_long = 0;
        unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_GETFLAGS, &mut flags) };
        flags
    }

    /// Fresh (empty or truncate-sparse) backing files on btrfs get +C; the
    /// repo checkout typically lives on btrfs — skip gracefully elsewhere.
    #[test]
    fn test_nocow_applied_to_fresh_backing_file() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
        if !fs_is_btrfs(&dir) {
            eprintln!("skip: target/ not on btrfs");
            return;
        }
        let path = dir.join("nocow_fresh_test.nvme");
        let _ = fs::remove_file(&path);
        let f = fs::File::create(&path).unwrap();
        f.set_len(4 * 1024 * 1024).unwrap(); // sparse: no data blocks
        drop(f);

        ensure_nocow_backing(&path).expect("fresh sparse file must accept +C");
        assert_ne!(
            get_flags(&path) & FS_NOCOW_FL,
            0,
            "NoCOW flag must be set on the fresh backing file"
        );
        let _ = fs::remove_file(&path);
    }

    /// Data-carrying CoW files must FAIL LOUD with the remediation — never
    /// silently serve a fabric-wedging backing file.
    #[test]
    fn test_nocow_rejects_data_carrying_cow_file() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
        if !fs_is_btrfs(&dir) {
            eprintln!("skip: target/ not on btrfs");
            return;
        }
        let path = dir.join("nocow_dirty_test.nvme");
        let _ = fs::remove_file(&path);
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(&[0xAA; 8192]).unwrap();
        f.sync_all().unwrap();
        drop(f);

        let err = ensure_nocow_backing(&path)
            .expect_err("data-carrying CoW backing file must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("NoCOW"),
            "error must explain the CoW hazard: {msg}"
        );
        assert!(
            msg.contains("chattr +C"),
            "error must carry the remediation: {msg}"
        );
        let _ = fs::remove_file(&path);
    }

    /// Non-btrfs filesystems (tmpfs /tmp) are a no-op.
    #[test]
    fn test_nocow_noop_on_non_btrfs() {
        let path = std::path::PathBuf::from("/tmp/nocow_noop_test.nvme");
        let _ = fs::remove_file(&path);
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(&[0xBB; 4096]).unwrap();
        drop(f);
        if fs_is_btrfs(std::path::Path::new("/tmp")) {
            eprintln!("skip: /tmp unexpectedly btrfs");
            let _ = fs::remove_file(&path);
            return;
        }
        ensure_nocow_backing(&path).expect("non-CoW filesystems need no flag");
        let _ = fs::remove_file(&path);
    }
}
