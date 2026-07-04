use crate::error::{Result, SqueezefsError};
use crate::meta_backend::storage::{MetaLvStorage, SECTOR_SIZE};
use zerocopy::{FromBytes, Immutable, IntoBytes};

pub const INODE_TABLE_START: u64 = 4096;
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
        Self {
            magic: 0x4E4F4445, // "NODE" in hex
            ino,
            mode,
            uid,
            gid,
            nlink: 1,
            size: 0,
            atime: 0,
            mtime: 0,
            ctime: 0,
            xattr_ptr: 0,
            flags: 0,
            unused: [0u8; 184],
        }
    }
}

/// Reads a disk inode from the inode table by its index.
pub fn read_inode(storage: &MetaLvStorage, index: u64) -> Result<DiskInode> {
    let _guard = storage.lock_op();
    let offset = INODE_TABLE_START + index * INODE_SLOT_SIZE as u64;
    let sector_offset = (offset / SECTOR_SIZE as u64) * SECTOR_SIZE as u64;
    let slot_in_sector = ((offset % SECTOR_SIZE as u64) / INODE_SLOT_SIZE as u64) as usize;

    let mut sector_buf = [0u8; SECTOR_SIZE];
    storage.read_blocks(sector_offset, &mut sector_buf)?;

    let mut inode = DiskInode::new_zeroed();
    let slot_bytes =
        &sector_buf[slot_in_sector * INODE_SLOT_SIZE..(slot_in_sector + 1) * INODE_SLOT_SIZE];
    inode.as_mut_bytes().copy_from_slice(slot_bytes);

    if inode.magic != 0x4E4F4445 {
        return Err(SqueezefsError::InvalidOperation(format!(
            "Inode slot {} has invalid magic: {:#X} (expected 0x4E4F4445), raw slot (first 32B): {:?}",
            index, inode.magic, &slot_bytes[..32]
        )));
    }

    Ok(inode)
}

pub fn write_inode_raw(storage: &MetaLvStorage, index: u64, inode: &DiskInode) -> Result<()> {
    let offset = INODE_TABLE_START + index * INODE_SLOT_SIZE as u64;
    let sector_offset = (offset / SECTOR_SIZE as u64) * SECTOR_SIZE as u64;
    let slot_in_sector = ((offset % SECTOR_SIZE as u64) / INODE_SLOT_SIZE as u64) as usize;

    let mut sector_buf = [0u8; SECTOR_SIZE];
    storage.read_blocks(sector_offset, &mut sector_buf)?;

    let slot_bytes = inode.as_bytes();
    sector_buf[slot_in_sector * INODE_SLOT_SIZE..(slot_in_sector + 1) * INODE_SLOT_SIZE]
        .copy_from_slice(slot_bytes);

    storage.write_blocks(sector_offset, &sector_buf)?;
    Ok(())
}

/// Writes a disk inode into the inode table at its index.
pub fn write_inode(storage: &MetaLvStorage, index: u64, inode: &DiskInode) -> Result<()> {
    let _guard = storage.lock_op();
    write_inode_raw(storage, index, inode)
}
