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
    async fn create(&self, parent: Ino, name: &str, mode: u32) -> Result<Inode>;
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
    async fn setattr(&self, ino: Ino, mode: Option<u32>, size: Option<u64>) -> Result<Inode>;
    async fn getxattr(&self, ino: Ino, name: &str) -> Result<Option<Vec<u8>>>;
    async fn setxattr(&self, ino: Ino, name: &str, value: &[u8]) -> Result<()>;
    async fn removexattr(&self, ino: Ino, name: &str) -> Result<()>;
    async fn listxattr(&self, ino: Ino) -> Result<Vec<String>>;
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
        for i in 2..1000 {
            if inode::read_inode(&self.storage, i).is_ok() {
                count += 1;
            }
        }
        count
    }

    /// Formats the raw block storage device with a superblock and the root inode
    pub fn format(storage: &storage::MetaLvStorage) -> Result<()> {
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

    async fn create(&self, parent: Ino, name: &str, mode: u32) -> Result<Inode> {
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
        for i in 2..1000 {
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

        let disk_inode = inode::DiskInode::new(new_ino, mode, 0, 0);
        inode::write_inode_raw(&self.storage, new_ino, &disk_inode)?;
        std::mem::drop(_op_guard);

        dentry::insert_dentry(&self.storage, parent, new_ino, name, mode & libc::S_IFMT)?;

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

            // Decrement nlink
            let mut disk_inode = inode::read_inode(&self.storage, ino)?;
            if disk_inode.nlink > 1 {
                disk_inode.nlink -= 1;
                inode::write_inode(&self.storage, ino, &disk_inode)?;
            } else {
                // Remove inode by zeroing magic
                let empty = inode::DiskInode::new_zeroed();
                inode::write_inode(&self.storage, ino, &empty)?;
            }
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
        inode::write_inode(&self.storage, ino, &disk_inode)?;

        dentry::insert_dentry(
            &self.storage,
            new_parent,
            ino,
            new_name,
            disk_inode.mode & libc::S_IFMT,
        )?;

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

    async fn setattr(&self, ino: Ino, mode: Option<u32>, size: Option<u64>) -> Result<Inode> {
        let _guard = self.dlm.lock_exclusive(&format!("I{}", ino)).await;
        let mut disk_inode = inode::read_inode(&self.storage, ino)?;
        if let Some(m) = mode {
            disk_inode.mode = m;
        }
        if let Some(s) = size {
            disk_inode.size = s;
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
}

#[derive(Clone)]
pub struct RoutedMetaBackend {
    pub volumes: Vec<std::sync::Arc<MetaLvBackend>>,
}

impl RoutedMetaBackend {
    pub fn new(volumes: Vec<std::sync::Arc<MetaLvBackend>>) -> Self {
        Self { volumes }
    }

    pub fn route_ino(&self, ino: Ino) -> (usize, Ino) {
        let num_volumes = self.volumes.len();
        if num_volumes <= 1 {
            return (0, ino);
        }
        if ino == 1 {
            return (0, 1);
        }
        let volume_idx = ((ino - 2) % num_volumes as u64) as usize;
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

    async fn create(&self, parent: Ino, name: &str, mode: u32) -> Result<Inode> {
        let (parent_v_idx, local_parent) = self.route_ino(parent);
        let is_dir = (mode & libc::S_IFMT) == libc::S_IFDIR;
        let target_v_idx = if is_dir {
            let mut min_idx = 0;
            let mut min_count = usize::MAX;
            for (i, vol) in self.volumes.iter().enumerate() {
                let count = vol.get_allocated_inode_count();
                if count < min_count {
                    min_count = count;
                    min_idx = i;
                }
            }
            min_idx
        } else {
            parent_v_idx
        };

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
        for i in 2..1000 {
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

        let disk_inode = inode::DiskInode::new(new_local_ino, mode, 0, 0);
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

            let mut disk_inode =
                inode::read_inode(&self.volumes[child_v_idx].storage, local_child)?;
            log::debug!(
                "meta_backend unlink: local_child = {}, nlink = {}",
                local_child,
                disk_inode.nlink
            );
            if disk_inode.nlink > 1 {
                disk_inode.nlink -= 1;
                log::debug!(
                    "meta_backend unlink (nlink > 1): local_child = {}, writing nlink = {}",
                    local_child,
                    disk_inode.nlink
                );
                inode::write_inode(&self.volumes[child_v_idx].storage, local_child, &disk_inode)?;
            } else {
                log::debug!(
                    "meta_backend unlink (nlink <= 1): local_child = {}, zeroing inode",
                    local_child
                );
                let empty = inode::DiskInode::new_zeroed();
                inode::write_inode(&self.volumes[child_v_idx].storage, local_child, &empty)?;
            }
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

    async fn setattr(&self, ino: Ino, mode: Option<u32>, size: Option<u64>) -> Result<Inode> {
        let (v_idx, local_ino) = self.route_ino(ino);
        let _guard = self.volumes[v_idx]
            .dlm
            .lock_exclusive(&format!("I{}", local_ino))
            .await;
        let mut disk_inode = inode::read_inode(&self.volumes[v_idx].storage, local_ino)?;
        if let Some(m) = mode {
            disk_inode.mode = m;
        }
        if let Some(s) = size {
            disk_inode.size = s;
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
        let _guard = self.volumes[v_idx]
            .dlm
            .lock_shared(&format!("I{}", local_ino))
            .await;
        xattr::get_xattr(&self.volumes[v_idx].storage, local_ino, name)
    }

    async fn setxattr(&self, ino: Ino, name: &str, value: &[u8]) -> Result<()> {
        let (v_idx, local_ino) = self.route_ino(ino);
        let _guard = self.volumes[v_idx]
            .dlm
            .lock_exclusive(&format!("I{}", local_ino))
            .await;
        xattr::set_xattr(&self.volumes[v_idx].storage, local_ino, name, value)
    }

    async fn removexattr(&self, ino: Ino, name: &str) -> Result<()> {
        let (v_idx, local_ino) = self.route_ino(ino);
        let _guard = self.volumes[v_idx]
            .dlm
            .lock_exclusive(&format!("I{}", local_ino))
            .await;
        xattr::remove_xattr(&self.volumes[v_idx].storage, local_ino, name)
    }

    async fn listxattr(&self, ino: Ino) -> Result<Vec<String>> {
        let (v_idx, local_ino) = self.route_ino(ino);
        let _guard = self.volumes[v_idx]
            .dlm
            .lock_shared(&format!("I{}", local_ino))
            .await;
        xattr::list_xattrs(&self.volumes[v_idx].storage, local_ino)
    }
}
