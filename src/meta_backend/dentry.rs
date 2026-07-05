use crate::error::{Result, SqueezefsError};
use crate::meta_backend::storage::{MetaLvStorage, SECTOR_SIZE};
use zerocopy::{FromBytes, Immutable, IntoBytes};

pub const DENTRY_TABLE_START: u64 = 1024 * 1024 * 8; // 8 MiB boundary
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
fn dentry_hash(parent_ino: u64, name: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::Hash;
    use std::hash::Hasher;
    parent_ino.hash(&mut hasher);
    name.hash(&mut hasher);
    hasher.finish()
}

/// Reads a dentry from a specific disk offset without locking.
pub async fn read_dentry_raw(storage: &MetaLvStorage, offset: u64) -> Result<DiskDentry> {
    let sector_offset = (offset / SECTOR_SIZE as u64) * SECTOR_SIZE as u64;
    let slot_in_sector = ((offset % SECTOR_SIZE as u64) / DENTRY_SLOT_SIZE as u64) as usize;

    let mut sector_buf = [0u8; SECTOR_SIZE];
    storage.read_blocks(sector_offset, &mut sector_buf).await?;

    let mut dentry = DiskDentry::new_zeroed();
    let slot_bytes =
        &sector_buf[slot_in_sector * DENTRY_SLOT_SIZE..(slot_in_sector + 1) * DENTRY_SLOT_SIZE];
    dentry.as_mut_bytes().copy_from_slice(slot_bytes);

    Ok(dentry)
}

/// Reads a dentry from a specific disk offset.
pub async fn read_dentry(storage: &MetaLvStorage, offset: u64) -> Result<DiskDentry> {
    let _guard = storage.dentry_lock.lock().await;
    read_dentry_raw(storage, offset).await
}

/// Writes a dentry to a specific disk offset without locking (internal use only).
pub async fn write_dentry_raw(
    storage: &MetaLvStorage,
    offset: u64,
    dentry: &DiskDentry,
) -> Result<()> {
    let sector_offset = (offset / SECTOR_SIZE as u64) * SECTOR_SIZE as u64;
    let slot_in_sector = ((offset % SECTOR_SIZE as u64) / DENTRY_SLOT_SIZE as u64) as usize;

    let mut sector_buf = [0u8; SECTOR_SIZE];
    storage.read_blocks(sector_offset, &mut sector_buf).await?;

    let slot_bytes = dentry.as_bytes();
    sector_buf[slot_in_sector * DENTRY_SLOT_SIZE..(slot_in_sector + 1) * DENTRY_SLOT_SIZE]
        .copy_from_slice(slot_bytes);

    storage.write_blocks(sector_offset, &sector_buf).await?;
    Ok(())
}

/// Writes a dentry to a specific disk offset.
pub async fn write_dentry(storage: &MetaLvStorage, offset: u64, dentry: &DiskDentry) -> Result<()> {
    let _guard = storage.dentry_lock.lock().await;
    write_dentry_raw(storage, offset, dentry).await
}

/// Lookup a dentry inside a parent directory using hash chains.
pub async fn find_dentry(
    storage: &MetaLvStorage,
    parent_ino: u64,
    name: &str,
) -> Result<Option<DiskDentry>> {
    let _guard = storage.dentry_lock.lock().await;
    let bucket = dentry_hash(parent_ino, name) % MAX_DENTRY_SLOTS;
    let mut offset = DENTRY_TABLE_START + bucket * DENTRY_SLOT_SIZE as u64;

    loop {
        let d = read_dentry_raw(storage, offset).await?;
        if d.parent_ino == parent_ino && d.get_name() == name {
            return Ok(Some(d));
        }
        if d.next_ptr == 0 {
            break;
        }
        offset = d.next_ptr;
    }
    Ok(None)
}

/// Inserts a new dentry inside a parent directory.
pub async fn insert_dentry(
    storage: &MetaLvStorage,
    parent_ino: u64,
    child_ino: u64,
    name: &str,
    file_type: u32,
) -> Result<()> {
    let _guard = storage.dentry_lock.lock().await;
    let bucket = dentry_hash(parent_ino, name) % MAX_DENTRY_SLOTS;
    let bucket_offset = DENTRY_TABLE_START + bucket * DENTRY_SLOT_SIZE as u64;

    let head = read_dentry_raw(storage, bucket_offset).await?;
    if head.parent_ino == 0 {
        // Bucket head is free, insert here!
        let new_dentry = DiskDentry::new(parent_ino, child_ino, name, file_type);
        write_dentry_raw(storage, bucket_offset, &new_dentry).await?;
        return Ok(());
    }

    // Bucket head is occupied. Find a free slot using sector-batched scanning.
    let mut free_offset = 0;
    let batch_sectors = 64;
    let batch_size = batch_sectors * SECTOR_SIZE;
    let mut buf = vec![0u8; batch_size];

    let total_slots = MAX_DENTRY_SLOTS;
    let slots_per_batch = batch_size / DENTRY_SLOT_SIZE;

    'outer: for batch_idx in 0..(total_slots as usize / slots_per_batch) {
        let batch_start_offset = DENTRY_TABLE_START + (batch_idx * batch_size) as u64;
        storage.read_blocks(batch_start_offset, &mut buf).await?;

        for slot_idx in 0..slots_per_batch {
            let slot_offset = batch_start_offset + (slot_idx * DENTRY_SLOT_SIZE) as u64;
            let offset_in_buf = slot_idx * DENTRY_SLOT_SIZE;
            let parent_ino_in_slot =
                u64::from_le_bytes(buf[offset_in_buf..offset_in_buf + 8].try_into().unwrap());
            if parent_ino_in_slot == 0 {
                free_offset = slot_offset;
                break 'outer;
            }
        }
    }

    if free_offset == 0 {
        return Err(SqueezefsError::InvalidOperation(
            "Dentry table full".to_string(),
        ));
    }

    // Write the new dentry to the free slot
    let mut new_dentry = DiskDentry::new(parent_ino, child_ino, name, file_type);
    new_dentry.next_ptr = 0;
    write_dentry_raw(storage, free_offset, &new_dentry).await?;

    // Now link it to the end of the chain starting at bucket_offset.
    let mut curr_offset = bucket_offset;
    loop {
        let mut d = read_dentry_raw(storage, curr_offset).await?;
        if d.next_ptr == 0 {
            d.next_ptr = free_offset;
            write_dentry_raw(storage, curr_offset, &d).await?;
            break;
        }
        curr_offset = d.next_ptr;
    }

    Ok(())
}

/// Removes a dentry from a parent directory.
pub async fn remove_dentry(storage: &MetaLvStorage, parent_ino: u64, name: &str) -> Result<()> {
    let _guard = storage.dentry_lock.lock().await;
    let bucket = dentry_hash(parent_ino, name) % MAX_DENTRY_SLOTS;
    let bucket_offset = DENTRY_TABLE_START + bucket * DENTRY_SLOT_SIZE as u64;

    let mut prev_offset = 0u64;
    let mut curr_offset = bucket_offset;

    loop {
        let d = read_dentry_raw(storage, curr_offset).await?;
        if d.parent_ino == parent_ino && d.get_name() == name {
            // Found the dentry!
            if prev_offset == 0 {
                // It is the head of the chain.
                if d.next_ptr == 0 {
                    // It was the only dentry in the chain. Just clear it.
                    let mut empty = DiskDentry::new_zeroed();
                    empty.parent_ino = 0;
                    write_dentry_raw(storage, curr_offset, &empty).await?;
                } else {
                    // There are other dentries. Copy the next dentry into the head slot, and mark the next slot as free!
                    let next_offset = d.next_ptr;
                    let next_dentry = read_dentry_raw(storage, next_offset).await?;
                    write_dentry_raw(storage, curr_offset, &next_dentry).await?;

                    // Clear the next slot
                    let mut empty = DiskDentry::new_zeroed();
                    empty.parent_ino = 0;
                    write_dentry_raw(storage, next_offset, &empty).await?;
                }
            } else {
                // It is a middle/tail dentry in the chain.
                // Update predecessor's next_ptr.
                let mut prev_dentry = read_dentry_raw(storage, prev_offset).await?;
                prev_dentry.next_ptr = d.next_ptr;
                write_dentry_raw(storage, prev_offset, &prev_dentry).await?;

                // Clear the current slot.
                let mut empty = DiskDentry::new_zeroed();
                empty.parent_ino = 0;
                write_dentry_raw(storage, curr_offset, &empty).await?;
            }
            return Ok(());
        }

        if d.next_ptr == 0 {
            break;
        }
        prev_offset = curr_offset;
        curr_offset = d.next_ptr;
    }

    Err(SqueezefsError::Io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "Dentry not found",
    )))
}

/// List all dentries in a parent directory.
pub async fn list_dentries(storage: &MetaLvStorage, parent_ino: u64) -> Result<Vec<DiskDentry>> {
    let _guard = storage.dentry_lock.lock().await;
    let mut list = Vec::new();

    // We scan the dentry table in sectors/batches (sector-batched scanning).
    let batch_sectors = 64;
    let batch_size = batch_sectors * SECTOR_SIZE;
    let mut buf = vec![0u8; batch_size];

    let total_slots = MAX_DENTRY_SLOTS;
    let slots_per_batch = batch_size / DENTRY_SLOT_SIZE;

    for batch_idx in 0..(total_slots as usize / slots_per_batch) {
        let batch_start_offset = DENTRY_TABLE_START + (batch_idx * batch_size) as u64;
        storage.read_blocks(batch_start_offset, &mut buf).await?;

        for slot_idx in 0..slots_per_batch {
            let offset_in_buf = slot_idx * DENTRY_SLOT_SIZE;
            let p_ino =
                u64::from_le_bytes(buf[offset_in_buf..offset_in_buf + 8].try_into().unwrap());
            if p_ino == parent_ino {
                let mut d = DiskDentry::new_zeroed();
                d.as_mut_bytes()
                    .copy_from_slice(&buf[offset_in_buf..offset_in_buf + DENTRY_SLOT_SIZE]);
                list.push(d);
            }
        }
    }

    Ok(list)
}
