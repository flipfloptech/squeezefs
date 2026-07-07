use crate::error::{Result, SqueezefsError};
use crate::meta_backend::storage::{MetaLvStorage, ACTIVE_TX};
use zerocopy::{FromBytes, Immutable, IntoBytes};

pub const XATTR_BLOCK_START: u64 = 1024 * 1024 * 72; // 72MB offset
pub const XATTR_BLOCK_SIZE: usize = 32768; // 32 KB
pub const ENTRY_SIZE: usize = 8260;

/// Quarantined ino range `[START, END)` whose 32 KiB xattr blocks physically
/// overlap the on-disk journal region (design-wal-crash-consistency §4.4,
/// Key Decision 5): journal writes corrupt their xattr blocks and their
/// xattr writes corrupt the journal. The range is derived from the geometry
/// (not hardcoded) and compile-time pinned below; it is reserved in the
/// in-RAM allocator, marked in the offset-4096 bitmap at format and every
/// table-derived refresh, and legacy occupants degrade safely (EIO for
/// mutations and symlink-target reads, empty for regular xattr reads).
pub const QUARANTINE_INO_START: u64 = (crate::meta_backend::storage::JOURNAL_REGION_START
    - XATTR_BLOCK_START)
    / XATTR_BLOCK_SIZE as u64;
pub const QUARANTINE_INO_END: u64 = (crate::meta_backend::storage::JOURNAL_REGION_START
    + crate::meta_backend::storage::JOURNAL_REGION_SIZE
    - XATTR_BLOCK_START)
    .div_ceil(XATTR_BLOCK_SIZE as u64);

// Pin the verified overlap (§2.6): (104−72) MiB / 32 KiB and (108−72) MiB /
// 32 KiB. If either constant moves, the on-disk quarantine story (bitmap
// marks, legacy-victim handling) must be re-derived — fail the build.
const _: () = assert!(
    QUARANTINE_INO_START == 1024 && QUARANTINE_INO_END == 1152,
    "xattr/journal overlap geometry changed — re-derive the quarantine design (§4.4)"
);

/// Whether `ino`'s xattr block overlaps the journal region.
#[inline]
pub fn is_quarantined(ino: u64) -> bool {
    (QUARANTINE_INO_START..QUARANTINE_INO_END).contains(&ino)
}

/// Clean `EIO` for xattr access whose backing block is quarantined.
fn quarantine_eio(ino: u64, op: &str) -> SqueezefsError {
    log::warn!(
        "xattr {op} on quarantined ino {ino}: its xattr block overlaps the \
         journal region [104 MiB, 108 MiB) and is presumed corrupt (EIO)"
    );
    SqueezefsError::Io(std::io::Error::other(format!(
        "xattr block of ino {ino} is quarantined (overlaps the metadata journal region)"
    )))
}

#[derive(IntoBytes, FromBytes, Immutable, Debug, Clone, Copy)]
#[repr(C)]
pub struct DiskXattrEntry {
    pub val_len: u16,         // 2 bytes, offset 0
    pub key_len: u8,          // 1 byte, offset 2
    pub unused: u8,           // 1 byte, offset 3
    pub key: [[u8; 32]; 2],   // 64 bytes, offset 4
    pub val: [[u8; 32]; 256], // 8192 bytes, offset 68
}

impl DiskXattrEntry {
    pub fn new_zeroed() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[derive(IntoBytes, FromBytes, Immutable, Debug, Clone, Copy)]
#[repr(C)]
pub struct DiskXattrBlock {
    pub magic: u32,
    pub num_entries: u32,
    pub data: [u8; 32760], // 32768 - 8 bytes
}

impl DiskXattrBlock {
    pub fn new_zeroed() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

fn cast_block_mut(buf: &mut [u8]) -> &mut DiskXattrBlock {
    unsafe { &mut *(buf.as_mut_ptr() as *mut DiskXattrBlock) }
}

fn cast_block(buf: &[u8]) -> &DiskXattrBlock {
    unsafe { &*(buf.as_ptr() as *const DiskXattrBlock) }
}

fn get_xattr_block_offset(ino: u64) -> u64 {
    XATTR_BLOCK_START + ino * XATTR_BLOCK_SIZE as u64
}

/// Serialization guard for one inode's 32 KiB xattr block (PR 6).
///
/// Out of a transaction: the block's sector-lock shards (ascending-deduped, the
/// commit protocol's order — see `MetaLvStorage::lock_sectors_*`). Xattr blocks
/// are per-inode and 32 KiB-aligned, so no two inodes share a sector; every
/// staged xattr patch covers the same shards, so commits and direct ops exclude
/// each other. Inside a transaction no lock is taken: the DLM `I{ino}` lock
/// already serializes same-inode mutators, reads see this tx's staging overlay,
/// and the sector locks are acquired at commit (taking them here would risk
/// commit-time reentrancy on the non-reentrant tokio lock, design §3.4).
enum XattrGuard<'a> {
    // Fields are RAII lock guards held only for Drop (underscore-named: never read).
    SectorsRead {
        _g: Vec<tokio::sync::RwLockReadGuard<'a, ()>>,
    },
    SectorsWrite {
        _g: Vec<tokio::sync::RwLockWriteGuard<'a, ()>>,
    },
    InTx,
}

async fn xattr_guard(storage: &MetaLvStorage, ino: u64, write: bool) -> XattrGuard<'_> {
    if ACTIVE_TX.try_with(|_| ()).is_ok() {
        return XattrGuard::InTx;
    }
    let offset = get_xattr_block_offset(ino);
    if write {
        XattrGuard::SectorsWrite {
            _g: storage.lock_sectors_write(offset, XATTR_BLOCK_SIZE).await,
        }
    } else {
        XattrGuard::SectorsRead {
            _g: storage.lock_sectors_read(offset, XATTR_BLOCK_SIZE).await,
        }
    }
}

fn get_entry(block: &DiskXattrBlock, idx: usize) -> &DiskXattrEntry {
    let start = idx * ENTRY_SIZE;
    let end = (idx + 1) * ENTRY_SIZE;
    unsafe { &*(block.data[start..end].as_ptr() as *const DiskXattrEntry) }
}

fn get_entry_mut(block: &mut DiskXattrBlock, idx: usize) -> &mut DiskXattrEntry {
    let start = idx * ENTRY_SIZE;
    let end = (idx + 1) * ENTRY_SIZE;
    unsafe { &mut *(block.data[start..end].as_mut_ptr() as *mut DiskXattrEntry) }
}

fn get_flat_key(entry: &DiskXattrEntry) -> Vec<u8> {
    let mut flat = Vec::with_capacity(64);
    flat.extend_from_slice(&entry.key[0]);
    flat.extend_from_slice(&entry.key[1]);
    flat.truncate(entry.key_len as usize);
    flat
}

fn get_flat_val(entry: &DiskXattrEntry) -> Vec<u8> {
    let mut flat = Vec::with_capacity(8192);
    for chunk in &entry.val {
        flat.extend_from_slice(chunk);
    }
    flat.truncate(entry.val_len as usize);
    flat
}

pub async fn get_xattr(storage: &MetaLvStorage, ino: u64, name: &str) -> Result<Option<Vec<u8>>> {
    let inode = crate::meta_backend::inode::read_inode(storage, ino)
        .await
        .ok();
    if let Some(ref inode) = inode {
        if (inode.mode & libc::S_IFMT) == libc::S_IFLNK {
            if name == "system.symlink" {
                if is_quarantined(ino) {
                    // Symlink content is `inode.size` RAW bytes of the xattr
                    // block — no magic guard exists, so a quarantined block
                    // would be served as a readlink target. An error beats
                    // journal-record garbage (§4.4, review Issue 4).
                    return Err(quarantine_eio(ino, "symlink-target read"));
                }
                let _guard = xattr_guard(storage, ino, false).await;
                let offset = get_xattr_block_offset(ino);
                let mut sector_buf = vec![0u8; XATTR_BLOCK_SIZE].into_boxed_slice();
                storage.read_blocks(offset, &mut sector_buf).await?;
                let len = std::cmp::min(inode.size as usize, XATTR_BLOCK_SIZE);
                return Ok(Some(sector_buf[..len].to_vec()));
            } else {
                return Ok(None);
            }
        }
    }

    if is_quarantined(ino) {
        // Deterministic degrade-to-empty: the overlapped bytes may even LOOK
        // like a valid xattr block (journal wrap / prior-life resurrection) —
        // never parse them (§4.4).
        return Ok(None);
    }

    let _guard = xattr_guard(storage, ino, false).await;
    let offset = get_xattr_block_offset(ino);
    let mut block_buf = vec![0u8; XATTR_BLOCK_SIZE].into_boxed_slice();
    storage.read_blocks(offset, &mut block_buf).await?;

    let block = cast_block(&block_buf);

    if block.magic != 0x58415452 {
        return Ok(None);
    }

    for i in 0..block.num_entries as usize {
        if i >= 3 {
            break;
        }
        let entry = get_entry(block, i);
        let key_bytes = get_flat_key(entry);
        let key_str = std::str::from_utf8(&key_bytes).map_err(|e| {
            SqueezefsError::InvalidOperation(format!("Invalid UTF-8 in key: {:?}", e))
        })?;
        if key_str == name {
            return Ok(Some(get_flat_val(entry)));
        }
    }

    Ok(None)
}

pub async fn set_xattr(storage: &MetaLvStorage, ino: u64, name: &str, value: &[u8]) -> Result<()> {
    if is_quarantined(ino) {
        // Before ANY side effect (the symlink branch below writes the inode
        // size first): a quarantined block must never be written — that would
        // scribble the journal region (§2.6).
        return Err(quarantine_eio(ino, "setxattr"));
    }
    if name.len() > 64 {
        return Err(SqueezefsError::InvalidOperation(
            "xattr key too long (max 64 bytes)".to_string(),
        ));
    }
    if name == "system.symlink" {
        if value.len() > 4096 {
            return Err(SqueezefsError::InvalidOperation(
                "symlink target path too long (max 4096 bytes)".to_string(),
            ));
        }
    } else if value.len() > 8192 {
        return Err(SqueezefsError::InvalidOperation(
            "xattr value too long (max 8192 bytes)".to_string(),
        ));
    }

    let inode = crate::meta_backend::inode::read_inode(storage, ino)
        .await
        .ok();
    if let Some(mut inode) = inode {
        if (inode.mode & libc::S_IFMT) == libc::S_IFLNK {
            if name == "system.symlink" {
                inode.size = value.len() as u64;
                crate::meta_backend::inode::write_inode(storage, ino, &inode).await?;

                let _guard = xattr_guard(storage, ino, true).await;
                let offset = get_xattr_block_offset(ino);
                let mut sector_buf = vec![0u8; XATTR_BLOCK_SIZE].into_boxed_slice();
                sector_buf[..value.len()].copy_from_slice(value);
                storage.write_blocks(offset, &sector_buf).await?;
                return Ok(());
            } else {
                return Err(SqueezefsError::InvalidOperation(
                    "custom xattrs not supported on symlinks".to_string(),
                ));
            }
        }
    }

    let _guard = xattr_guard(storage, ino, true).await;
    let offset = get_xattr_block_offset(ino);
    let mut block_buf = vec![0u8; XATTR_BLOCK_SIZE].into_boxed_slice();
    storage.read_blocks(offset, &mut block_buf).await?;

    let block = cast_block_mut(&mut block_buf);

    if block.magic != 0x58415452 {
        block.magic = 0x58415452;
        block.num_entries = 0;
        unsafe { std::ptr::write_bytes(block.data.as_mut_ptr(), 0, block.data.len()) };
    }

    let mut updated = false;
    for i in 0..block.num_entries as usize {
        if i >= 3 {
            break;
        }
        let entry = get_entry_mut(block, i);
        let key_bytes = get_flat_key(entry);
        let key_str = std::str::from_utf8(&key_bytes).map_err(|e| {
            SqueezefsError::InvalidOperation(format!("Invalid UTF-8 in key: {:?}", e))
        })?;
        if key_str == name {
            entry.val_len = value.len() as u16;
            let mut padded = Box::new([0u8; 8192]);
            padded[..value.len()].copy_from_slice(value);
            for chunk_idx in 0..256 {
                entry.val[chunk_idx].copy_from_slice(&padded[chunk_idx * 32..(chunk_idx + 1) * 32]);
            }
            updated = true;
            break;
        }
    }

    if !updated {
        let idx = block.num_entries as usize;
        if idx >= 3 {
            return Err(SqueezefsError::InvalidOperation(
                "xattr block full (max 3 entries)".to_string(),
            ));
        }
        let entry = get_entry_mut(block, idx);
        unsafe { std::ptr::write_bytes(entry as *mut DiskXattrEntry, 0, 1) };
        entry.key_len = name.len() as u8;
        let mut padded_key = [0u8; 64];
        padded_key[..name.len()].copy_from_slice(name.as_bytes());
        entry.key[0].copy_from_slice(&padded_key[0..32]);
        entry.key[1].copy_from_slice(&padded_key[32..64]);

        entry.val_len = value.len() as u16;
        let mut padded_val = Box::new([0u8; 8192]);
        padded_val[..value.len()].copy_from_slice(value);
        for chunk_idx in 0..256 {
            entry.val[chunk_idx].copy_from_slice(&padded_val[chunk_idx * 32..(chunk_idx + 1) * 32]);
        }
        block.num_entries += 1;
    }

    storage.write_blocks(offset, &block_buf).await?;
    Ok(())
}

pub async fn remove_xattr(storage: &MetaLvStorage, ino: u64, name: &str) -> Result<()> {
    if is_quarantined(ino) {
        return Err(quarantine_eio(ino, "removexattr"));
    }
    let _guard = xattr_guard(storage, ino, true).await;
    let offset = get_xattr_block_offset(ino);
    let mut block_buf = vec![0u8; XATTR_BLOCK_SIZE].into_boxed_slice();
    storage.read_blocks(offset, &mut block_buf).await?;

    let block = cast_block_mut(&mut block_buf);

    if block.magic != 0x58415452 {
        return Err(SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No extended attributes found",
        )));
    }

    let mut found_idx = None;
    for i in 0..block.num_entries as usize {
        if i >= 3 {
            break;
        }
        let entry = get_entry(block, i);
        let key_bytes = get_flat_key(entry);
        let key_str = std::str::from_utf8(&key_bytes).map_err(|e| {
            SqueezefsError::InvalidOperation(format!("Invalid UTF-8 in key: {:?}", e))
        })?;
        if key_str == name {
            found_idx = Some(i);
            break;
        }
    }

    if let Some(idx) = found_idx {
        for i in idx..block.num_entries as usize - 1 {
            if i + 1 < 3 {
                let next_entry_bytes = *get_entry(block, i + 1);
                let entry_mut = get_entry_mut(block, i);
                *entry_mut = next_entry_bytes;
            }
        }
        block.num_entries -= 1;
        storage.write_blocks(offset, &block_buf).await?;
        Ok(())
    } else {
        Err(SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Attribute not found",
        )))
    }
}

pub async fn list_xattrs(storage: &MetaLvStorage, ino: u64) -> Result<Vec<String>> {
    if is_quarantined(ino) {
        // Same deterministic degrade-to-empty as get_xattr (§4.4).
        return Ok(Vec::new());
    }
    let _guard = xattr_guard(storage, ino, false).await;
    let offset = get_xattr_block_offset(ino);
    let mut block_buf = vec![0u8; XATTR_BLOCK_SIZE].into_boxed_slice();
    storage.read_blocks(offset, &mut block_buf).await?;

    let block = cast_block(&block_buf);

    if block.magic != 0x58415452 {
        return Ok(Vec::new());
    }

    let mut list = Vec::new();
    for i in 0..block.num_entries as usize {
        if i >= 3 {
            break;
        }
        let entry = get_entry(block, i);
        let key_bytes = get_flat_key(entry);
        let key_str = std::str::from_utf8(&key_bytes).map_err(|e| {
            SqueezefsError::InvalidOperation(format!("Invalid UTF-8 in key: {:?}", e))
        })?;
        list.push(key_str.to_string());
    }

    Ok(list)
}
