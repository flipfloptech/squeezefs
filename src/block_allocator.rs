use crate::dlm::MetaClient;
use crate::error::Result;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

pub struct BlockAllocator {
    _client: Arc<MetaClient>,
    _volume_id: Box<str>,
    chunk_size: u64,
    free_blocks: dashmap::DashSet<u64>,
    highest_block: AtomicU64,
    refcounts: scc::HashMap<u64, AtomicU32>,
    /// Per-offset incarnation seqlock: `gen << 1 | stable`.
    ///
    /// Block keys are plain offset strings, so when an offset is freed and
    /// reallocated the *same key string* names a new incarnation. A reader that
    /// resolved the key through a slightly stale block map can device-read the
    /// offset mid-transition and then publish those bytes into the shared block
    /// caches — poisoning the key for its new owner (the block then reads as
    /// zeros or another block's data until remount, while durable data is
    /// correct). The seqlock makes cache fills validated: `allocate_block`
    /// marks the offset in-flight (gen+1, stable=0), the writer calls
    /// [`Self::publish_block`] after its device write (stable=1), and
    /// `free_block` retires it (gen+1, stable=0). A fill may publish into the
    /// caches only if the word was stable before its device read and unchanged
    /// after — anything else returns bytes to the caller uncached.
    incarnations: scc::HashMap<u64, AtomicU64>,
}

impl BlockAllocator {
    pub async fn new(client: Arc<MetaClient>, volume_id: &str) -> Result<Self> {
        Ok(Self {
            _client: client,
            _volume_id: volume_id.to_string().into_boxed_str(),
            chunk_size: 4 * 1024 * 1024, // 4MB
            free_blocks: dashmap::DashSet::new(),
            highest_block: AtomicU64::new(0),
            refcounts: scc::HashMap::new(),
            incarnations: scc::HashMap::new(),
        })
    }

    /// gen+1, stable=0 — offset owned by a writer whose data is not yet on the
    /// device (or retired by a free). Cache fills must not publish.
    fn mark_incarnation_unstable(&self, offset: u64) {
        match self.incarnations.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(occ) => {
                let _ = occ
                    .get()
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                        Some(((cur >> 1) + 1) << 1)
                    });
            }
            scc::hash_map::Entry::Vacant(vac) => {
                let _ = vac.insert_entry(AtomicU64::new(1 << 1));
            }
        }
    }

    /// Writer's durable device write for this incarnation completed — cache
    /// fills that observe an unchanged stable word may publish.
    pub fn publish_block(&self, offset: u64) {
        match self.incarnations.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(occ) => {
                occ.get().fetch_or(1, Ordering::AcqRel);
            }
            scc::hash_map::Entry::Vacant(vac) => {
                let _ = vac.insert_entry(AtomicU64::new(1));
            }
        }
    }

    /// Snapshot the incarnation word for a fill. `None` while unstable
    /// (in-flight write or retired/free) — the fill must not publish. Offsets
    /// with no recorded incarnation (written before this process / by another
    /// node) are treated as stable.
    pub fn fill_incarnation(&self, offset: u64) -> Option<u64> {
        match self
            .incarnations
            .read_sync(&offset, |_, v| v.load(Ordering::Acquire))
        {
            Some(word) if word & 1 == 1 => Some(word),
            Some(_) => None,
            None => Some(u64::MAX), // unknown offset: stable sentinel
        }
    }

    /// True if the incarnation word is unchanged since [`Self::fill_incarnation`]
    /// (no allocate/publish/free transitioned the offset during the fill's
    /// device read).
    pub fn fill_incarnation_still(&self, offset: u64, before: u64) -> bool {
        let now = self
            .incarnations
            .read_sync(&offset, |_, v| v.load(Ordering::Acquire));
        match (before, now) {
            (u64::MAX, None) => true,
            (b, Some(n)) => b == n,
            _ => false,
        }
    }

    pub fn increment_refcount(&self, offset: u64) {
        match self.refcounts.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(mut occ) => {
                occ.get_mut().fetch_add(1, Ordering::SeqCst);
            }
            scc::hash_map::Entry::Vacant(vac) => {
                let _ = vac.insert_entry(AtomicU32::new(2));
            }
        }
    }

    pub fn volume_id(&self) -> &str {
        &self._volume_id
    }

    pub fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    pub async fn allocate_block(&self) -> Result<u64> {
        let mut found_idx = None;
        for item in self.free_blocks.iter() {
            found_idx = Some(*item);
            break;
        }
        let block_idx = if let Some(idx) = found_idx {
            if self.free_blocks.remove(&idx).is_some() {
                idx
            } else {
                self.highest_block.fetch_add(1, Ordering::Relaxed)
            }
        } else {
            self.highest_block.fetch_add(1, Ordering::Relaxed)
        };
        let offset = block_idx * self.chunk_size;
        let _ = self.refcounts.insert_sync(offset, AtomicU32::new(1));
        // New incarnation, not yet durable: cache fills must not publish until
        // the owner calls `publish_block` after its device write.
        self.mark_incarnation_unstable(offset);
        Ok(offset)
    }

    pub async fn free_block(&self, offset: u64) -> Result<()> {
        let should_free = if let Some(cell) = self
            .refcounts
            .read_sync(&offset, |_, v| v.fetch_sub(1, Ordering::SeqCst) == 1)
        {
            if cell {
                self.refcounts.remove_sync(&offset);
                true
            } else {
                false
            }
        } else {
            true
        };

        if should_free {
            // Retire this incarnation before the offset becomes reallocatable so
            // any in-flight cache fill that resolved a stale map to this key
            // fails its seqlock validation instead of poisoning the next owner.
            self.mark_incarnation_unstable(offset);
            let block_idx = offset / self.chunk_size;
            self.free_blocks.insert(block_idx);
            crate::fuse_client::METRICS
                .del_obj
                .fetch_add(1, Ordering::Relaxed);
        }

        Ok(())
    }

    pub async fn free_blocks(&self, offsets: &[u64]) -> Result<()> {
        for &offset in offsets {
            let _ = self.free_block(offset).await;
        }
        Ok(())
    }

    pub fn get_used_blocks(&self) -> u64 {
        let highest = self.highest_block.load(Ordering::Relaxed);
        let free = self.free_blocks.len() as u64;
        highest.saturating_sub(free)
    }

    pub async fn calculate_fragmentation(&self) -> Result<(u64, u64, u64, f64)> {
        let highest_block = self.highest_block.load(Ordering::Relaxed);
        let free_blocks = self.free_blocks.len() as u64;
        let used_blocks = highest_block.saturating_sub(free_blocks);
        let frag_percent = if highest_block > 0 {
            (free_blocks as f64 / highest_block as f64) * 100.0
        } else {
            0.0
        };

        Ok((highest_block, used_blocks, free_blocks, frag_percent))
    }

    pub async fn allocate_specific_block(&self, block_idx: u64) -> Result<()> {
        let cur_highest = self.highest_block.load(Ordering::Relaxed);
        if block_idx >= cur_highest {
            for idx in cur_highest..block_idx {
                self.free_blocks.insert(idx);
            }
            self.highest_block.store(block_idx + 1, Ordering::Relaxed);
        } else {
            if !self.free_blocks.remove(&block_idx).is_some() {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "Block {} is not free or does not exist",
                    block_idx
                )));
            }
        }
        let offset = block_idx * self.chunk_size;
        let _ = self.refcounts.insert_sync(offset, AtomicU32::new(1));
        Ok(())
    }

    pub async fn get_free_blocks(&self) -> Result<Vec<u64>> {
        let mut list: Vec<u64> = self.free_blocks.iter().map(|item| *item).collect();
        list.sort_unstable();
        Ok(list)
    }

    pub async fn recover_block(&self, block_idx: u64) -> Result<()> {
        let cur_highest = self.highest_block.load(Ordering::Relaxed);
        let offset = block_idx * self.chunk_size;

        if block_idx >= cur_highest {
            for idx in cur_highest..block_idx {
                self.free_blocks.insert(idx);
            }
            self.highest_block.store(block_idx + 1, Ordering::Relaxed);
            let _ = self.refcounts.insert_sync(offset, AtomicU32::new(1));
        } else {
            if self.free_blocks.remove(&block_idx).is_some() {
                let _ = self.refcounts.insert_sync(offset, AtomicU32::new(1));
            } else {
                match self.refcounts.entry_sync(offset) {
                    scc::hash_map::Entry::Occupied(mut occ) => {
                        occ.get_mut().fetch_add(1, Ordering::SeqCst);
                    }
                    scc::hash_map::Entry::Vacant(vac) => {
                        let _ = vac.insert_entry(AtomicU32::new(1));
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn recover_active_blocks(
        &self,
        storage: &crate::meta_backend::storage::MetaLvStorage,
        backend_router: &crate::routing::BackendRouter,
    ) -> Result<()> {
        let max_inodes = (8 * 1024 * 1024 - 4096) / 256;
        let mut checked = 0;
        let mut valid_inodes = 0;
        let mut layouts_found = 0;
        for ino in 1..=max_inodes {
            if let Ok(inode) = crate::meta_backend::inode::read_inode(storage, ino).await {
                checked += 1;
                if inode.nlink > 0 {
                    valid_inodes += 1;
                    if let Ok(Some(bytes)) =
                        crate::meta_backend::xattr::get_xattr(storage, ino, "layout").await
                    {
                        layouts_found += 1;
                        let layout_opt: Option<crate::routing::LayoutMetadata> =
                            if bytes.starts_with(b"{") {
                                serde_json::from_slice(&bytes).ok()
                            } else {
                                bincode::deserialize(&bytes).ok()
                            };
                        println!(
                            "ino: {}, mode: {}, nlink: {}, layout: {:?}",
                            ino, inode.mode, inode.nlink, layout_opt
                        );
                        if let Some(layout) = layout_opt {
                            if layout.file_type == "striped" {
                                // 1. Check for indirect block map
                                if let Some(ref map_id) = layout.block_map_id {
                                    if map_id.starts_with("indirect:") {
                                        let indirect_key =
                                            map_id.strip_prefix("indirect:").unwrap();
                                        let mut matches = false;
                                        let mut block_offset = 0;
                                        if let Ok((be_id, offset)) =
                                            backend_router.parse_block_key(indirect_key)
                                        {
                                            if be_id == self._volume_id.as_ref()
                                                || ((be_id == "backend_0" || be_id == "squeezefs")
                                                    && (self._volume_id.as_ref() == "squeezefs"
                                                        || self._volume_id.as_ref()
                                                            == backend_router
                                                                .default_allocator
                                                                .volume_id()))
                                            {
                                                matches = true;
                                                block_offset = offset;
                                            }
                                        } else if let Ok(offset) = indirect_key.parse::<u64>() {
                                            if self._volume_id.as_ref() == "squeezefs"
                                                || self._volume_id.as_ref()
                                                    == backend_router.default_allocator.volume_id()
                                            {
                                                matches = true;
                                                block_offset = offset;
                                            }
                                        }
                                        if matches {
                                            let indirect_idx = block_offset / self.chunk_size;
                                            let _ = self.recover_block(indirect_idx).await;
                                        }
                                        // Read the indirect block to recover its entries
                                        let block_size =
                                            backend_router.block_size.load(Ordering::Relaxed)
                                                as usize;
                                        if let Ok(raw_bytes) = backend_router
                                            .read_block(indirect_key, block_size)
                                            .await
                                        {
                                            if let Ok(entries) =
                                                bincode::deserialize::<Vec<(u32, u64)>>(&raw_bytes)
                                            {
                                                for (_b, offset) in entries {
                                                    let block_idx = offset / self.chunk_size;
                                                    let _ = self.recover_block(block_idx).await;
                                                }
                                            }
                                        }
                                    }
                                }
                                // 2. Check for inline block map
                                if let Some(ref bm) = layout.block_map {
                                    for offset_str in bm.values() {
                                        let block_key =
                                            offset_str.split(':').next().unwrap_or(offset_str);
                                        let mut matches = false;
                                        let mut block_offset = 0;
                                        if let Ok((be_id, offset)) =
                                            backend_router.parse_block_key(block_key)
                                        {
                                            if be_id == self._volume_id.as_ref()
                                                || ((be_id == "backend_0" || be_id == "squeezefs")
                                                    && (self._volume_id.as_ref() == "squeezefs"
                                                        || self._volume_id.as_ref()
                                                            == backend_router
                                                                .default_allocator
                                                                .volume_id()))
                                            {
                                                matches = true;
                                                block_offset = offset;
                                            }
                                        } else if let Ok(offset) = block_key.parse::<u64>() {
                                            if self._volume_id.as_ref() == "squeezefs"
                                                || self._volume_id.as_ref()
                                                    == backend_router.default_allocator.volume_id()
                                            {
                                                matches = true;
                                                block_offset = offset;
                                            }
                                        }
                                        if matches {
                                            let block_idx = block_offset / self.chunk_size;
                                            let _ = self.recover_block(block_idx).await;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        println!(
            "Recovery scan summary: checked={}, valid_inodes={}, layouts_found={}",
            checked, valid_inodes, layouts_found
        );
        Ok(())
    }
}
