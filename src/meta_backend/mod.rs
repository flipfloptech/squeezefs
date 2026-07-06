pub mod dentry;
pub mod dlm;
pub mod inode;
pub mod journal;
pub mod storage;
pub mod xattr;

use crate::error::Result;

pub type Ino = u64;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inode {
    pub ino: Ino,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub nlink: u32,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub flags: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub ino: Ino,
    pub name: String,
    pub file_type: u32,
}

#[async_trait::async_trait]
pub trait Metadata: Send + Sync {
    async fn lookup(&self, parent: Ino, name: &str) -> Result<Inode>;
    async fn create(&self, parent: Ino, name: &str, mode: u32, uid: u32, gid: u32)
        -> Result<Inode>;
    async fn unlink(&self, parent: Ino, name: &str) -> Result<Ino>;
    async fn link(&self, ino: Ino, new_parent: Ino, new_name: &str) -> Result<Inode>;
    async fn rename(
        &self,
        old_parent: Ino,
        old_name: &str,
        new_parent: Ino,
        new_name: &str,
        flags: u32,
    ) -> Result<()>;
    async fn readdir(&self, dir: Ino, offset: u64, max: usize) -> Result<Vec<DirEntry>>;
    async fn getattr(&self, ino: Ino) -> Result<Inode>;
    async fn setattr(
        &self,
        ino: Ino,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<u64>,
        mtime: Option<u64>,
        ctime: Option<u64>,
    ) -> Result<Inode>;
    async fn getxattr(&self, ino: Ino, name: &str) -> Result<Option<Vec<u8>>>;
    async fn setxattr(&self, ino: Ino, name: &str, value: &[u8]) -> Result<()>;
    async fn removexattr(&self, ino: Ino, name: &str) -> Result<()>;
    async fn listxattr(&self, ino: Ino) -> Result<Vec<String>>;
    async fn destroy_inode(&self, ino: Ino) -> Result<()>;
}

pub struct MetaLvBackend {
    pub storage: storage::MetaLvStorage,
    pub dlm: dlm::DlmLockManager,
    pub journal: journal::Journal,
}

impl MetaLvBackend {
    pub fn new(storage: storage::MetaLvStorage) -> Self {
        let journal = journal::Journal::new(1024 * 1024 * 104, 1024 * 1024 * 4); // 104MB offset, 4MB size
        Self {
            storage,
            dlm: dlm::DlmLockManager::new(),
            journal,
        }
    }

    pub async fn run_transaction<F, Fut, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<R>>,
    {
        use crate::meta_backend::storage::ACTIVE_TX;
        if ACTIVE_TX.try_with(|_| ()).is_ok() {
            return f().await;
        }

        let _tx_lock_guard = self.storage.transaction_lock.lock().await;

        let tx: std::sync::Arc<std::sync::Mutex<Vec<(std::path::PathBuf, u64, Vec<u8>)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let tx_clone = tx.clone();

        let res = ACTIVE_TX.scope(tx_clone, f()).await;

        match res {
            Ok(ret) => {
                let ops = tx.lock().unwrap().clone();
                if !ops.is_empty() {
                    let mut groups: std::collections::HashMap<
                        std::path::PathBuf,
                        Vec<(u64, Vec<u8>)>,
                    > = std::collections::HashMap::new();
                    for (path, offset, buf) in ops {
                        groups.entry(path).or_default().push((offset, buf));
                    }

                    let sync = crate::meta_backend::storage::FORCE_SYNC_TX
                        .try_with(|v| *v)
                        .unwrap_or(false);

                    for (path, path_ops) in groups {
                        if path == self.storage.path {
                            let record_bytes = bincode::serialize(&path_ops).map_err(|e| {
                                crate::error::SqueezefsError::Io(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    format!("Failed to serialize transaction record: {:?}", e),
                                ))
                            })?;
                            self.journal
                                .write_record(&self.storage, &record_bytes, sync)
                                .await?;
                            let mut need_sb = false;
                            let mut need_inode = false;
                            let mut need_dentry = false;
                            let mut need_xattr = false;

                            for &(offset, _) in &path_ops {
                                if offset < 4096 {
                                    need_sb = true;
                                } else if offset < dentry::DENTRY_TABLE_START {
                                    need_inode = true;
                                } else if offset < xattr::XATTR_BLOCK_START {
                                    need_dentry = true;
                                } else {
                                    need_xattr = true;
                                }
                            }

                            let _sb_guard = if need_sb {
                                Some(self.storage.superblock_lock.lock().await)
                            } else {
                                None
                            };
                            let _inode_guard = if need_inode {
                                Some(self.storage.inode_lock.lock().await)
                            } else {
                                None
                            };
                            let _dentry_guard = if need_dentry {
                                Some(self.storage.dentry_lock.lock().await)
                            } else {
                                None
                            };
                            let _xattr_guard = if need_xattr {
                                Some(self.storage.xattr_lock.lock().await)
                            } else {
                                None
                            };

                            for (offset, buf) in path_ops {
                                self.storage.write_blocks_direct(offset, &buf).await?;
                            }
                        } else {
                            for (offset, buf) in path_ops {
                                crate::uring_fs::write_at(
                                    &path,
                                    offset,
                                    bytes::Bytes::copy_from_slice(&buf),
                                )
                                .await?;
                            }
                        }
                    }
                }
                Ok(ret)
            }
            Err(e) => Err(e),
        }
    }

    pub async fn get_allocated_inode_count(&self) -> usize {
        let mut bitmap_sector = [0u8; 4096];
        if self
            .storage
            .read_blocks(4096, &mut bitmap_sector)
            .await
            .is_err()
        {
            return 0;
        }
        let mut count = 0;
        for i in 2..20000 {
            let byte_idx = i / 8;
            let bit_idx = i % 8;
            if (bitmap_sector[byte_idx] & (1 << bit_idx)) != 0 {
                count += 1;
            }
        }
        count
    }

    /// Formats the raw block storage device with a superblock and the root inode
    pub async fn format(storage: &storage::MetaLvStorage) -> Result<()> {
        Self::format_with_options(storage, true, None).await
    }

    pub async fn format_with_options(
        storage: &storage::MetaLvStorage,
        quick: bool,
        pb: Option<indicatif::ProgressBar>,
    ) -> Result<()> {
        // Zero-wipe the entire metadata volume first to prevent stale garbage issues
        storage.wipe(quick, pb).await?;

        // Initialize Superblock
        let sb = storage::Superblock {
            magic: *storage::MAGIC_VALUE,
            version: 2,
            inode_count: 1000000,
            free_inode_bitmap_root: 4096,
            dentry_root: dentry::DENTRY_TABLE_START,
            journal_start: 1024 * 1024 * 104,
            journal_size: 1024 * 1024 * 4,
            checksum: 0,
        };
        storage.write_superblock(&sb).await?;

        // Zero-initialize the free-inode bitmap sector (sector 1, starting at 4096)
        let mut bitmap_sector = [0u8; 4096];
        // Mark index 0 and 1 as allocated
        bitmap_sector[0] = 0b0000_0011;
        storage.write_blocks(4096, &bitmap_sector).await?;

        // Format root Inode (ino 1)
        let root_uid = unsafe { libc::getuid() };
        let root_gid = unsafe { libc::getgid() };
        let root_inode = inode::DiskInode::new(1, libc::S_IFDIR | 0o755, root_uid, root_gid);
        inode::write_inode(storage, 1, &root_inode).await?;

        Ok(())
    }
}

#[async_trait::async_trait]
impl Metadata for MetaLvBackend {
    async fn lookup(&self, parent: Ino, name: &str) -> Result<Inode> {
        let _guard = self.dlm.lock_shared(&format!("D{}:{}", parent, name)).await;
        if let Some(dentry) = dentry::find_dentry(&self.storage, parent, name).await? {
            self.getattr(dentry.child_ino).await
        } else {
            Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Dentry {} not found in parent {}", name, parent),
            )))
        }
    }

    async fn create(
        &self,
        parent: Ino,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Inode> {
        let _parent_guard = self.dlm.lock_exclusive(&format!("I{}", parent)).await;
        let _dentry_guard = self
            .dlm
            .lock_exclusive(&format!("D{}:{}", parent, name))
            .await;

        if let Some(_) = dentry::find_dentry(&self.storage, parent, name).await? {
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "File already exists".to_string(),
            ));
        }

        let parent_inode = inode::read_inode(&self.storage, parent).await?;
        let mut final_gid = gid;
        let mut final_mode = mode;
        if (parent_inode.mode & libc::S_ISGID) != 0 {
            final_gid = parent_inode.gid;
            if (mode & libc::S_IFMT) == libc::S_IFDIR {
                final_mode |= libc::S_ISGID;
            }
        }

        self.run_transaction(|| async {
            let new_ino;
            {
                let _guard = self.storage.inode_lock.lock().await;
                new_ino = self.storage.alloc_inode_bit_locked().await?;
                let disk_inode = inode::DiskInode::new(new_ino, final_mode, uid, final_gid);
                inode::write_inode_raw(&self.storage, new_ino, &disk_inode).await?;
            }

            dentry::insert_dentry(
                &self.storage,
                parent,
                new_ino,
                name,
                final_mode & libc::S_IFMT,
            )
            .await?;

            // Update parent directory times
            if let Ok(mut parent_inode) = inode::read_inode(&self.storage, parent).await {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                parent_inode.mtime = now;
                parent_inode.ctime = now;
                let _ = inode::write_inode(&self.storage, parent, &parent_inode).await;
            }

            let disk_inode = inode::read_inode(&self.storage, new_ino).await?;
            Ok(Inode {
                ino: new_ino,
                mode: disk_inode.mode,
                uid: disk_inode.uid,
                gid: disk_inode.gid,
                size: disk_inode.size,
                nlink: disk_inode.nlink,
                atime: disk_inode.atime,
                mtime: disk_inode.mtime,
                ctime: disk_inode.ctime,
                flags: disk_inode.flags,
            })
        })
        .await
    }

    async fn unlink(&self, parent: Ino, name: &str) -> Result<Ino> {
        let _parent_guard = self.dlm.lock_exclusive(&format!("I{}", parent)).await;
        let _dentry_guard = self
            .dlm
            .lock_exclusive(&format!("D{}:{}", parent, name))
            .await;

        if let Some(dentry) = dentry::find_dentry(&self.storage, parent, name).await? {
            let ino = dentry.child_ino;
            let _inode_guard = if parent != ino {
                Some(self.dlm.lock_exclusive(&format!("I{}", ino)).await)
            } else {
                None
            };

            self.run_transaction(|| async {
                dentry::remove_dentry(&self.storage, parent, name).await?;

                // Update parent directory times
                if let Ok(mut parent_inode) = inode::read_inode(&self.storage, parent).await {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos() as u64;
                    parent_inode.mtime = now;
                    parent_inode.ctime = now;
                    let _ = inode::write_inode(&self.storage, parent, &parent_inode).await;
                }

                // Decrement nlink
                let mut disk_inode = inode::read_inode(&self.storage, ino).await?;
                if disk_inode.nlink > 0 {
                    disk_inode.nlink -= 1;
                }
                disk_inode.ctime = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                inode::write_inode(&self.storage, ino, &disk_inode).await?;
                Ok(())
            })
            .await?;
            Ok(ino)
        } else {
            Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Dentry not found",
            )))
        }
    }

    async fn link(&self, ino: Ino, new_parent: Ino, new_name: &str) -> Result<Inode> {
        let _parent_guard = self.dlm.lock_exclusive(&format!("I{}", new_parent)).await;
        let _dentry_guard = self
            .dlm
            .lock_exclusive(&format!("D{}:{}", new_parent, new_name))
            .await;

        if let Some(_) = dentry::find_dentry(&self.storage, new_parent, new_name).await? {
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "File already exists".to_string(),
            ));
        }

        let _inode_guard = self.dlm.lock_exclusive(&format!("I{}", ino)).await;
        let mut disk_inode = inode::read_inode(&self.storage, ino).await?;
        disk_inode.nlink += 1;
        disk_inode.ctime = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;

        self.run_transaction(|| async {
            inode::write_inode(&self.storage, ino, &disk_inode).await?;

            dentry::insert_dentry(
                &self.storage,
                new_parent,
                ino,
                new_name,
                disk_inode.mode & libc::S_IFMT,
            )
            .await?;

            // Update parent directory times
            if let Ok(mut parent_inode) = inode::read_inode(&self.storage, new_parent).await {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                parent_inode.mtime = now;
                parent_inode.ctime = now;
                let _ = inode::write_inode(&self.storage, new_parent, &parent_inode).await;
            }
            Ok(())
        })
        .await?;

        Ok(Inode {
            ino,
            mode: disk_inode.mode,
            uid: disk_inode.uid,
            gid: disk_inode.gid,
            size: disk_inode.size,
            nlink: disk_inode.nlink,
            atime: disk_inode.atime,
            mtime: disk_inode.mtime,
            ctime: disk_inode.ctime,
            flags: disk_inode.flags,
        })
    }

    async fn rename(
        &self,
        old_parent: Ino,
        old_name: &str,
        new_parent: Ino,
        new_name: &str,
        flags: u32,
    ) -> Result<()> {
        if flags & (libc::RENAME_NOREPLACE | libc::RENAME_EXCHANGE)
            == (libc::RENAME_NOREPLACE | libc::RENAME_EXCHANGE)
        {
            return Err(crate::error::SqueezefsError::Io(
                std::io::Error::from_raw_os_error(libc::EINVAL),
            ));
        }

        let mut parents = vec![old_parent, new_parent];
        parents.sort_unstable();
        parents.dedup();
        let mut _parent_guards = Vec::new();
        for p in parents {
            _parent_guards.push(self.dlm.lock_exclusive(&format!("I{}", p)).await);
        }

        let old_dentry_key = format!("D{}:{}", old_parent, old_name);
        let new_dentry_key = format!("D{}:{}", new_parent, new_name);
        let mut dentries = vec![old_dentry_key.clone(), new_dentry_key.clone()];
        dentries.sort_unstable();
        dentries.dedup();
        let mut _dentry_guards = Vec::new();
        for d in dentries {
            _dentry_guards.push(self.dlm.lock_exclusive(&d).await);
        }

        let old_dentry_opt = dentry::find_dentry(&self.storage, old_parent, old_name).await?;
        let new_dentry_opt = dentry::find_dentry(&self.storage, new_parent, new_name).await?;

        if flags & libc::RENAME_EXCHANGE != 0 {
            let old_dentry = old_dentry_opt.ok_or_else(|| {
                crate::error::SqueezefsError::Io(std::io::Error::from_raw_os_error(libc::ENOENT))
            })?;
            let new_dentry = new_dentry_opt.ok_or_else(|| {
                crate::error::SqueezefsError::Io(std::io::Error::from_raw_os_error(libc::ENOENT))
            })?;

            self.run_transaction(|| async {
                dentry::remove_dentry(&self.storage, old_parent, old_name).await?;
                dentry::remove_dentry(&self.storage, new_parent, new_name).await?;

                dentry::insert_dentry(
                    &self.storage,
                    new_parent,
                    old_dentry.child_ino,
                    new_name,
                    old_dentry.file_type,
                )
                .await?;

                dentry::insert_dentry(
                    &self.storage,
                    old_parent,
                    new_dentry.child_ino,
                    old_name,
                    new_dentry.file_type,
                )
                .await?;

                Ok(())
            })
            .await
        } else {
            if flags & libc::RENAME_NOREPLACE != 0 && new_dentry_opt.is_some() {
                return Err(crate::error::SqueezefsError::Io(
                    std::io::Error::from_raw_os_error(libc::EEXIST),
                ));
            }

            if let Some(dentry) = old_dentry_opt {
                self.run_transaction(|| async {
                    if new_dentry_opt.is_some() {
                        dentry::remove_dentry(&self.storage, new_parent, new_name).await?;
                    }
                    dentry::remove_dentry(&self.storage, old_parent, old_name).await?;
                    dentry::insert_dentry(
                        &self.storage,
                        new_parent,
                        dentry.child_ino,
                        new_name,
                        dentry.file_type,
                    )
                    .await?;
                    Ok(())
                })
                .await
            } else {
                Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Source dentry not found",
                )))
            }
        }
    }

    async fn readdir(&self, dir: Ino, _offset: u64, _max: usize) -> Result<Vec<DirEntry>> {
        let _guard = self.dlm.lock_shared(&format!("I{}", dir)).await;
        let dentries = dentry::list_dentries(&self.storage, dir).await?;
        let mut list = Vec::new();
        for d in dentries {
            list.push(DirEntry {
                ino: d.child_ino,
                name: d.get_name(),
                file_type: d.file_type,
            });
        }
        Ok(list)
    }

    async fn getattr(&self, ino: Ino) -> Result<Inode> {
        let _guard = self.dlm.lock_shared(&format!("I{}", ino)).await;
        let disk_inode = inode::read_inode(&self.storage, ino).await?;
        Ok(Inode {
            ino: disk_inode.ino,
            mode: disk_inode.mode,
            uid: disk_inode.uid,
            gid: disk_inode.gid,
            size: disk_inode.size,
            nlink: disk_inode.nlink,
            atime: disk_inode.atime,
            mtime: disk_inode.mtime,
            ctime: disk_inode.ctime,
            flags: disk_inode.flags,
        })
    }

    async fn setattr(
        &self,
        ino: Ino,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<u64>,
        mtime: Option<u64>,
        ctime: Option<u64>,
    ) -> Result<Inode> {
        let _guard = self.dlm.lock_exclusive(&format!("I{}", ino)).await;
        let mut disk_inode = inode::read_inode(&self.storage, ino).await?;
        let mut ctime_updated = false;
        if let Some(m) = mode {
            disk_inode.mode = m;
            ctime_updated = true;
        }
        if let Some(u) = uid {
            disk_inode.uid = u;
            ctime_updated = true;
        }
        if let Some(g) = gid {
            disk_inode.gid = g;
            ctime_updated = true;
        }
        if let Some(s) = size {
            disk_inode.size = s;
            ctime_updated = true;
        }
        if let Some(a) = atime {
            disk_inode.atime = a;
        }
        if let Some(m) = mtime {
            disk_inode.mtime = m;
        }
        if let Some(c) = ctime {
            disk_inode.ctime = c;
        } else if ctime_updated {
            disk_inode.ctime = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
        }
        self.run_transaction(|| async {
            inode::write_inode(&self.storage, ino, &disk_inode).await?;
            Ok(())
        })
        .await?;

        Ok(Inode {
            ino: disk_inode.ino,
            mode: disk_inode.mode,
            uid: disk_inode.uid,
            gid: disk_inode.gid,
            size: disk_inode.size,
            nlink: disk_inode.nlink,
            atime: disk_inode.atime,
            mtime: disk_inode.mtime,
            ctime: disk_inode.ctime,
            flags: disk_inode.flags,
        })
    }

    async fn getxattr(&self, ino: Ino, name: &str) -> Result<Option<Vec<u8>>> {
        let _guard = self.dlm.lock_shared(&format!("I{}", ino)).await;
        xattr::get_xattr(&self.storage, ino, name).await
    }

    async fn setxattr(&self, ino: Ino, name: &str, value: &[u8]) -> Result<()> {
        let _guard = self.dlm.lock_exclusive(&format!("I{}", ino)).await;
        self.run_transaction(|| async { xattr::set_xattr(&self.storage, ino, name, value).await })
            .await
    }

    async fn removexattr(&self, ino: Ino, name: &str) -> Result<()> {
        let _guard = self.dlm.lock_exclusive(&format!("I{}", ino)).await;
        self.run_transaction(|| async { xattr::remove_xattr(&self.storage, ino, name).await })
            .await
    }

    async fn listxattr(&self, ino: Ino) -> Result<Vec<String>> {
        let _guard = self.dlm.lock_shared(&format!("I{}", ino)).await;
        xattr::list_xattrs(&self.storage, ino).await
    }

    async fn destroy_inode(&self, ino: Ino) -> Result<()> {
        let _guard = self.dlm.lock_exclusive(&format!("I{}", ino)).await;

        // Check under exclusive lock to prevent TOCTOU race (P0-8)
        match inode::read_inode(&self.storage, ino).await {
            Ok(disk_inode) => {
                if disk_inode.nlink > 0 {
                    log::debug!(
                        "destroy_inode: ino {} has nlink = {}, skipping destruction",
                        ino,
                        disk_inode.nlink
                    );
                    return Ok(());
                }
            }
            Err(_) => {
                // If it doesn't exist or is already zeroed (magic 0), return success
                return Ok(());
            }
        }

        self.run_transaction(|| async {
            // Bitmap free + zero slot must be one critical section so a concurrent
            // create cannot reallocate the bit before the slot is cleared.
            let _inode_guard = self.storage.inode_lock.lock().await;
            self.storage.free_inode_bit_locked(ino).await?;
            let empty = inode::DiskInode::new_zeroed();
            inode::write_inode_raw(&self.storage, ino, &empty).await?;
            Ok(())
        })
        .await
    }
}

#[derive(Clone)]
pub struct RoutedMetaBackend {
    pub volumes: Vec<std::sync::Arc<MetaLvBackend>>,
    pub disabled_volumes: std::sync::Arc<dashmap::DashMap<usize, bool, ahash::RandomState>>,
    pub redirections: std::sync::Arc<dashmap::DashMap<usize, usize, ahash::RandomState>>,
}

impl RoutedMetaBackend {
    pub fn new(volumes: Vec<std::sync::Arc<MetaLvBackend>>) -> Self {
        Self {
            volumes,
            disabled_volumes: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            redirections: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
        }
    }

    pub fn check_volume_enabled(&self, idx: usize) -> Result<()> {
        if self.disabled_volumes.contains_key(&idx) {
            return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                format!("Metadata volume {} is disabled", idx),
            )));
        }
        Ok(())
    }

    pub async fn get_volume_health(&self, idx: usize) -> u32 {
        if self.disabled_volumes.contains_key(&idx) {
            return 0;
        }
        if idx >= self.volumes.len() {
            return 0;
        }
        // Health only used for dir placement; a short cache avoids scanning the
        // free-inode bitmap on every mkdir under multi-thread load.
        static HEALTH_CACHE: once_cell::sync::Lazy<
            scc::HashMap<usize, (u32, std::time::Instant)>,
        > = once_cell::sync::Lazy::new(scc::HashMap::new);
        if let Some(v) = HEALTH_CACHE.read_sync(&idx, |_, v| *v) {
            if v.1.elapsed() < std::time::Duration::from_millis(500) {
                return v.0;
            }
        }
        let vol = &self.volumes[idx];
        let allocated = vol.get_allocated_inode_count().await;
        let max_inodes = 20000;

        let free_factor = if max_inodes > allocated {
            (max_inodes - allocated) as f64 / max_inodes as f64
        } else {
            0.0
        };

        let score = (free_factor * 1000.0) as u32;
        let score = score.min(1000);
        let _ = HEALTH_CACHE.upsert_sync(idx, (score, std::time::Instant::now()));
        score
    }

    pub fn route_ino(&self, ino: Ino) -> (usize, Ino) {
        let num_volumes = self.volumes.len();
        if num_volumes <= 1 {
            return (0, ino);
        }
        if ino == 1 {
            return (0, 1);
        }
        let mut volume_idx = ((ino - 2) % num_volumes as u64) as usize;

        // Follow redirections
        while let Some(red) = self.redirections.get(&volume_idx) {
            volume_idx = *red;
        }

        let local_ino = ((ino - 2) / num_volumes as u64) + 2;
        (volume_idx, local_ino)
    }

    pub fn make_global_ino(&self, local_ino: Ino, volume_idx: usize) -> Ino {
        let num_volumes = self.volumes.len();
        if num_volumes <= 1 {
            return local_ino;
        }
        if local_ino == 1 && volume_idx == 0 {
            return 1;
        }
        (local_ino - 2) * num_volumes as u64 + volume_idx as u64 + 2
    }
}

#[async_trait::async_trait]
impl Metadata for RoutedMetaBackend {
    async fn lookup(&self, parent: Ino, name: &str) -> Result<Inode> {
        let (v_idx, local_parent) = self.route_ino(parent);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx]
            .dlm
            .lock_shared(&format!("D{}:{}", local_parent, name))
            .await;
        if let Some(dentry) =
            dentry::find_dentry(&self.volumes[v_idx].storage, local_parent, name).await?
        {
            self.getattr(dentry.child_ino).await
        } else {
            Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Dentry {} not found in parent {}", name, parent),
            )))
        }
    }

    async fn create(
        &self,
        parent: Ino,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Inode> {
        let (parent_v_idx, local_parent) = self.route_ino(parent);
        self.check_volume_enabled(parent_v_idx)?;
        let is_dir = (mode & libc::S_IFMT) == libc::S_IFDIR;
        let target_v_idx = if is_dir {
            let mut candidates = Vec::new();
            for (i, _) in self.volumes.iter().enumerate() {
                if self.disabled_volumes.contains_key(&i) {
                    continue;
                }
                let health = self.get_volume_health(i).await;
                candidates.push((i, health));
            }
            if candidates.is_empty() {
                parent_v_idx
            } else {
                candidates.sort_by(|a, b| b.1.cmp(&a.1));
                let max_health = candidates[0].1;
                let top_candidates: Vec<_> = candidates
                    .into_iter()
                    .filter(|c| c.1 >= (max_health * 9) / 10)
                    .collect();

                static META_COUNTER: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                let idx = META_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    % top_candidates.len();
                top_candidates[idx].0
            }
        } else {
            parent_v_idx
        };
        self.check_volume_enabled(target_v_idx)?;

        // Directories need exclusive parent lock (nlink). Regular files only need
        // the dentry name lock — parent exclusive serialized all same-dir creates
        // and dominated multi-thread small-file create+write.
        let _parent_guard = if is_dir {
            Some(
                self.volumes[parent_v_idx]
                    .dlm
                    .lock_exclusive(&format!("I{}", local_parent))
                    .await,
            )
        } else {
            None
        };
        let _dentry_guard = self.volumes[parent_v_idx]
            .dlm
            .lock_exclusive(&format!("D{}:{}", local_parent, name))
            .await;

        if parent_v_idx == target_v_idx {
            let backend = &self.volumes[target_v_idx];
            let is_dir_flag = is_dir;

            // Regular files: lock-only direct I/O (no journal transaction). Journal
            // run_transaction serializes all same-volume creates via transaction_lock
            // and was the ~2ms/op floor for small-file create+write. Sector safety is
            // still provided by inode_lock / dentry_lock RMW.
            // Directories keep the journaled path (parent nlink multi-field update).
            if !is_dir_flag {
                // Skip parent inode read (no SGID inheritance on this fast path).
                // Existence is enforced by the exclusive dentry name lock + insert.
                if dentry::find_dentry(&backend.storage, local_parent, name)
                    .await?
                    .is_some()
                {
                    return Err(crate::error::SqueezefsError::InvalidOperation(
                        "File already exists".to_string(),
                    ));
                }

                let final_mode = mode;
                let new_local_ino;
                let disk_inode;
                {
                    let _guard = backend.storage.inode_lock.lock().await;
                    new_local_ino = backend.storage.alloc_inode_bit_locked().await?;
                    let di = inode::DiskInode::new(new_local_ino, final_mode, uid, gid);
                    inode::write_inode_raw(&backend.storage, new_local_ino, &di).await?;
                    disk_inode = di;
                }

                let global_child_ino = self.make_global_ino(new_local_ino, target_v_idx);
                dentry::insert_dentry(
                    &backend.storage,
                    local_parent,
                    global_child_ino,
                    name,
                    final_mode & libc::S_IFMT,
                )
                .await?;

                return Ok(Inode {
                    ino: global_child_ino,
                    mode: disk_inode.mode,
                    uid: disk_inode.uid,
                    gid: disk_inode.gid,
                    size: disk_inode.size,
                    nlink: disk_inode.nlink,
                    atime: disk_inode.atime,
                    mtime: disk_inode.mtime,
                    ctime: disk_inode.ctime,
                    flags: disk_inode.flags,
                });
            }

            let inode = backend
                .run_transaction(|| async {
                if dentry::find_dentry(&backend.storage, local_parent, name)
                    .await?
                    .is_some()
                {
                    return Err(crate::error::SqueezefsError::InvalidOperation(
                        "File already exists".to_string(),
                    ));
                }

                let parent_inode =
                    inode::read_inode(&backend.storage, local_parent).await?;
                let mut final_gid = gid;
                let mut final_mode = mode;
                if (parent_inode.mode & libc::S_ISGID) != 0 {
                    final_gid = parent_inode.gid;
                    if (mode & libc::S_IFMT) == libc::S_IFDIR {
                        final_mode |= libc::S_ISGID;
                    }
                }

                let new_local_ino;
                let disk_inode;
                {
                    let _guard = backend.storage.inode_lock.lock().await;
                    new_local_ino = backend.storage.alloc_inode_bit_locked().await?;
                    let mut di =
                        inode::DiskInode::new(new_local_ino, final_mode, uid, final_gid);
                    di.nlink = 2;
                    inode::write_inode_raw(&backend.storage, new_local_ino, &di).await?;
                    disk_inode = di;
                }

                let global_child_ino = self.make_global_ino(new_local_ino, target_v_idx);

                dentry::insert_dentry(
                    &backend.storage,
                    local_parent,
                    global_child_ino,
                    name,
                    final_mode & libc::S_IFMT,
                )
                .await?;

                let mut parent_inode =
                    inode::read_inode(&backend.storage, local_parent).await?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                parent_inode.mtime = now;
                parent_inode.ctime = now;
                parent_inode.nlink += 1;
                inode::write_inode(&backend.storage, local_parent, &parent_inode).await?;

                Ok(Inode {
                    ino: global_child_ino,
                    mode: disk_inode.mode,
                    uid: disk_inode.uid,
                    gid: disk_inode.gid,
                    size: disk_inode.size,
                    nlink: disk_inode.nlink,
                    atime: disk_inode.atime,
                    mtime: disk_inode.mtime,
                    ctime: disk_inode.ctime,
                    flags: disk_inode.flags,
                })
            })
            .await?;
            Ok(inode)
        } else {
            // Cross-volume create: never mix unjournaled direct sector writes with
            // concurrent run_transaction applies on the same volume (stale sector
            // snapshots wipe sibling slots → magic 0 / ESTALE). Mutate each volume
            // only inside that volume's run_transaction (holds transaction_lock).
            if let Some(_) =
                dentry::find_dentry(&self.volumes[parent_v_idx].storage, local_parent, name).await?
            {
                return Err(crate::error::SqueezefsError::InvalidOperation(
                    "File already exists".to_string(),
                ));
            }

            let parent_inode =
                inode::read_inode(&self.volumes[parent_v_idx].storage, local_parent).await?;
            let mut final_gid = gid;
            let mut final_mode = mode;
            if (parent_inode.mode & libc::S_ISGID) != 0 {
                final_gid = parent_inode.gid;
                if (mode & libc::S_IFMT) == libc::S_IFDIR {
                    final_mode |= libc::S_ISGID;
                }
            }

            let target = self.volumes[target_v_idx].clone();
            let is_dir_flag = is_dir;
            let (new_local_ino, disk_inode) = target
                .run_transaction(|| async {
                    let _guard = target.storage.inode_lock.lock().await;
                    let new_local_ino = target.storage.alloc_inode_bit_locked().await?;
                    let mut disk_inode =
                        inode::DiskInode::new(new_local_ino, final_mode, uid, final_gid);
                    if is_dir_flag {
                        disk_inode.nlink = 2;
                    }
                    inode::write_inode_raw(&target.storage, new_local_ino, &disk_inode).await?;
                    Ok((new_local_ino, disk_inode))
                })
                .await?;

            let global_child_ino = self.make_global_ino(new_local_ino, target_v_idx);

            let parent_be = self.volumes[parent_v_idx].clone();
            let name_owned = name.to_string();
            parent_be
                .run_transaction(|| async {
                    dentry::insert_dentry(
                        &parent_be.storage,
                        local_parent,
                        global_child_ino,
                        &name_owned,
                        final_mode & libc::S_IFMT,
                    )
                    .await?;

                    if is_dir_flag {
                        let mut parent_inode =
                            inode::read_inode(&parent_be.storage, local_parent).await?;
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos() as u64;
                        parent_inode.mtime = now;
                        parent_inode.ctime = now;
                        parent_inode.nlink += 1;
                        inode::write_inode(&parent_be.storage, local_parent, &parent_inode)
                            .await?;
                    }
                    Ok(())
                })
                .await?;

            Ok(Inode {
                ino: global_child_ino,
                mode: disk_inode.mode,
                uid: disk_inode.uid,
                gid: disk_inode.gid,
                size: disk_inode.size,
                nlink: disk_inode.nlink,
                atime: disk_inode.atime,
                mtime: disk_inode.mtime,
                ctime: disk_inode.ctime,
                flags: disk_inode.flags,
            })
        }
    }

    async fn unlink(&self, parent: Ino, name: &str) -> Result<Ino> {
        let (parent_v_idx, local_parent) = self.route_ino(parent);
        self.check_volume_enabled(parent_v_idx)?;
        let _parent_guard = self.volumes[parent_v_idx]
            .dlm
            .lock_exclusive(&format!("I{}", local_parent))
            .await;
        let _dentry_guard = self.volumes[parent_v_idx]
            .dlm
            .lock_exclusive(&format!("D{}:{}", local_parent, name))
            .await;

        if let Some(dentry) =
            dentry::find_dentry(&self.volumes[parent_v_idx].storage, local_parent, name).await?
        {
            let global_child_ino = dentry.child_ino;
            let (child_v_idx, local_child) = self.route_ino(global_child_ino);
            self.check_volume_enabled(child_v_idx)?;

            let _inode_guard = if parent != global_child_ino {
                Some(
                    self.volumes[child_v_idx]
                        .dlm
                        .lock_exclusive(&format!("I{}", local_child))
                        .await,
                )
            } else {
                None
            };

            if parent_v_idx == child_v_idx {
                let backend = &self.volumes[parent_v_idx];
                backend.run_transaction(|| async {
                    dentry::remove_dentry(&backend.storage, local_parent, name).await?;

                    let is_dir = dentry.file_type == libc::S_IFDIR;

                    // Update parent directory times
                    let mut parent_inode =
                        inode::read_inode(&backend.storage, local_parent).await?;
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos() as u64;
                    parent_inode.mtime = now;
                    parent_inode.ctime = now;
                    let old_nlink = parent_inode.nlink;
                    if is_dir {
                        if parent_inode.nlink > 2 {
                            parent_inode.nlink -= 1;
                        }
                    }
                    log::debug!(
                        "meta_backend unlink parent={}: name={}, child={}, file_type={:o}, is_dir={}, parent_nlink_before={}, parent_nlink_after={}",
                        local_parent, name, global_child_ino, dentry.file_type, is_dir, old_nlink, parent_inode.nlink
                    );
                    inode::write_inode(
                        &backend.storage,
                        local_parent,
                        &parent_inode,
                    )
                    .await?;

                    let mut disk_inode =
                        inode::read_inode(&backend.storage, local_child).await?;
                    log::debug!(
                        "meta_backend unlink: local_child = {}, nlink = {}",
                        local_child,
                        disk_inode.nlink
                    );
                    if is_dir {
                        disk_inode.nlink = 0;
                    } else if disk_inode.nlink > 0 {
                        disk_inode.nlink -= 1;
                    }
                    disk_inode.ctime = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos() as u64;
                    log::debug!(
                        "meta_backend unlink: local_child = {}, writing nlink = {}",
                        local_child,
                        disk_inode.nlink
                    );
                    inode::write_inode(&backend.storage, local_child, &disk_inode)
                        .await?;
                    Ok(global_child_ino)
                }).await
            } else {
                dentry::remove_dentry(&self.volumes[parent_v_idx].storage, local_parent, name)
                    .await?;

                let is_dir = dentry.file_type == libc::S_IFDIR;

                // Update parent directory times
                let mut parent_inode =
                    inode::read_inode(&self.volumes[parent_v_idx].storage, local_parent).await?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                parent_inode.mtime = now;
                parent_inode.ctime = now;
                let old_nlink = parent_inode.nlink;
                if is_dir {
                    if parent_inode.nlink > 2 {
                        parent_inode.nlink -= 1;
                    }
                }
                log::debug!(
                    "meta_backend unlink parent={}: name={}, child={}, file_type={:o}, is_dir={}, parent_nlink_before={}, parent_nlink_after={}",
                    local_parent, name, global_child_ino, dentry.file_type, is_dir, old_nlink, parent_inode.nlink
                );
                inode::write_inode(
                    &self.volumes[parent_v_idx].storage,
                    local_parent,
                    &parent_inode,
                )
                .await?;

                let mut disk_inode =
                    inode::read_inode(&self.volumes[child_v_idx].storage, local_child).await?;
                log::debug!(
                    "meta_backend unlink: local_child = {}, nlink = {}",
                    local_child,
                    disk_inode.nlink
                );
                if is_dir {
                    disk_inode.nlink = 0;
                } else if disk_inode.nlink > 0 {
                    disk_inode.nlink -= 1;
                }
                disk_inode.ctime = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                log::debug!(
                    "meta_backend unlink: local_child = {}, writing nlink = {}",
                    local_child,
                    disk_inode.nlink
                );
                inode::write_inode(&self.volumes[child_v_idx].storage, local_child, &disk_inode)
                    .await?;
                Ok(global_child_ino)
            }
        } else {
            Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Dentry not found",
            )))
        }
    }

    async fn link(&self, ino: Ino, new_parent: Ino, new_name: &str) -> Result<Inode> {
        let (parent_v_idx, local_parent) = self.route_ino(new_parent);
        let (child_v_idx, local_child) = self.route_ino(ino);
        self.check_volume_enabled(parent_v_idx)?;
        self.check_volume_enabled(child_v_idx)?;

        let _parent_guard = self.volumes[parent_v_idx]
            .dlm
            .lock_exclusive(&format!("I{}", local_parent))
            .await;
        let _dentry_guard = self.volumes[parent_v_idx]
            .dlm
            .lock_exclusive(&format!("D{}:{}", local_parent, new_name))
            .await;

        if let Some(_) =
            dentry::find_dentry(&self.volumes[parent_v_idx].storage, local_parent, new_name).await?
        {
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "File already exists".to_string(),
            ));
        }

        let _inode_guard = self.volumes[child_v_idx]
            .dlm
            .lock_exclusive(&format!("I{}", local_child))
            .await;
        if parent_v_idx == child_v_idx {
            let backend = &self.volumes[parent_v_idx];
            backend
                .run_transaction(|| async {
                    let mut disk_inode = inode::read_inode(&backend.storage, local_child).await?;
                    if disk_inode.nlink >= 65000 {
                        return Err(crate::error::SqueezefsError::InvalidOperation(
                            "Too many links".to_string(),
                        ));
                    }
                    log::debug!(
                        "meta_backend link: local_child = {}, nlink before = {}",
                        local_child,
                        disk_inode.nlink
                    );
                    disk_inode.nlink += 1;
                    disk_inode.ctime = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos() as u64;
                    log::debug!(
                        "meta_backend link: local_child = {}, nlink after = {}",
                        local_child,
                        disk_inode.nlink
                    );
                    inode::write_inode(&backend.storage, local_child, &disk_inode).await?;

                    dentry::insert_dentry(
                        &backend.storage,
                        local_parent,
                        ino,
                        new_name,
                        disk_inode.mode & libc::S_IFMT,
                    )
                    .await?;

                    // Update parent directory times
                    if let Ok(mut parent_inode) =
                        inode::read_inode(&backend.storage, local_parent).await
                    {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos() as u64;
                        parent_inode.mtime = now;
                        parent_inode.ctime = now;
                        let _ =
                            inode::write_inode(&backend.storage, local_parent, &parent_inode).await;
                    }

                    Ok(Inode {
                        ino,
                        mode: disk_inode.mode,
                        uid: disk_inode.uid,
                        gid: disk_inode.gid,
                        size: disk_inode.size,
                        nlink: disk_inode.nlink,
                        atime: disk_inode.atime,
                        mtime: disk_inode.mtime,
                        ctime: disk_inode.ctime,
                        flags: disk_inode.flags,
                    })
                })
                .await
        } else {
            let mut disk_inode =
                inode::read_inode(&self.volumes[child_v_idx].storage, local_child).await?;
            if disk_inode.nlink >= 65000 {
                return Err(crate::error::SqueezefsError::InvalidOperation(
                    "Too many links".to_string(),
                ));
            }
            log::debug!(
                "meta_backend link: local_child = {}, nlink before = {}",
                local_child,
                disk_inode.nlink
            );
            disk_inode.nlink += 1;
            disk_inode.ctime = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            log::debug!(
                "meta_backend link: local_child = {}, nlink after = {}",
                local_child,
                disk_inode.nlink
            );
            inode::write_inode(&self.volumes[child_v_idx].storage, local_child, &disk_inode)
                .await?;

            dentry::insert_dentry(
                &self.volumes[parent_v_idx].storage,
                local_parent,
                ino,
                new_name,
                disk_inode.mode & libc::S_IFMT,
            )
            .await?;

            // Update parent directory times
            if let Ok(mut parent_inode) =
                inode::read_inode(&self.volumes[parent_v_idx].storage, local_parent).await
            {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                parent_inode.mtime = now;
                parent_inode.ctime = now;
                let _ = inode::write_inode(
                    &self.volumes[parent_v_idx].storage,
                    local_parent,
                    &parent_inode,
                )
                .await;
            }

            Ok(Inode {
                ino,
                mode: disk_inode.mode,
                uid: disk_inode.uid,
                gid: disk_inode.gid,
                size: disk_inode.size,
                nlink: disk_inode.nlink,
                atime: disk_inode.atime,
                mtime: disk_inode.mtime,
                ctime: disk_inode.ctime,
                flags: disk_inode.flags,
            })
        }
    }

    async fn rename(
        &self,
        old_parent: Ino,
        old_name: &str,
        new_parent: Ino,
        new_name: &str,
        flags: u32,
    ) -> Result<()> {
        if flags & (libc::RENAME_NOREPLACE | libc::RENAME_EXCHANGE)
            == (libc::RENAME_NOREPLACE | libc::RENAME_EXCHANGE)
        {
            return Err(crate::error::SqueezefsError::Io(
                std::io::Error::from_raw_os_error(libc::EINVAL),
            ));
        }

        let (old_parent_v_idx, local_old_parent) = self.route_ino(old_parent);
        let (new_parent_v_idx, local_new_parent) = self.route_ino(new_parent);
        self.check_volume_enabled(old_parent_v_idx)?;
        self.check_volume_enabled(new_parent_v_idx)?;

        let mut parent_lock_keys = vec![
            (old_parent_v_idx, local_old_parent, old_parent),
            (new_parent_v_idx, local_new_parent, new_parent),
        ];
        parent_lock_keys.sort_unstable_by_key(|&(v, l, _)| (v, l));
        parent_lock_keys.dedup_by_key(|&mut (_, _, orig)| orig);

        let mut _parent_guards = Vec::new();
        for (v_idx, local_p, _) in parent_lock_keys {
            _parent_guards.push(
                self.volumes[v_idx]
                    .dlm
                    .lock_exclusive(&format!("I{}", local_p))
                    .await,
            );
        }

        let old_dentry_key = format!("D{}:{}", local_old_parent, old_name);
        let new_dentry_key = format!("D{}:{}", local_new_parent, new_name);

        let mut dentry_lock_keys = vec![
            (old_parent_v_idx, old_dentry_key.clone()),
            (new_parent_v_idx, new_dentry_key.clone()),
        ];
        dentry_lock_keys.sort_unstable_by(|a, b| match a.0.cmp(&b.0) {
            std::cmp::Ordering::Equal => a.1.cmp(&b.1),
            other => other,
        });
        dentry_lock_keys.dedup();

        let mut _dentry_guards = Vec::new();
        for (v_idx, d_key) in dentry_lock_keys {
            _dentry_guards.push(self.volumes[v_idx].dlm.lock_exclusive(&d_key).await);
        }

        let old_dentry_opt = dentry::find_dentry(
            &self.volumes[old_parent_v_idx].storage,
            local_old_parent,
            old_name,
        )
        .await?;

        let new_dentry_opt = dentry::find_dentry(
            &self.volumes[new_parent_v_idx].storage,
            local_new_parent,
            new_name,
        )
        .await?;

        if old_parent_v_idx == new_parent_v_idx {
            let backend = &self.volumes[old_parent_v_idx];
            backend
                .run_transaction(|| async {
                    if flags & libc::RENAME_EXCHANGE != 0 {
                        let old_dentry = old_dentry_opt.ok_or_else(|| {
                            crate::error::SqueezefsError::Io(std::io::Error::from_raw_os_error(
                                libc::ENOENT,
                            ))
                        })?;
                        let new_dentry = new_dentry_opt.ok_or_else(|| {
                            crate::error::SqueezefsError::Io(std::io::Error::from_raw_os_error(
                                libc::ENOENT,
                            ))
                        })?;

                        // Remove both
                        dentry::remove_dentry(&backend.storage, local_old_parent, old_name).await?;
                        dentry::remove_dentry(&backend.storage, local_new_parent, new_name).await?;

                        // Insert swapped
                        dentry::insert_dentry(
                            &backend.storage,
                            local_new_parent,
                            old_dentry.child_ino,
                            new_name,
                            old_dentry.file_type,
                        )
                        .await?;

                        dentry::insert_dentry(
                            &backend.storage,
                            local_old_parent,
                            new_dentry.child_ino,
                            old_name,
                            new_dentry.file_type,
                        )
                        .await?;

                        Ok(())
                    } else {
                        if flags & libc::RENAME_NOREPLACE != 0 && new_dentry_opt.is_some() {
                            return Err(crate::error::SqueezefsError::Io(
                                std::io::Error::from_raw_os_error(libc::EEXIST),
                            ));
                        }

                        if let Some(dentry) = old_dentry_opt {
                            let is_dir = dentry.file_type == libc::S_IFDIR;
                            let cross_dir = old_parent_v_idx != new_parent_v_idx
                                || local_old_parent != local_new_parent;

                            if is_dir && cross_dir {
                                // Decrement old parent link count
                                if let Ok(mut old_p_inode) =
                                    inode::read_inode(&backend.storage, local_old_parent).await
                                {
                                    if old_p_inode.nlink > 2 {
                                        old_p_inode.nlink -= 1;
                                    }
                                    let _ = inode::write_inode(
                                        &backend.storage,
                                        local_old_parent,
                                        &old_p_inode,
                                    )
                                    .await;
                                }
                                // Increment new parent link count
                                if let Ok(mut new_p_inode) =
                                    inode::read_inode(&backend.storage, local_new_parent).await
                                {
                                    new_p_inode.nlink += 1;
                                    let _ = inode::write_inode(
                                        &backend.storage,
                                        local_new_parent,
                                        &new_p_inode,
                                    )
                                    .await;
                                }
                            }

                            // Check if destination already exists to decrement its link count
                            if let Some(dest_dentry) = new_dentry_opt {
                                let dest_ino = dest_dentry.child_ino;
                                let (dest_v_idx, local_dest) = self.route_ino(dest_ino);
                                if let Ok(mut dest_inode) =
                                    inode::read_inode(&self.volumes[dest_v_idx].storage, local_dest)
                                        .await
                                {
                                    // Check if destination is a directory and is not empty
                                    if (dest_inode.mode & libc::S_IFMT) == libc::S_IFDIR {
                                        let dentries = dentry::list_dentries(
                                            &self.volumes[dest_v_idx].storage,
                                            local_dest,
                                        )
                                        .await?;
                                        if !dentries.is_empty() {
                                            return Err(crate::error::SqueezefsError::Io(
                                                std::io::Error::from_raw_os_error(libc::ENOTEMPTY),
                                            ));
                                        }
                                    }

                                    if dest_inode.nlink > 0 {
                                        dest_inode.nlink -= 1;
                                    }
                                    dest_inode.ctime = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_nanos()
                                        as u64;
                                    let _ = inode::write_inode(
                                        &self.volumes[dest_v_idx].storage,
                                        local_dest,
                                        &dest_inode,
                                    )
                                    .await;
                                }
                                // Remove the destination dentry so it gets replaced cleanly
                                dentry::remove_dentry(&backend.storage, local_new_parent, new_name)
                                    .await?;
                            }

                            dentry::remove_dentry(&backend.storage, local_old_parent, old_name)
                                .await?;
                            dentry::insert_dentry(
                                &backend.storage,
                                local_new_parent,
                                dentry.child_ino,
                                new_name,
                                dentry.file_type,
                            )
                            .await?;
                            Ok(())
                        } else {
                            Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                                std::io::ErrorKind::NotFound,
                                "Source dentry not found",
                            )))
                        }
                    }
                })
                .await
        } else {
            if flags & libc::RENAME_EXCHANGE != 0 {
                let old_dentry = old_dentry_opt.ok_or_else(|| {
                    crate::error::SqueezefsError::Io(std::io::Error::from_raw_os_error(
                        libc::ENOENT,
                    ))
                })?;
                let new_dentry = new_dentry_opt.ok_or_else(|| {
                    crate::error::SqueezefsError::Io(std::io::Error::from_raw_os_error(
                        libc::ENOENT,
                    ))
                })?;

                // Remove both
                dentry::remove_dentry(
                    &self.volumes[old_parent_v_idx].storage,
                    local_old_parent,
                    old_name,
                )
                .await?;
                dentry::remove_dentry(
                    &self.volumes[new_parent_v_idx].storage,
                    local_new_parent,
                    new_name,
                )
                .await?;

                // Insert swapped
                dentry::insert_dentry(
                    &self.volumes[new_parent_v_idx].storage,
                    local_new_parent,
                    old_dentry.child_ino,
                    new_name,
                    old_dentry.file_type,
                )
                .await?;

                dentry::insert_dentry(
                    &self.volumes[old_parent_v_idx].storage,
                    local_old_parent,
                    new_dentry.child_ino,
                    old_name,
                    new_dentry.file_type,
                )
                .await?;

                Ok(())
            } else {
                if flags & libc::RENAME_NOREPLACE != 0 && new_dentry_opt.is_some() {
                    return Err(crate::error::SqueezefsError::Io(
                        std::io::Error::from_raw_os_error(libc::EEXIST),
                    ));
                }

                if let Some(dentry) = old_dentry_opt {
                    let is_dir = dentry.file_type == libc::S_IFDIR;
                    let cross_dir = old_parent_v_idx != new_parent_v_idx
                        || local_old_parent != local_new_parent;

                    if is_dir && cross_dir {
                        // Decrement old parent link count
                        if let Ok(mut old_p_inode) = inode::read_inode(
                            &self.volumes[old_parent_v_idx].storage,
                            local_old_parent,
                        )
                        .await
                        {
                            if old_p_inode.nlink > 2 {
                                old_p_inode.nlink -= 1;
                            }
                            let _ = inode::write_inode(
                                &self.volumes[old_parent_v_idx].storage,
                                local_old_parent,
                                &old_p_inode,
                            )
                            .await;
                        }
                        // Increment new parent link count
                        if let Ok(mut new_p_inode) = inode::read_inode(
                            &self.volumes[new_parent_v_idx].storage,
                            local_new_parent,
                        )
                        .await
                        {
                            new_p_inode.nlink += 1;
                            let _ = inode::write_inode(
                                &self.volumes[new_parent_v_idx].storage,
                                local_new_parent,
                                &new_p_inode,
                            )
                            .await;
                        }
                    }

                    // Check if destination already exists to decrement its link count
                    if let Some(dest_dentry) = new_dentry_opt {
                        let dest_ino = dest_dentry.child_ino;
                        let (dest_v_idx, local_dest) = self.route_ino(dest_ino);
                        if let Ok(mut dest_inode) =
                            inode::read_inode(&self.volumes[dest_v_idx].storage, local_dest).await
                        {
                            // Check if destination is a directory and is not empty
                            if (dest_inode.mode & libc::S_IFMT) == libc::S_IFDIR {
                                let dentries = dentry::list_dentries(
                                    &self.volumes[dest_v_idx].storage,
                                    local_dest,
                                )
                                .await?;
                                if !dentries.is_empty() {
                                    return Err(crate::error::SqueezefsError::Io(
                                        std::io::Error::from_raw_os_error(libc::ENOTEMPTY),
                                    ));
                                }
                            }

                            if dest_inode.nlink > 0 {
                                dest_inode.nlink -= 1;
                            }
                            dest_inode.ctime = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_nanos() as u64;
                            let _ = inode::write_inode(
                                &self.volumes[dest_v_idx].storage,
                                local_dest,
                                &dest_inode,
                            )
                            .await;
                        }
                        // Remove the destination dentry so it gets replaced cleanly
                        dentry::remove_dentry(
                            &self.volumes[new_parent_v_idx].storage,
                            local_new_parent,
                            new_name,
                        )
                        .await?;
                    }

                    dentry::remove_dentry(
                        &self.volumes[old_parent_v_idx].storage,
                        local_old_parent,
                        old_name,
                    )
                    .await?;
                    dentry::insert_dentry(
                        &self.volumes[new_parent_v_idx].storage,
                        local_new_parent,
                        dentry.child_ino,
                        new_name,
                        dentry.file_type,
                    )
                    .await?;
                    Ok(())
                } else {
                    Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "Source dentry not found",
                    )))
                }
            }
        }
    }

    async fn readdir(&self, dir: Ino, _offset: u64, _max: usize) -> Result<Vec<DirEntry>> {
        let (v_idx, local_dir) = self.route_ino(dir);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx]
            .dlm
            .lock_shared(&format!("I{}", local_dir))
            .await;
        let dentries = dentry::list_dentries(&self.volumes[v_idx].storage, local_dir).await?;
        let mut list = Vec::new();
        for d in dentries {
            list.push(DirEntry {
                ino: d.child_ino,
                name: d.get_name(),
                file_type: d.file_type,
            });
        }
        Ok(list)
    }

    async fn getattr(&self, ino: Ino) -> Result<Inode> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx]
            .dlm
            .lock_shared(&format!("I{}", local_ino))
            .await;
        let disk_inode = inode::read_inode(&self.volumes[v_idx].storage, local_ino).await?;
        Ok(Inode {
            ino: ino,
            mode: disk_inode.mode,
            uid: disk_inode.uid,
            gid: disk_inode.gid,
            size: disk_inode.size,
            nlink: disk_inode.nlink,
            atime: disk_inode.atime,
            mtime: disk_inode.mtime,
            ctime: disk_inode.ctime,
            flags: disk_inode.flags,
        })
    }

    async fn setattr(
        &self,
        ino: Ino,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<u64>,
        mtime: Option<u64>,
        ctime: Option<u64>,
    ) -> Result<Inode> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx]
            .dlm
            .lock_exclusive(&format!("I{}", local_ino))
            .await;
        let mut disk_inode = inode::read_inode(&self.volumes[v_idx].storage, local_ino).await?;
        let mut ctime_updated = false;
        if let Some(m) = mode {
            disk_inode.mode = m;
            ctime_updated = true;
        }
        if let Some(u) = uid {
            disk_inode.uid = u;
            ctime_updated = true;
        }
        if let Some(g) = gid {
            disk_inode.gid = g;
            ctime_updated = true;
        }
        if let Some(s) = size {
            disk_inode.size = s;
            ctime_updated = true;
        }
        if let Some(a) = atime {
            disk_inode.atime = a;
        }
        if let Some(m) = mtime {
            disk_inode.mtime = m;
        }
        if let Some(c) = ctime {
            disk_inode.ctime = c;
        } else if ctime_updated {
            disk_inode.ctime = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
        }
        inode::write_inode(&self.volumes[v_idx].storage, local_ino, &disk_inode).await?;
        Ok(Inode {
            ino: ino,
            mode: disk_inode.mode,
            uid: disk_inode.uid,
            gid: disk_inode.gid,
            size: disk_inode.size,
            nlink: disk_inode.nlink,
            atime: disk_inode.atime,
            mtime: disk_inode.mtime,
            ctime: disk_inode.ctime,
            flags: disk_inode.flags,
        })
    }

    async fn getxattr(&self, ino: Ino, name: &str) -> Result<Option<Vec<u8>>> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx]
            .dlm
            .lock_shared(&format!("I{}", local_ino))
            .await;
        xattr::get_xattr(&self.volumes[v_idx].storage, local_ino, name).await
    }

    async fn setxattr(&self, ino: Ino, name: &str, value: &[u8]) -> Result<()> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx]
            .dlm
            .lock_exclusive(&format!("I{}", local_ino))
            .await;
        xattr::set_xattr(&self.volumes[v_idx].storage, local_ino, name, value).await
    }

    async fn removexattr(&self, ino: Ino, name: &str) -> Result<()> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx]
            .dlm
            .lock_exclusive(&format!("I{}", local_ino))
            .await;
        xattr::remove_xattr(&self.volumes[v_idx].storage, local_ino, name).await
    }

    async fn listxattr(&self, ino: Ino) -> Result<Vec<String>> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx]
            .dlm
            .lock_shared(&format!("I{}", local_ino))
            .await;
        xattr::list_xattrs(&self.volumes[v_idx].storage, local_ino).await
    }

    async fn destroy_inode(&self, ino: Ino) -> Result<()> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        self.volumes[v_idx].destroy_inode(local_ino).await
    }
}

impl RoutedMetaBackend {
    pub async fn sync_all_devices(&self) -> Result<()> {
        for vol in &self.volumes {
            crate::uring_fs::fdatasync(vol.storage.device_path()).await?;
        }
        Ok(())
    }

    /// fdatasync only the MetaLV volume that owns `ino` (avoid multi-volume fsync tax).
    pub async fn sync_device_for_ino(&self, ino: Ino) -> Result<()> {
        let (v_idx, _) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        crate::uring_fs::fdatasync(self.volumes[v_idx].storage.device_path()).await
    }

    /// Persist layout xattr + size with fine locks (no journal transaction_lock).
    /// Used on fsync/release writeback; avoids serializing all layout commits.
    pub async fn set_layout_and_size(&self, ino: Ino, layout: &[u8], size: u64) -> Result<()> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let be = &self.volumes[v_idx];
        let _guard = be.dlm.lock_exclusive(&format!("I{}", local_ino)).await;
        xattr::set_xattr(&be.storage, local_ino, "layout", layout).await?;
        let mut disk_inode = inode::read_inode(&be.storage, local_ino).await?;
        disk_inode.size = size;
        disk_inode.ctime = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        inode::write_inode(&be.storage, local_ino, &disk_inode).await?;
        Ok(())
    }
}
