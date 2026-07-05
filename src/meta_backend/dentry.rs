use crate::error::{Result, SqueezefsError};
use crate::meta_backend::storage::{MetaLvStorage, SECTOR_SIZE};
use zerocopy::{FromBytes, Immutable, IntoBytes};

pub const DENTRY_TABLE_START: u64 = 1024 * 1024 * 8; // 8 MiB boundary
pub const DENTRY_SLOT_SIZE: usize = 512;
pub const DENTRIES_PER_SECTOR: usize = SECTOR_SIZE / DENTRY_SLOT_SIZE;
pub const MAX_HASH_BUCKETS: u64 = 32768;
pub const MAX_DENTRY_SLOTS: u64 = 131072;

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
    storage.ensure_dentry_index().await?;
    let _guard = storage.dentry_lock.lock().await;

    let mut found = None;
    let _ = storage.dentry_index.read_sync(&parent_ino, |_, entries| {
        for (_, d) in entries {
            if d.get_name() == name {
                found = Some(*d);
                break;
            }
        }
    });
    Ok(found)
}

/// Inserts a new dentry inside a parent directory.
pub async fn insert_dentry(
    storage: &MetaLvStorage,
    parent_ino: u64,
    child_ino: u64,
    name: &str,
    file_type: u32,
) -> Result<()> {
    storage.ensure_dentry_index().await?;
    let _guard = storage.dentry_lock.lock().await;
    let bucket = dentry_hash(parent_ino, name) % MAX_HASH_BUCKETS;
    let bucket_offset = DENTRY_TABLE_START + bucket * DENTRY_SLOT_SIZE as u64;

    let head_occupied = {
        let occupied = storage.dentry_occupied_offsets.lock().unwrap();
        occupied.contains(&bucket_offset)
    };

    let new_dentry = DiskDentry::new(parent_ino, child_ino, name, file_type);

    if !head_occupied {
        // Bucket head is free, insert here!
        write_dentry_raw(storage, bucket_offset, &new_dentry).await?;

        // Update in-memory index
        storage
            .dentry_occupied_offsets
            .lock()
            .unwrap()
            .insert(bucket_offset);
        storage
            .dentry_index
            .entry_sync(parent_ino)
            .or_default()
            .get_mut()
            .push((bucket_offset, new_dentry));
        let _ = storage
            .dentry_by_offset
            .insert_sync(bucket_offset, (parent_ino, new_dentry));
        return Ok(());
    }

    // Bucket head is occupied. Find a free slot in overflow region.
    let mut free_offset = 0;
    {
        let mut occupied = storage.dentry_occupied_offsets.lock().unwrap();
        for i in MAX_HASH_BUCKETS..MAX_DENTRY_SLOTS {
            let offset = DENTRY_TABLE_START + i * DENTRY_SLOT_SIZE as u64;
            if !occupied.contains(&offset) {
                free_offset = offset;
                occupied.insert(offset);
                break;
            }
        }
    }

    if free_offset == 0 {
        return Err(SqueezefsError::InvalidOperation(
            "Dentry table full".to_string(),
        ));
    }

    // Write the new dentry to the free slot
    let mut new_dentry_with_link = new_dentry;
    new_dentry_with_link.next_ptr = 0;
    write_dentry_raw(storage, free_offset, &new_dentry_with_link).await?;

    // Link it to the end of the chain starting at bucket_offset.
    let mut curr_offset = bucket_offset;
    loop {
        let mut d = DiskDentry::new_zeroed();
        let mut p_ino = 0;
        let found = storage
            .dentry_by_offset
            .read_sync(&curr_offset, |_k_p_ino, val| {
                p_ino = val.0;
                d = val.1;
            });
        if found.is_none() {
            break;
        }
        if d.next_ptr == 0 {
            d.next_ptr = free_offset;
            write_dentry_raw(storage, curr_offset, &d).await?;

            // Update in-memory index
            match storage.dentry_by_offset.entry_sync(curr_offset) {
                scc::hash_map::Entry::Occupied(mut occ) => {
                    occ.get_mut().1.next_ptr = free_offset;
                }
                _ => {}
            }
            match storage.dentry_index.entry_sync(p_ino) {
                scc::hash_map::Entry::Occupied(mut occ) => {
                    if let Some(tuple) = occ
                        .get_mut()
                        .iter_mut()
                        .find(|(off, _)| *off == curr_offset)
                    {
                        tuple.1.next_ptr = free_offset;
                    }
                }
                _ => {}
            }
            break;
        }
        curr_offset = d.next_ptr;
    }

    // Insert new dentry to in-memory maps
    storage
        .dentry_index
        .entry_sync(parent_ino)
        .or_default()
        .get_mut()
        .push((free_offset, new_dentry_with_link));
    let _ = storage
        .dentry_by_offset
        .insert_sync(free_offset, (parent_ino, new_dentry_with_link));

    Ok(())
}

/// Removes a dentry from a parent directory.
pub async fn remove_dentry(storage: &MetaLvStorage, parent_ino: u64, name: &str) -> Result<()> {
    storage.ensure_dentry_index().await?;
    let _guard = storage.dentry_lock.lock().await;
    let bucket = dentry_hash(parent_ino, name) % MAX_HASH_BUCKETS;
    let bucket_offset = DENTRY_TABLE_START + bucket * DENTRY_SLOT_SIZE as u64;

    let mut prev_offset = 0u64;
    let mut curr_offset = bucket_offset;

    loop {
        // Look up current dentry in memory
        let mut d = DiskDentry::new_zeroed();
        let found = storage.dentry_by_offset.read_sync(&curr_offset, |_, val| {
            d = val.1;
        });
        if found.is_none() {
            break;
        }

        if d.parent_ino == parent_ino && d.get_name() == name {
            // Found the dentry!
            if prev_offset == 0 {
                // It is the head of the chain.
                if d.next_ptr == 0 {
                    // It was the only dentry in the chain. Just clear it.
                    let mut empty = DiskDentry::new_zeroed();
                    empty.parent_ino = 0;
                    write_dentry_raw(storage, curr_offset, &empty).await?;

                    // Update in-memory maps
                    storage.dentry_by_offset.remove_sync(&curr_offset);
                    storage
                        .dentry_occupied_offsets
                        .lock()
                        .unwrap()
                        .remove(&curr_offset);
                    match storage.dentry_index.entry_sync(parent_ino) {
                        scc::hash_map::Entry::Occupied(mut occ) => {
                            occ.get_mut().retain(|(off, _)| *off != curr_offset);
                        }
                        _ => {}
                    }
                } else {
                    // There are other dentries. Copy the next dentry into the head slot, and mark the next slot as free!
                    let next_offset = d.next_ptr;
                    let mut next_dentry = DiskDentry::new_zeroed();
                    let next_found = storage.dentry_by_offset.read_sync(&next_offset, |_, val| {
                        next_dentry = val.1;
                    });
                    if next_found.is_none() {
                        return Err(SqueezefsError::InvalidOperation(
                            "next_dentry missing".into(),
                        ));
                    }
                    write_dentry_raw(storage, curr_offset, &next_dentry).await?;

                    // Clear the next slot
                    let mut empty = DiskDentry::new_zeroed();
                    empty.parent_ino = 0;
                    write_dentry_raw(storage, next_offset, &empty).await?;

                    // Update in-memory maps:
                    // 1. Remove deleted dentry from parent_ino's entry list
                    match storage.dentry_index.entry_sync(parent_ino) {
                        scc::hash_map::Entry::Occupied(mut occ) => {
                            occ.get_mut().retain(|(off, _)| *off != curr_offset);
                        }
                        _ => {}
                    }

                    // 2. Since next_dentry is moved from next_offset to curr_offset,
                    // we must update next_dentry.parent_ino's entry list!
                    match storage.dentry_index.entry_sync(next_dentry.parent_ino) {
                        scc::hash_map::Entry::Occupied(mut occ) => {
                            let entries = occ.get_mut();
                            // Remove next_offset
                            entries.retain(|(off, _)| *off != next_offset);
                            // Add curr_offset
                            entries.push((curr_offset, next_dentry));
                        }
                        scc::hash_map::Entry::Vacant(vac) => {
                            vac.insert_entry(vec![(curr_offset, next_dentry)]);
                        }
                    }

                    // 3. Update dentry_by_offset
                    storage.dentry_by_offset.remove_sync(&curr_offset);
                    let _ = storage
                        .dentry_by_offset
                        .insert_sync(curr_offset, (next_dentry.parent_ino, next_dentry));
                    storage.dentry_by_offset.remove_sync(&next_offset);
                    storage
                        .dentry_occupied_offsets
                        .lock()
                        .unwrap()
                        .remove(&next_offset);
                }
            } else {
                // It is a middle/tail dentry in the chain.
                // Update predecessor's next_ptr.
                let mut prev_p_ino = 0;
                let mut prev_dentry = DiskDentry::new_zeroed();
                let prev_found = storage.dentry_by_offset.read_sync(&prev_offset, |_, val| {
                    prev_p_ino = val.0;
                    prev_dentry = val.1;
                });
                if prev_found.is_none() {
                    return Err(SqueezefsError::InvalidOperation(
                        "prev_dentry missing".into(),
                    ));
                }
                prev_dentry.next_ptr = d.next_ptr;
                write_dentry_raw(storage, prev_offset, &prev_dentry).await?;

                // Clear the current slot.
                let mut empty = DiskDentry::new_zeroed();
                empty.parent_ino = 0;
                write_dentry_raw(storage, curr_offset, &empty).await?;

                // Update in-memory maps
                storage.dentry_by_offset.remove_sync(&curr_offset);
                storage
                    .dentry_occupied_offsets
                    .lock()
                    .unwrap()
                    .remove(&curr_offset);
                match storage.dentry_index.entry_sync(parent_ino) {
                    scc::hash_map::Entry::Occupied(mut occ) => {
                        occ.get_mut().retain(|(off, _)| *off != curr_offset);
                    }
                    _ => {}
                }
                // Update predecessor in parent index
                match storage.dentry_index.entry_sync(prev_p_ino) {
                    scc::hash_map::Entry::Occupied(mut occ) => {
                        if let Some(tuple) = occ
                            .get_mut()
                            .iter_mut()
                            .find(|(off, _)| *off == prev_offset)
                        {
                            tuple.1.next_ptr = d.next_ptr;
                        }
                    }
                    _ => {}
                }
                // Update predecessor in dentry_by_offset
                match storage.dentry_by_offset.entry_sync(prev_offset) {
                    scc::hash_map::Entry::Occupied(mut occ) => {
                        occ.get_mut().1.next_ptr = d.next_ptr;
                    }
                    _ => {}
                }
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
    storage.ensure_dentry_index().await?;
    let _guard = storage.dentry_lock.lock().await;

    let mut list = Vec::new();
    let _ = storage.dentry_index.read_sync(&parent_ino, |_, entries| {
        for (_, d) in entries {
            list.push(*d);
        }
    });
    Ok(list)
}
