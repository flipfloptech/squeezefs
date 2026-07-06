use crate::error::{Result, SqueezefsError};
use crate::meta_backend::storage::MetaLvStorage;
use zerocopy::{FromBytes, Immutable, IntoBytes};

pub const XATTR_BLOCK_START: u64 = 1024 * 1024 * 72; // 72MB offset
pub const XATTR_BLOCK_SIZE: usize = 32768; // 32 KB
pub const ENTRY_SIZE: usize = 8260;

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
                let _guard = storage.xattr_lock.lock().await;
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

    let _guard = storage.xattr_lock.lock().await;
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

                let _guard = storage.xattr_lock.lock().await;
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

    let _guard = storage.xattr_lock.lock().await;
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
    let _guard = storage.xattr_lock.lock().await;
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
    let _guard = storage.xattr_lock.lock().await;
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
