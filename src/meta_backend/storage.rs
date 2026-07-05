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
    pub path: PathBuf,
    pub superblock_lock: Arc<tokio::sync::Mutex<()>>,
    pub inode_lock: Arc<tokio::sync::Mutex<()>>,
    pub dentry_lock: Arc<tokio::sync::Mutex<()>>,
    pub xattr_lock: Arc<tokio::sync::Mutex<()>>,
    pub transaction_lock: Arc<tokio::sync::Mutex<()>>,
    pub dentry_index: Arc<scc::HashMap<u64, Vec<(u64, crate::meta_backend::dentry::DiskDentry)>>>,
    pub dentry_by_offset: Arc<scc::HashMap<u64, (u64, crate::meta_backend::dentry::DiskDentry)>>,
    pub dentry_occupied_offsets: Arc<std::sync::Mutex<std::collections::HashSet<u64>>>,
    pub dentry_index_initialized: Arc<tokio::sync::OnceCell<()>>,
}

tokio::task_local! {
    pub static ACTIVE_TX: std::sync::Arc<std::sync::Mutex<Vec<(std::path::PathBuf, u64, Vec<u8>)>>>;
    pub static FORCE_SYNC_TX: bool;
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
            transaction_lock: Arc::new(tokio::sync::Mutex::new(())),
            dentry_index: Arc::new(scc::HashMap::new()),
            dentry_by_offset: Arc::new(scc::HashMap::new()),
            dentry_occupied_offsets: Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
            dentry_index_initialized: Arc::new(tokio::sync::OnceCell::new()),
        };

        Ok(storage)
    }

    pub async fn ensure_dentry_index(&self) -> Result<()> {
        self.dentry_index_initialized
            .get_or_try_init(|| async {
                use crate::meta_backend::dentry::{
                    DiskDentry, DENTRY_SLOT_SIZE, DENTRY_TABLE_START, MAX_DENTRY_SLOTS,
                };
                use zerocopy::IntoBytes;

                let batch_sectors = 64;
                let batch_size = batch_sectors * SECTOR_SIZE;
                let mut buf = vec![0u8; batch_size];

                let total_slots = MAX_DENTRY_SLOTS;
                let slots_per_batch = batch_size / DENTRY_SLOT_SIZE;

                let mut local_occupied = std::collections::HashSet::new();

                for batch_idx in 0..(total_slots as usize / slots_per_batch) {
                    let batch_start_offset = DENTRY_TABLE_START + (batch_idx * batch_size) as u64;
                    self.read_blocks_direct(batch_start_offset, &mut buf)
                        .await?;

                    for slot_idx in 0..slots_per_batch {
                        let offset_in_buf = slot_idx * DENTRY_SLOT_SIZE;
                        let p_ino = u64::from_le_bytes(
                            buf[offset_in_buf..offset_in_buf + 8].try_into().unwrap(),
                        );
                        if p_ino != 0 {
                            let mut d = DiskDentry::new_zeroed();
                            d.as_mut_bytes().copy_from_slice(
                                &buf[offset_in_buf..offset_in_buf + DENTRY_SLOT_SIZE],
                            );
                            let offset = batch_start_offset + (slot_idx * DENTRY_SLOT_SIZE) as u64;

                            local_occupied.insert(offset);

                            self.dentry_index
                                .entry_sync(p_ino)
                                .or_default()
                                .get_mut()
                                .push((offset, d));

                            let _ = self.dentry_by_offset.insert_sync(offset, (p_ino, d));
                        }
                    }
                }

                let mut occupied = self.dentry_occupied_offsets.lock().unwrap();
                *occupied = local_occupied;

                Ok::<(), SqueezefsError>(())
            })
            .await?;
        Ok(())
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
            for (path, off, data) in guard.iter().rev() {
                if path == &self.path && *off == offset && data.len() == buf.len() {
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

    pub fn get_size(&self) -> u64 {
        if let Ok(file) = OpenOptions::new().read(true).open(&self.path) {
            use std::io::Seek;
            let mut f = file;
            f.seek(std::io::SeekFrom::End(0)).unwrap_or(0)
        } else {
            0
        }
    }

    pub fn max_inodes(&self) -> usize {
        let size = self.get_size();
        let xattr_start = 1024 * 1024 * 72; // XATTR_BLOCK_START
        if size > xattr_start {
            ((size - xattr_start) / 4096) as usize
        } else {
            0
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
        let read_len = bytes.len();
        if read_len > 0 {
            let limit = std::cmp::min(read_len, buf.len());
            buf[..limit].copy_from_slice(&bytes[..limit]);
        }
        if read_len < buf.len() {
            buf[read_len..].fill(0);
        }
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
            tx.lock()
                .unwrap()
                .push((self.path.clone(), offset, buf.to_vec()));
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
