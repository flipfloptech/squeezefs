use crate::error::{Result, SqueezefsError};
use parking_lot::Mutex;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zerocopy::{FromBytes, Immutable, IntoBytes};

pub const MAGIC_VALUE: &[u8; 8] = b"METALV01";
pub const SECTOR_SIZE: usize = 4096;

#[derive(IntoBytes, FromBytes, Immutable, Debug, Clone, Copy)]
#[repr(C)]
pub struct Superblock {
    pub magic: [u8; 8],
    pub version: u32,
    pub inode_count: u32,
    pub free_inode_bitmap_root: u64,
    pub dentry_root: u64,
    pub journal_start: u64,
    pub journal_size: u64,
    pub checksum: u64,
}

impl Superblock {
    pub fn new_zeroed() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[derive(Clone)]
pub struct MetaLvStorage {
    file: Arc<Mutex<File>>,
    path: PathBuf,
    op_lock: Arc<parking_lot::ReentrantMutex<()>>,
}

impl MetaLvStorage {
    /// Opens the raw metadata partition (file or block device).
    /// If the path does not exist, a simulated file is created.
    pub fn open<P: AsRef<Path>>(path: P, size_limit: u64) -> Result<Self> {
        let path_ref = path.as_ref();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path_ref)
            .map_err(SqueezefsError::Io)?;

        use std::os::unix::fs::FileTypeExt;
        let meta = file.metadata().map_err(SqueezefsError::Io)?;
        if !meta.file_type().is_block_device() && meta.len() < size_limit && size_limit > 0 {
            file.set_len(size_limit).map_err(SqueezefsError::Io)?;
        }

        let storage = Self {
            file: Arc::new(Mutex::new(file)),
            path: path_ref.to_path_buf(),
            op_lock: Arc::new(parking_lot::ReentrantMutex::new(())),
        };

        Ok(storage)
    }

    pub fn lock_op(&self) -> parking_lot::ReentrantMutexGuard<'_, ()> {
        self.op_lock.lock()
    }

    /// Read the Superblock at offset 0
    pub fn read_superblock(&self) -> Result<Superblock> {
        let mut buf = [0u8; SECTOR_SIZE];
        let file = self.file.lock();
        file.read_exact_at(&mut buf, 0)
            .map_err(SqueezefsError::Io)?;

        let mut sb = Superblock::new_zeroed();
        let sb_len = sb.as_bytes().len();
        if buf.len() >= sb_len {
            sb.as_mut_bytes().copy_from_slice(&buf[..sb_len]);
        }

        if &sb.magic != MAGIC_VALUE {
            return Err(SqueezefsError::InvalidOperation(format!(
                "Invalid superblock magic: {:?}",
                sb.magic
            )));
        }

        Ok(sb)
    }

    /// Write the Superblock at offset 0
    pub fn write_superblock(&self, sb: &Superblock) -> Result<()> {
        let mut buf = [0u8; SECTOR_SIZE];
        let sb_bytes = sb.as_bytes();
        buf[..sb_bytes.len()].copy_from_slice(sb_bytes);

        let file = self.file.lock();
        file.write_all_at(&buf, 0).map_err(SqueezefsError::Io)?;
        file.sync_all().map_err(SqueezefsError::Io)?;

        Ok(())
    }

    /// Direct block read at a sector-aligned offset
    pub fn read_blocks(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if offset % SECTOR_SIZE as u64 != 0 {
            return Err(SqueezefsError::InvalidOperation(format!(
                "Read offset {} must be sector-aligned",
                offset
            )));
        }
        let file = self.file.lock();
        file.read_exact_at(buf, offset)
            .map_err(SqueezefsError::Io)?;
        Ok(())
    }

    /// Direct block write at a sector-aligned offset
    pub fn write_blocks(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if offset % SECTOR_SIZE as u64 != 0 {
            return Err(SqueezefsError::InvalidOperation(format!(
                "Write offset {} must be sector-aligned",
                offset
            )));
        }
        let file = self.file.lock();
        file.write_all_at(buf, offset).map_err(SqueezefsError::Io)?;
        file.sync_all().map_err(SqueezefsError::Io)?;
        Ok(())
    }

    pub fn device_path(&self) -> &Path {
        &self.path
    }
}
