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
    async fn unlink(&self, parent: Ino, name: &str) -> Result<()>;
    async fn link(&self, ino: Ino, new_parent: Ino, new_name: &str) -> Result<Inode>;
    async fn rename(
        &self,
        old_parent: Ino,
        old_name: &str,
        new_parent: Ino,
        new_name: &str,
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
        let journal = journal::Journal::new(1024 * 1024 * 16, 1024 * 1024 * 4); // 16MB offset, 4MB size
        Self {
            storage,
            dlm: dlm::DlmLockManager::new(),
            journal,
        }
    }

    pub fn get_allocated_inode_count(&self) -> usize {
        let mut count = 0;
        for i in 2..20000 {
            if inode::read_inode(&self.storage, i).is_ok() {
                count += 1;
            }
        }
        count
    }

    /// Formats the raw block storage device with a superblock and the root inode
    pub fn format(storage: &storage::MetaLvStorage) -> Result<()> {
        // Zero-wipe the entire metadata volume first to prevent stale garbage issues
        storage.wipe()?;

        // Initialize Superblock
        let sb = storage::Superblock {
            magic: *storage::MAGIC_VALUE,
            version: 1,
            inode_count: 1000000,
            free_inode_bitmap_root: 4096 * 2,
            dentry_root: dentry::DENTRY_TABLE_START,
            journal_start: 1024 * 1024 * 16,
            journal_size: 1024 * 1024 * 4,
            checksum: 0,
        };
        storage.write_superblock(&sb)?;

        // Format root Inode (ino 1)
        let root_inode = inode::DiskInode::new(1, libc::S_IFDIR | 0o755, 0, 0);
        inode::write_inode(storage, 1, &root_inode)?;

        Ok(())
    }
}

#[async_trait::async_trait]
impl Metadata for MetaLvBackend {
    async fn lookup(&self, parent: Ino, name: &str) -> Result<Inode> {
        let _guard = self.dlm.lock_shared(&format!("D{}:{}", parent, name)).await;
        if let Some(dentry) = dentry::find_dentry(&self.storage, parent, name)? {
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

        if let Some(_) = dentry::find_dentry(&self.storage, parent, name)? {
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "File already exists".to_string(),
            ));
        }

        let _op_guard = self.storage.lock_op();
        // Allocate a new inode by scanning table (basic allocator for Phase 0)
        let mut new_ino = 0;
        for i in 2..20000 {
            if let Err(_) = inode::read_inode(&self.storage, i) {
                // Inode slot is uninitialized/magic invalid -> we can use it!
                new_ino = i;
                break;
            }
        }
        if new_ino == 0 {
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "Inode table full".to_string(),
            ));
        }

        let disk_inode = inode::DiskInode::new(new_ino, mode, uid, gid);
        inode::write_inode_raw(&self.storage, new_ino, &disk_inode)?;
        std::mem::drop(_op_guard);

        dentry::insert_dentry(&self.storage, parent, new_ino, name, mode & libc::S_IFMT)?;

        // Update parent directory times
        if let Ok(mut parent_inode) = inode::read_inode(&self.storage, parent) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            parent_inode.mtime = now;
            parent_inode.ctime = now;
            let _ = inode::write_inode(&self.storage, parent, &parent_inode);
        }

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
    }

    async fn unlink(&self, parent: Ino, name: &str) -> Result<()> {
        let _parent_guard = self.dlm.lock_exclusive(&format!("I{}", parent)).await;
        let _dentry_guard = self
            .dlm
            .lock_exclusive(&format!("D{}:{}", parent, name))
            .await;

        if let Some(dentry) = dentry::find_dentry(&self.storage, parent, name)? {
            let ino = dentry.child_ino;
            let _inode_guard = if parent != ino {
                Some(self.dlm.lock_exclusive(&format!("I{}", ino)).await)
            } else {
                None
            };

            dentry::remove_dentry(&self.storage, parent, name)?;

            // Update parent directory times
            if let Ok(mut parent_inode) = inode::read_inode(&self.storage, parent) {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                parent_inode.mtime = now;
                parent_inode.ctime = now;
                let _ = inode::write_inode(&self.storage, parent, &parent_inode);
            }

            // Decrement nlink
            let mut disk_inode = inode::read_inode(&self.storage, ino)?;
            if disk_inode.nlink > 0 {
                disk_inode.nlink -= 1;
            }
            disk_inode.ctime = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            inode::write_inode(&self.storage, ino, &disk_inode)?;
            Ok(())
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

        if let Some(_) = dentry::find_dentry(&self.storage, new_parent, new_name)? {
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "File already exists".to_string(),
            ));
        }

        let _inode_guard = self.dlm.lock_exclusive(&format!("I{}", ino)).await;
        let mut disk_inode = inode::read_inode(&self.storage, ino)?;
        disk_inode.nlink += 1;
        disk_inode.ctime = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        inode::write_inode(&self.storage, ino, &disk_inode)?;

        dentry::insert_dentry(
            &self.storage,
            new_parent,
            ino,
            new_name,
            disk_inode.mode & libc::S_IFMT,
        )?;

        // Update parent directory times
        if let Ok(mut parent_inode) = inode::read_inode(&self.storage, new_parent) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            parent_inode.mtime = now;
            parent_inode.ctime = now;
            let _ = inode::write_inode(&self.storage, new_parent, &parent_inode);
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

    async fn rename(
        &self,
        old_parent: Ino,
        old_name: &str,
        new_parent: Ino,
        new_name: &str,
    ) -> Result<()> {
        let _old_pg = self.dlm.lock_exclusive(&format!("I{}", old_parent)).await;
        let _new_pg = if old_parent != new_parent {
            Some(self.dlm.lock_exclusive(&format!("I{}", new_parent)).await)
        } else {
            None
        };

        let old_dentry_key = format!("D{}:{}", old_parent, old_name);
        let new_dentry_key = format!("D{}:{}", new_parent, new_name);
        let _old_dg = self.dlm.lock_exclusive(&old_dentry_key).await;
        let _new_dg = if old_dentry_key != new_dentry_key {
            Some(self.dlm.lock_exclusive(&new_dentry_key).await)
        } else {
            None
        };

        if let Some(dentry) = dentry::find_dentry(&self.storage, old_parent, old_name)? {
            dentry::remove_dentry(&self.storage, old_parent, old_name)?;
            dentry::insert_dentry(
                &self.storage,
                new_parent,
                dentry.child_ino,
                new_name,
                dentry.file_type,
            )?;
            Ok(())
        } else {
            Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Source dentry not found",
            )))
        }
    }

    async fn readdir(&self, dir: Ino, _offset: u64, _max: usize) -> Result<Vec<DirEntry>> {
        let _guard = self.dlm.lock_shared(&format!("I{}", dir)).await;
        let dentries = dentry::list_dentries(&self.storage, dir)?;
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
        let disk_inode = inode::read_inode(&self.storage, ino)?;
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
        let mut disk_inode = inode::read_inode(&self.storage, ino)?;
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
        inode::write_inode(&self.storage, ino, &disk_inode)?;
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
        xattr::get_xattr(&self.storage, ino, name)
    }

    async fn setxattr(&self, ino: Ino, name: &str, value: &[u8]) -> Result<()> {
        let _guard = self.dlm.lock_exclusive(&format!("I{}", ino)).await;
        xattr::set_xattr(&self.storage, ino, name, value)
    }

    async fn removexattr(&self, ino: Ino, name: &str) -> Result<()> {
        let _guard = self.dlm.lock_exclusive(&format!("I{}", ino)).await;
        xattr::remove_xattr(&self.storage, ino, name)
    }

    async fn listxattr(&self, ino: Ino) -> Result<Vec<String>> {
        let _guard = self.dlm.lock_shared(&format!("I{}", ino)).await;
        xattr::list_xattrs(&self.storage, ino)
    }

    async fn destroy_inode(&self, ino: Ino) -> Result<()> {
        let _guard = self.dlm.lock_exclusive(&format!("I{}", ino)).await;
        let empty = inode::DiskInode::new_zeroed();
        inode::write_inode(&self.storage, ino, &empty)?;
        Ok(())
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

    pub fn get_volume_health(&self, idx: usize) -> u32 {
        if self.disabled_volumes.contains_key(&idx) {
            return 0;
        }
        if idx >= self.volumes.len() {
            return 0;
        }
        let vol = &self.volumes[idx];
        let allocated = vol.get_allocated_inode_count();
        let max_inodes = 20000;

        let free_factor = if max_inodes > allocated {
            (max_inodes - allocated) as f64 / max_inodes as f64
        } else {
            0.0
        };

        let score = (free_factor * 1000.0) as u32;
        score.min(1000)
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
        if let Some(dentry) = dentry::find_dentry(&self.volumes[v_idx].storage, local_parent, name)?
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
                let health = self.get_volume_health(i);
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

        let _parent_guard = self.volumes[parent_v_idx]
            .dlm
            .lock_exclusive(&format!("I{}", local_parent))
            .await;
        let _dentry_guard = self.volumes[parent_v_idx]
            .dlm
            .lock_exclusive(&format!("D{}:{}", local_parent, name))
            .await;

        if let Some(_) =
            dentry::find_dentry(&self.volumes[parent_v_idx].storage, local_parent, name)?
        {
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "File already exists".to_string(),
            ));
        }

        let _op_guard = self.volumes[target_v_idx].storage.lock_op();
        let mut new_local_ino = 0;
        for i in 2..20000 {
            if let Err(_) = inode::read_inode(&self.volumes[target_v_idx].storage, i) {
                new_local_ino = i;
                break;
            }
        }
        if new_local_ino == 0 {
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "Inode table full".to_string(),
            ));
        }

        let mut disk_inode = inode::DiskInode::new(new_local_ino, mode, uid, gid);
        if is_dir {
            disk_inode.nlink = 2;
        }
        inode::write_inode_raw(
            &self.volumes[target_v_idx].storage,
            new_local_ino,
            &disk_inode,
        )?;
        std::mem::drop(_op_guard);

        let global_child_ino = self.make_global_ino(new_local_ino, target_v_idx);

        dentry::insert_dentry(
            &self.volumes[parent_v_idx].storage,
            local_parent,
            global_child_ino,
            name,
            mode & libc::S_IFMT,
        )?;

        // Update parent directory times
        if let Ok(mut parent_inode) =
            inode::read_inode(&self.volumes[parent_v_idx].storage, local_parent)
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            parent_inode.mtime = now;
            parent_inode.ctime = now;
            if is_dir {
                parent_inode.nlink += 1;
            }
            let _ = inode::write_inode(
                &self.volumes[parent_v_idx].storage,
                local_parent,
                &parent_inode,
            );
        }

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

    async fn unlink(&self, parent: Ino, name: &str) -> Result<()> {
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
            dentry::find_dentry(&self.volumes[parent_v_idx].storage, local_parent, name)?
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

            dentry::remove_dentry(&self.volumes[parent_v_idx].storage, local_parent, name)?;

            let is_dir = dentry.file_type == libc::S_IFDIR;

            // Update parent directory times
            if let Ok(mut parent_inode) =
                inode::read_inode(&self.volumes[parent_v_idx].storage, local_parent)
            {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                parent_inode.mtime = now;
                parent_inode.ctime = now;
                if is_dir {
                    if parent_inode.nlink > 2 {
                        parent_inode.nlink -= 1;
                    }
                }
                let _ = inode::write_inode(
                    &self.volumes[parent_v_idx].storage,
                    local_parent,
                    &parent_inode,
                );
            }

            let mut disk_inode =
                inode::read_inode(&self.volumes[child_v_idx].storage, local_child)?;
            log::debug!(
                "meta_backend unlink: local_child = {}, nlink = {}",
                local_child,
                disk_inode.nlink
            );
            if disk_inode.nlink > 0 {
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
            inode::write_inode(&self.volumes[child_v_idx].storage, local_child, &disk_inode)?;
            Ok(())
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
            dentry::find_dentry(&self.volumes[parent_v_idx].storage, local_parent, new_name)?
        {
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "File already exists".to_string(),
            ));
        }

        let _inode_guard = self.volumes[child_v_idx]
            .dlm
            .lock_exclusive(&format!("I{}", local_child))
            .await;
        let mut disk_inode = inode::read_inode(&self.volumes[child_v_idx].storage, local_child)?;
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
        inode::write_inode(&self.volumes[child_v_idx].storage, local_child, &disk_inode)?;

        dentry::insert_dentry(
            &self.volumes[parent_v_idx].storage,
            local_parent,
            ino,
            new_name,
            disk_inode.mode & libc::S_IFMT,
        )?;

        // Update parent directory times
        if let Ok(mut parent_inode) =
            inode::read_inode(&self.volumes[parent_v_idx].storage, local_parent)
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
            );
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

    async fn rename(
        &self,
        old_parent: Ino,
        old_name: &str,
        new_parent: Ino,
        new_name: &str,
    ) -> Result<()> {
        let (old_parent_v_idx, local_old_parent) = self.route_ino(old_parent);
        let (new_parent_v_idx, local_new_parent) = self.route_ino(new_parent);
        self.check_volume_enabled(old_parent_v_idx)?;
        self.check_volume_enabled(new_parent_v_idx)?;

        let _old_pg = self.volumes[old_parent_v_idx]
            .dlm
            .lock_exclusive(&format!("I{}", local_old_parent))
            .await;
        let _new_pg = if old_parent != new_parent {
            Some(
                self.volumes[new_parent_v_idx]
                    .dlm
                    .lock_exclusive(&format!("I{}", local_new_parent))
                    .await,
            )
        } else {
            None
        };

        let old_dentry_key = format!("D{}:{}", local_old_parent, old_name);
        let new_dentry_key = format!("D{}:{}", local_new_parent, new_name);
        let _old_dg = self.volumes[old_parent_v_idx]
            .dlm
            .lock_exclusive(&old_dentry_key)
            .await;
        let _new_dg = if old_dentry_key != new_dentry_key || old_parent_v_idx != new_parent_v_idx {
            Some(
                self.volumes[new_parent_v_idx]
                    .dlm
                    .lock_exclusive(&new_dentry_key)
                    .await,
            )
        } else {
            None
        };

        if let Some(dentry) = dentry::find_dentry(
            &self.volumes[old_parent_v_idx].storage,
            local_old_parent,
            old_name,
        )? {
            let is_dir = dentry.file_type == libc::S_IFDIR;
            let cross_dir =
                old_parent_v_idx != new_parent_v_idx || local_old_parent != local_new_parent;

            if is_dir && cross_dir {
                // Decrement old parent link count
                if let Ok(mut old_p_inode) =
                    inode::read_inode(&self.volumes[old_parent_v_idx].storage, local_old_parent)
                {
                    if old_p_inode.nlink > 2 {
                        old_p_inode.nlink -= 1;
                    }
                    let _ = inode::write_inode(
                        &self.volumes[old_parent_v_idx].storage,
                        local_old_parent,
                        &old_p_inode,
                    );
                }
                // Increment new parent link count
                if let Ok(mut new_p_inode) =
                    inode::read_inode(&self.volumes[new_parent_v_idx].storage, local_new_parent)
                {
                    new_p_inode.nlink += 1;
                    let _ = inode::write_inode(
                        &self.volumes[new_parent_v_idx].storage,
                        local_new_parent,
                        &new_p_inode,
                    );
                }
            }
            // Check if destination already exists to decrement its link count
            if let Some(dest_dentry) = dentry::find_dentry(
                &self.volumes[new_parent_v_idx].storage,
                local_new_parent,
                new_name,
            )? {
                let dest_ino = dest_dentry.child_ino;
                let (dest_v_idx, local_dest) = self.route_ino(dest_ino);
                if let Ok(mut dest_inode) =
                    inode::read_inode(&self.volumes[dest_v_idx].storage, local_dest)
                {
                    // Check if destination is a directory and is not empty
                    if (dest_inode.mode & libc::S_IFMT) == libc::S_IFDIR {
                        let dentries =
                            dentry::list_dentries(&self.volumes[dest_v_idx].storage, local_dest)?;
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
                    );
                }
                // Remove the destination dentry so it gets replaced cleanly
                dentry::remove_dentry(
                    &self.volumes[new_parent_v_idx].storage,
                    local_new_parent,
                    new_name,
                )?;
            }

            dentry::remove_dentry(
                &self.volumes[old_parent_v_idx].storage,
                local_old_parent,
                old_name,
            )?;
            dentry::insert_dentry(
                &self.volumes[new_parent_v_idx].storage,
                local_new_parent,
                dentry.child_ino,
                new_name,
                dentry.file_type,
            )?;
            Ok(())
        } else {
            Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Source dentry not found",
            )))
        }
    }

    async fn readdir(&self, dir: Ino, _offset: u64, _max: usize) -> Result<Vec<DirEntry>> {
        let (v_idx, local_dir) = self.route_ino(dir);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx]
            .dlm
            .lock_shared(&format!("I{}", local_dir))
            .await;
        let dentries = dentry::list_dentries(&self.volumes[v_idx].storage, local_dir)?;
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
        let disk_inode = inode::read_inode(&self.volumes[v_idx].storage, local_ino)?;
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
        let mut disk_inode = inode::read_inode(&self.volumes[v_idx].storage, local_ino)?;
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
        inode::write_inode(&self.volumes[v_idx].storage, local_ino, &disk_inode)?;
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

    async fn getxattr(&self, ino: Ino, name: &str) -> Result<Option<Vec<u8>>> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx]
            .dlm
            .lock_shared(&format!("I{}", local_ino))
            .await;
        xattr::get_xattr(&self.volumes[v_idx].storage, local_ino, name)
    }

    async fn setxattr(&self, ino: Ino, name: &str, value: &[u8]) -> Result<()> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx]
            .dlm
            .lock_exclusive(&format!("I{}", local_ino))
            .await;
        xattr::set_xattr(&self.volumes[v_idx].storage, local_ino, name, value)
    }

    async fn removexattr(&self, ino: Ino, name: &str) -> Result<()> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx]
            .dlm
            .lock_exclusive(&format!("I{}", local_ino))
            .await;
        xattr::remove_xattr(&self.volumes[v_idx].storage, local_ino, name)
    }

    async fn listxattr(&self, ino: Ino) -> Result<Vec<String>> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx]
            .dlm
            .lock_shared(&format!("I{}", local_ino))
            .await;
        xattr::list_xattrs(&self.volumes[v_idx].storage, local_ino)
    }

    async fn destroy_inode(&self, ino: Ino) -> Result<()> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        self.volumes[v_idx].destroy_inode(local_ino).await
    }
}
