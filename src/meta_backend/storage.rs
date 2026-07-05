use crate::error::{Result, SqueezefsError};
use std::fs::OpenOptions;
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
    path: PathBuf,
    pub superblock_lock: Arc<tokio::sync::Mutex<()>>,
    pub inode_lock: Arc<tokio::sync::Mutex<()>>,
    pub dentry_lock: Arc<tokio::sync::Mutex<()>>,
    pub xattr_lock: Arc<tokio::sync::Mutex<()>>,
}

tokio::task_local! {
    pub static ACTIVE_TX: std::sync::Arc<std::sync::Mutex<Vec<(u64, Vec<u8>)>>>;
}

impl MetaLvStorage {
    /// Opens the raw metadata partition (file or block device).
    /// If the path does not exist, a simulated file is created.
    pub fn open<P: AsRef<Path>>(path: P, size_limit: u64) -> Result<Self> {
        let path_ref = path.as_ref();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path_ref)
            .map_err(SqueezefsError::Io)?;

        use std::io::Seek;
        let dev_size = file
            .seek(std::io::SeekFrom::End(0))
            .map_err(SqueezefsError::Io)?;

        use std::os::unix::fs::FileTypeExt;
        let meta = file.metadata().map_err(SqueezefsError::Io)?;
        if !meta.file_type().is_block_device() {
            if dev_size < size_limit && size_limit > 0 {
                file.set_len(size_limit).map_err(SqueezefsError::Io)?;
            }
        } else {
            if dev_size < size_limit && size_limit > 0 {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "Metadata block device {} is too small: size {} bytes, expected at least {} bytes",
                    path_ref.display(), dev_size, size_limit
                )));
            }
        }

        let storage = Self {
            path: path_ref.to_path_buf(),
            superblock_lock: Arc::new(tokio::sync::Mutex::new(())),
            inode_lock: Arc::new(tokio::sync::Mutex::new(())),
            dentry_lock: Arc::new(tokio::sync::Mutex::new(())),
            xattr_lock: Arc::new(tokio::sync::Mutex::new(())),
        };

        Ok(storage)
    }

    /// Read the Superblock at offset 0
    pub async fn read_superblock(&self) -> Result<Superblock> {
        let _guard = self.superblock_lock.lock().await;
        let bytes = crate::uring_fs::read_at(&self.path, 0, SECTOR_SIZE).await?;
        let mut sb = Superblock::new_zeroed();
        let sb_len = sb.as_bytes().len();
        if bytes.len() >= sb_len {
            sb.as_mut_bytes().copy_from_slice(&bytes[..sb_len]);
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
    pub async fn write_superblock(&self, sb: &Superblock) -> Result<()> {
        let _guard = self.superblock_lock.lock().await;
        let mut buf = [0u8; SECTOR_SIZE];
        let sb_bytes = sb.as_bytes();
        buf[..sb_bytes.len()].copy_from_slice(sb_bytes);

        crate::uring_fs::write_at(&self.path, 0, bytes::Bytes::copy_from_slice(&buf)).await?;
        Ok(())
    }

    /// Direct block read at a sector-aligned offset
    pub async fn read_blocks(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if offset % SECTOR_SIZE as u64 != 0 {
            return Err(SqueezefsError::InvalidOperation(format!(
                "Read offset {} must be sector-aligned",
                offset
            )));
        }

        let mut found_in_tx = false;
        let _ = ACTIVE_TX.try_with(|tx| {
            let guard = tx.lock().unwrap();
            for (off, data) in guard.iter().rev() {
                if *off == offset && data.len() == buf.len() {
                    buf.copy_from_slice(data);
                    found_in_tx = true;
                    break;
                }
            }
        });

        if found_in_tx {
            Ok(())
        } else {
            self.read_blocks_direct(offset, buf).await
        }
    }

    pub async fn read_blocks_direct(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if offset % SECTOR_SIZE as u64 != 0 {
            return Err(SqueezefsError::InvalidOperation(format!(
                "Read offset {} must be sector-aligned",
                offset
            )));
        }
        let bytes = crate::uring_fs::read_at(&self.path, offset, buf.len()).await?;
        buf.copy_from_slice(&bytes);
        Ok(())
    }

    /// Direct block write at a sector-aligned offset
    pub async fn write_blocks(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if offset % SECTOR_SIZE as u64 != 0 {
            return Err(SqueezefsError::InvalidOperation(format!(
                "Write offset {} must be sector-aligned",
                offset
            )));
        }

        let mut redirected = false;
        let _ = ACTIVE_TX.try_with(|tx| {
            tx.lock().unwrap().push((offset, buf.to_vec()));
            redirected = true;
        });

        if redirected {
            Ok(())
        } else {
            self.write_blocks_direct(offset, buf).await
        }
    }

    pub async fn write_blocks_direct(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if offset % SECTOR_SIZE as u64 != 0 {
            return Err(SqueezefsError::InvalidOperation(format!(
                "Write offset {} must be sector-aligned",
                offset
            )));
        }
        crate::uring_fs::write_at(&self.path, offset, bytes::Bytes::copy_from_slice(buf)).await?;
        Ok(())
    }

    pub fn device_path(&self) -> &Path {
        &self.path
    }

    pub async fn wipe(&self, quick: bool, pb: Option<indicatif::ProgressBar>) -> Result<()> {
        let size = {
            let file = OpenOptions::new()
                .read(true)
                .open(&self.path)
                .map_err(SqueezefsError::Io)?;
            let mut size = 0;
            if let Ok(meta) = file.metadata() {
                size = meta.len();
            }
            size
        };
        let wipe_len = if quick {
            if size > 0 {
                std::cmp::min(size, 108 * 1024 * 1024)
            } else {
                108 * 1024 * 1024
            }
        } else {
            if size > 0 {
                std::cmp::max(size, 128 * 1024 * 1024)
            } else {
                128 * 1024 * 1024
            }
        };

        if let Some(ref p_bar) = pb {
            p_bar.set_length(wipe_len);
        }

        let zeros = vec![0u8; 1024 * 1024];
        let mut written = 0;
        while written < wipe_len {
            let to_write = std::cmp::min(zeros.len() as u64, wipe_len - written) as usize;
            crate::uring_fs::write_at(
                &self.path,
                written,
                bytes::Bytes::copy_from_slice(&zeros[..to_write]),
            )
            .await?;
            written += to_write as u64;
            if let Some(ref p_bar) = pb {
                p_bar.inc(to_write as u64);
            }
        }
        if let Some(ref p_bar) = pb {
            p_bar.finish_with_message("Complete");
        }
        Ok(())
    }
}
