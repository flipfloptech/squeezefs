use crate::error::{Result, SqueezefsError};
use crate::meta_backend::storage::{
    meta_sector_locks_enabled, MetaLvStorage, ACTIVE_TX, SECTOR_SIZE,
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

pub const INODE_TABLE_START: u64 = 8192;
pub const INODE_SLOT_SIZE: usize = 256;
pub const INODES_PER_SECTOR: usize = SECTOR_SIZE / INODE_SLOT_SIZE;

#[derive(IntoBytes, FromBytes, Immutable, Debug, Clone, Copy)]
#[repr(C)]
pub struct DiskInode {
    pub ino: u64,
    pub size: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub xattr_ptr: u64,
    pub magic: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub flags: u32,
    pub unused: [u8; 184], // Pads to exactly 256 bytes
}

impl DiskInode {
    pub fn new_zeroed() -> Self {
        unsafe { std::mem::zeroed() }
    }

    pub fn new(ino: u64, mode: u32, uid: u32, gid: u32) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        Self {
            magic: 0x4E4F4445, // "NODE" in hex
            ino,
            mode,
            uid,
            gid,
            nlink: 1,
            size: 0,
            atime: now,
            mtime: now,
            ctime: now,
            xattr_ptr: 0,
            flags: 0,
            unused: [0u8; 184],
        }
    }
}

/// Byte offset of inode slot `index` in the inode table.
#[inline]
fn slot_offset(index: u64) -> u64 {
    INODE_TABLE_START + index * INODE_SLOT_SIZE as u64
}

/// Extract inode `index` from a full 4 KiB sector image, validating magic.
fn extract_inode(sector_buf: &[u8], index: u64) -> Result<DiskInode> {
    let offset = slot_offset(index);
    let slot_in_sector = ((offset % SECTOR_SIZE as u64) / INODE_SLOT_SIZE as u64) as usize;
    let mut inode = DiskInode::new_zeroed();
    let slot_bytes =
        &sector_buf[slot_in_sector * INODE_SLOT_SIZE..(slot_in_sector + 1) * INODE_SLOT_SIZE];
    inode.as_mut_bytes().copy_from_slice(slot_bytes);

    if inode.magic == 0 {
        return Err(SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Inode slot {} is empty (magic 0)", index),
        )));
    }
    if inode.magic != 0x4E4F4445 {
        return Err(SqueezefsError::InvalidOperation(format!(
            "Inode slot {} has invalid magic: {:#X} (expected 0x4E4F4445), raw slot (first 32B): {:?}",
            index, inode.magic, &slot_bytes[..32]
        )));
    }
    Ok(inode)
}

/// Reads a disk inode from the inode table by its index.
///
/// Flag-on: takes the sector **read** lock only when *not* inside a transaction
/// (inside a tx the staging overlay + the DLM lock give a consistent view, and
/// taking the sector lock in the closure would risk commit-time reentrancy on
/// the non-reentrant tokio lock, design §3.4). Never takes `inode_lock`.
/// Flag-off: takes the global `inode_lock` (today's behavior).
pub async fn read_inode(storage: &MetaLvStorage, index: u64) -> Result<DiskInode> {
    let sector_offset = (slot_offset(index) / SECTOR_SIZE as u64) * SECTOR_SIZE as u64;
    let mut sector_buf = [0u8; SECTOR_SIZE];

    if meta_sector_locks_enabled() {
        let in_tx = ACTIVE_TX.try_with(|_| ()).is_ok();
        if in_tx {
            storage.read_blocks(sector_offset, &mut sector_buf).await?;
        } else {
            let _g = storage.sector_lock(sector_offset).read().await;
            storage.read_blocks(sector_offset, &mut sector_buf).await?;
        }
    } else {
        let _guard = storage.inode_lock.lock().await;
        storage.read_blocks(sector_offset, &mut sector_buf).await?;
    }
    extract_inode(&sector_buf, index)
}

/// Persist inode `index`.
///
/// Flag-on, in-transaction: stages a **256-byte sub-sector patch** at the slot's
/// byte offset. The whole-sector RMW is performed under the sector write lock at
/// commit, so siblings sharing the 4 KiB sector are preserved. Must be called
/// inside a transaction (`write_blocks` stages there); out-of-tx callers must
/// use [`write_inode`].
///
/// Flag-off: full-sector read-modify-write (stages the full sector image inside
/// a tx, else writes it directly).
pub async fn write_inode_raw(storage: &MetaLvStorage, index: u64, inode: &DiskInode) -> Result<()> {
    let offset = slot_offset(index);
    if meta_sector_locks_enabled() {
        storage.write_blocks(offset, inode.as_bytes()).await
    } else {
        let sector_offset = (offset / SECTOR_SIZE as u64) * SECTOR_SIZE as u64;
        let slot_in_sector = ((offset % SECTOR_SIZE as u64) / INODE_SLOT_SIZE as u64) as usize;
        let mut sector_buf = [0u8; SECTOR_SIZE];
        storage.read_blocks(sector_offset, &mut sector_buf).await?;
        sector_buf[slot_in_sector * INODE_SLOT_SIZE..(slot_in_sector + 1) * INODE_SLOT_SIZE]
            .copy_from_slice(inode.as_bytes());
        storage.write_blocks(sector_offset, &sector_buf).await
    }
}

/// Writes a disk inode into the inode table at its index.
///
/// Flag-on: never takes `inode_lock` (it would serialize all inode writes and
/// defeat closure concurrency). Inside a tx it stages the slot patch (commit
/// does the RMW); outside a tx it does a sector-safe full RMW under the sector
/// **write** lock, so direct writers (e.g. `set_layout_and_size`, the routed
/// cross-volume paths) never write a sector outside its lock (invariant R1).
/// Flag-off: takes `inode_lock` (today's behavior).
pub async fn write_inode(storage: &MetaLvStorage, index: u64, inode: &DiskInode) -> Result<()> {
    if meta_sector_locks_enabled() {
        let in_tx = ACTIVE_TX.try_with(|_| ()).is_ok();
        if in_tx {
            write_inode_raw(storage, index, inode).await
        } else {
            let offset = slot_offset(index);
            let sector_offset = (offset / SECTOR_SIZE as u64) * SECTOR_SIZE as u64;
            let slot_in_sector = ((offset % SECTOR_SIZE as u64) / INODE_SLOT_SIZE as u64) as usize;
            let _g = storage.sector_lock(sector_offset).write().await;
            let mut sector_buf = [0u8; SECTOR_SIZE];
            storage
                .read_blocks_direct(sector_offset, &mut sector_buf)
                .await?;
            sector_buf[slot_in_sector * INODE_SLOT_SIZE..(slot_in_sector + 1) * INODE_SLOT_SIZE]
                .copy_from_slice(inode.as_bytes());
            storage
                .write_blocks_direct(sector_offset, &sector_buf)
                .await
        }
    } else {
        let _guard = storage.inode_lock.lock().await;
        write_inode_raw(storage, index, inode).await
    }
}
