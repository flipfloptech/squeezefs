use crate::dlm::MetaClient;
use crate::error::Result;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub struct BlockAllocator {
    _client: Arc<MetaClient>,
    _volume_id: Box<str>,
    chunk_size: u64,
    free_blocks: Mutex<HashSet<u64>>,
    highest_block: AtomicU64,
    refcounts: Mutex<HashMap<u64, u32>>,
}

impl BlockAllocator {
    pub async fn new(client: Arc<MetaClient>, volume_id: &str) -> Result<Self> {
        Ok(Self {
            _client: client,
            _volume_id: volume_id.to_string().into_boxed_str(),
            chunk_size: 4 * 1024 * 1024, // 4MB
            free_blocks: Mutex::new(HashSet::new()),
            highest_block: AtomicU64::new(0),
            refcounts: Mutex::new(HashMap::new()),
        })
    }

    pub fn increment_refcount(&self, offset: u64) {
        let mut refs = self.refcounts.lock();
        let count = refs.entry(offset).or_insert(1);
        *count += 1;
    }

    pub async fn allocate_block(&self) -> Result<u64> {
        let mut free = self.free_blocks.lock();
        let block_idx = if let Some(&idx) = free.iter().next() {
            free.remove(&idx);
            idx
        } else {
            self.highest_block.fetch_add(1, Ordering::Relaxed)
        };
        let offset = block_idx * self.chunk_size;
        self.refcounts.lock().insert(offset, 1);
        Ok(offset)
    }

    pub async fn free_block(&self, offset: u64) -> Result<()> {
        let mut refs = self.refcounts.lock();
        let should_free = if let Some(count) = refs.get_mut(&offset) {
            *count -= 1;
            if *count == 0 {
                refs.remove(&offset);
                true
            } else {
                false
            }
        } else {
            true
        };

        if should_free {
            let block_idx = offset / self.chunk_size;
            self.free_blocks.lock().insert(block_idx);
            crate::fuse_client::METRICS
                .del_obj
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        let free = self.free_blocks.lock().len() as u64;
        highest.saturating_sub(free)
    }

    pub async fn calculate_fragmentation(&self) -> Result<(u64, u64, u64, f64)> {
        let highest_block = self.highest_block.load(Ordering::Relaxed);
        let free_blocks = self.free_blocks.lock().len() as u64;
        let used_blocks = highest_block.saturating_sub(free_blocks);
        let frag_percent = if highest_block > 0 {
            (free_blocks as f64 / highest_block as f64) * 100.0
        } else {
            0.0
        };

        Ok((highest_block, used_blocks, free_blocks, frag_percent))
    }

    pub async fn allocate_specific_block(&self, block_idx: u64) -> Result<()> {
        let mut free = self.free_blocks.lock();
        let cur_highest = self.highest_block.load(Ordering::Relaxed);
        if block_idx >= cur_highest {
            for idx in cur_highest..block_idx {
                free.insert(idx);
            }
            self.highest_block.store(block_idx + 1, Ordering::Relaxed);
        } else {
            if !free.remove(&block_idx) {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "Block {} is not free or does not exist",
                    block_idx
                )));
            }
        }
        let offset = block_idx * self.chunk_size;
        self.refcounts.lock().insert(offset, 1);
        Ok(())
    }

    pub async fn get_free_blocks(&self) -> Result<Vec<u64>> {
        let free = self.free_blocks.lock();
        let mut list: Vec<u64> = free.iter().cloned().collect();
        list.sort_unstable();
        Ok(list)
    }

    pub async fn recover_block(&self, block_idx: u64) -> Result<()> {
        let mut free = self.free_blocks.lock();
        let cur_highest = self.highest_block.load(Ordering::Relaxed);
        let offset = block_idx * self.chunk_size;

        if block_idx >= cur_highest {
            for idx in cur_highest..block_idx {
                free.insert(idx);
            }
            self.highest_block.store(block_idx + 1, Ordering::Relaxed);
            self.refcounts.lock().insert(offset, 1);
        } else {
            if free.remove(&block_idx) {
                self.refcounts.lock().insert(offset, 1);
            } else {
                let mut refs = self.refcounts.lock();
                let count = refs.entry(offset).or_insert(0);
                *count += 1;
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
                                        if let Ok(offset) = indirect_key.parse::<u64>() {
                                            let indirect_idx = offset / self.chunk_size;
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
                                        if let Ok(offset) = block_key.parse::<u64>() {
                                            let block_idx = offset / self.chunk_size;
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
