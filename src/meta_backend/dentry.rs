use crate::error::{Result, SqueezefsError};
use crate::meta_backend::storage::{MetaLvStorage, SECTOR_SIZE};
use zerocopy::{FromBytes, Immutable, IntoBytes};

pub const DENTRY_TABLE_START: u64 = 1024 * 1024; // 1 MiB boundary
pub const DENTRY_SLOT_SIZE: usize = 512;
pub const DENTRIES_PER_SECTOR: usize = SECTOR_SIZE / DENTRY_SLOT_SIZE;
pub const MAX_DENTRY_SLOTS: u64 = 16384;

#[derive(IntoBytes, FromBytes, Immutable, Debug, Clone, Copy)]
#[repr(C)]
pub struct DiskDentry {
    pub parent_ino: u64,
    pub child_ino: u64,
    pub file_type: u32,
    pub name_len: u32,
    pub name: [u8; 256],   // Maximum name length is 256 bytes (NAME_MAX = 255)
    pub next_ptr: u64,     // Offset to the next dentry in the hash chain
    pub unused: [u8; 224], // Pad to exactly 512 bytes
}

impl DiskDentry {
    pub fn new_zeroed() -> Self {
        unsafe { std::mem::zeroed() }
    }

    pub fn new(parent_ino: u64, child_ino: u64, name: &str, file_type: u32) -> Self {
        let mut d = Self {
            parent_ino,
            child_ino,
            file_type,
            name_len: name.len() as u32,
            name: [0u8; 256],
            next_ptr: 0,
            unused: [0u8; 224],
        };
        let bytes = name.as_bytes();
        let copy_len = bytes.len().min(256);
        d.name[..copy_len].copy_from_slice(&bytes[..copy_len]);
        d
    }

    pub fn get_name(&self) -> String {
        let len = self.name_len as usize;
        let bound = len.min(256);
        String::from_utf8_lossy(&self.name[..bound]).into_owned()
    }
}

/// Reads a dentry from a specific disk offset.
pub fn read_dentry(storage: &MetaLvStorage, offset: u64) -> Result<DiskDentry> {
    let _guard = storage.lock_op();
    let sector_offset = (offset / SECTOR_SIZE as u64) * SECTOR_SIZE as u64;
    let slot_in_sector = ((offset % SECTOR_SIZE as u64) / DENTRY_SLOT_SIZE as u64) as usize;

    let mut sector_buf = [0u8; SECTOR_SIZE];
    storage.read_blocks(sector_offset, &mut sector_buf)?;

    let mut dentry = DiskDentry::new_zeroed();
    let slot_bytes =
        &sector_buf[slot_in_sector * DENTRY_SLOT_SIZE..(slot_in_sector + 1) * DENTRY_SLOT_SIZE];
    dentry.as_mut_bytes().copy_from_slice(slot_bytes);

    Ok(dentry)
}

/// Writes a dentry to a specific disk offset without locking (internal use only).
pub fn write_dentry_raw(storage: &MetaLvStorage, offset: u64, dentry: &DiskDentry) -> Result<()> {
    let sector_offset = (offset / SECTOR_SIZE as u64) * SECTOR_SIZE as u64;
    let slot_in_sector = ((offset % SECTOR_SIZE as u64) / DENTRY_SLOT_SIZE as u64) as usize;

    let mut sector_buf = [0u8; SECTOR_SIZE];
    storage.read_blocks(sector_offset, &mut sector_buf)?;

    let slot_bytes = dentry.as_bytes();
    sector_buf[slot_in_sector * DENTRY_SLOT_SIZE..(slot_in_sector + 1) * DENTRY_SLOT_SIZE]
        .copy_from_slice(slot_bytes);

    storage.write_blocks(sector_offset, &sector_buf)?;
    Ok(())
}

/// Writes a dentry to a specific disk offset.
pub fn write_dentry(storage: &MetaLvStorage, offset: u64, dentry: &DiskDentry) -> Result<()> {
    let _guard = storage.lock_op();
    write_dentry_raw(storage, offset, dentry)
}

/// Simple lookup of a dentry inside a parent directory.
pub fn find_dentry(
    storage: &MetaLvStorage,
    parent_ino: u64,
    name: &str,
) -> Result<Option<DiskDentry>> {
    // Scan all slots in the dentry table (basic linear scan for Phase 0)
    for i in 0..MAX_DENTRY_SLOTS {
        let offset = DENTRY_TABLE_START + i * DENTRY_SLOT_SIZE as u64;
        let d = read_dentry(storage, offset)?;
        if d.parent_ino == parent_ino && d.get_name() == name {
            return Ok(Some(d));
        }
    }
    Ok(None)
}

/// Inserts a new dentry inside a parent directory.
pub fn insert_dentry(
    storage: &MetaLvStorage,
    parent_ino: u64,
    child_ino: u64,
    name: &str,
    file_type: u32,
) -> Result<()> {
    let _guard = storage.lock_op();
    // Find an empty slot (parent_ino == 0)
    for i in 0..MAX_DENTRY_SLOTS {
        let offset = DENTRY_TABLE_START + i * DENTRY_SLOT_SIZE as u64;
        let d = read_dentry(storage, offset)?;
        if d.parent_ino == 0 {
            let new_dentry = DiskDentry::new(parent_ino, child_ino, name, file_type);
            write_dentry_raw(storage, offset, &new_dentry)?;
            return Ok(());
        }
    }
    Err(SqueezefsError::InvalidOperation(
        "Dentry table full".to_string(),
    ))
}

/// Removes a dentry from a parent directory.
pub fn remove_dentry(storage: &MetaLvStorage, parent_ino: u64, name: &str) -> Result<()> {
    let _guard = storage.lock_op();
    for i in 0..MAX_DENTRY_SLOTS {
        let offset = DENTRY_TABLE_START + i * DENTRY_SLOT_SIZE as u64;
        let d = read_dentry(storage, offset)?;
        if d.parent_ino == parent_ino && d.get_name() == name {
            let mut empty = DiskDentry::new_zeroed();
            empty.parent_ino = 0; // Clear it
            write_dentry_raw(storage, offset, &empty)?;
            return Ok(());
        }
    }
    Err(SqueezefsError::InvalidOperation(
        "Dentry not found".to_string(),
    ))
}

/// List all dentries in a parent directory.
pub fn list_dentries(storage: &MetaLvStorage, parent_ino: u64) -> Result<Vec<DiskDentry>> {
    let mut list = Vec::new();
    for i in 0..MAX_DENTRY_SLOTS {
        let offset = DENTRY_TABLE_START + i * DENTRY_SLOT_SIZE as u64;
        let d = read_dentry(storage, offset)?;
        if d.parent_ino == parent_ino {
            list.push(d);
        }
    }
    Ok(list)
}
