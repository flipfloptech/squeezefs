use bytes::Bytes;
use memmap2::MmapMut;
use parking_lot::{RwLock, RwLockReadGuard};
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use xxhash_rust::xxh3::xxh3_64;

const BLOCK_MAGIC: u32 = 0xCAFEBABE;
const HEADER_SIZE: usize = 12; // magic (4B) + key_len (4B) + val_len (4B)

#[derive(Clone, Copy, Debug)]
struct BlockMeta {
    offset: usize,
    len: usize,
}

pub struct NvmeShardInner {
    pub mmap: MmapMut,
    map: HashMap<Bytes, BlockMeta>,
    active_keys: VecDeque<Bytes>,
    write_offset: usize,
    capacity: usize,
}

impl NvmeShardInner {
    fn overlaps(w_start: usize, w_end: usize, b_start: usize, b_end: usize) -> bool {
        w_start < b_end && w_end > b_start
    }

    fn evict_overlapping(
        &mut self,
        w_start: usize,
        w_end: usize,
        evicted: &mut Vec<(Bytes, Bytes)>,
    ) {
        while let Some(front_key) = self.active_keys.front() {
            let meta = match self.map.get(front_key) {
                Some(m) => *m,
                None => {
                    self.active_keys.pop_front();
                    continue;
                }
            };
            let b_start = meta.offset;
            let b_end = meta.offset + meta.len;

            if Self::overlaps(w_start, w_end, b_start, b_end) {
                let val_len = self.get_val_len_at(b_start);
                let k_start = b_start + HEADER_SIZE;
                if meta.len < HEADER_SIZE + val_len {
                    self.map.remove(front_key);
                    self.active_keys.pop_front();
                    continue;
                }
                let k_end = k_start + (meta.len - HEADER_SIZE - val_len);

                if k_end > self.mmap.len() || b_start + meta.len > self.mmap.len() {
                    self.map.remove(front_key);
                    self.active_keys.pop_front();
                    continue;
                }

                let key_bytes = front_key.clone();
                let v_start = k_end;
                let v_end = b_start + meta.len;
                let val_bytes = Bytes::copy_from_slice(&self.mmap[v_start..v_end]);

                evicted.push((key_bytes.clone(), val_bytes));

                self.map.remove(&key_bytes);
                self.active_keys.pop_front();
            } else {
                break;
            }
        }
    }

    fn get_val_len_at(&self, offset: usize) -> usize {
        let val_len_bytes = &self.mmap[offset + 8..offset + 12];
        u32::from_le_bytes(val_len_bytes.try_into().unwrap()) as usize
    }
}

pub struct NvmeReadGuard<'a> {
    pub _device: Option<Arc<NvmeDevice>>,
    pub guard: RwLockReadGuard<'a, NvmeShardInner>,
    pub offset: usize,
    pub len: usize,
}

pub struct NvmeCacheReadGuard {
    pub _device: Arc<NvmeDevice>,
    pub guard: RwLockReadGuard<'static, NvmeShardInner>,
    pub offset: usize,
    pub len: usize,
}

impl std::ops::Deref for NvmeCacheReadGuard {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        &self.guard.mmap[self.offset..self.offset + self.len]
    }
}

unsafe impl Send for NvmeCacheReadGuard {}
unsafe impl Sync for NvmeCacheReadGuard {}

pub struct NvmeShard {
    inner: RwLock<NvmeShardInner>,
    _file: Option<File>, // Keep file handle alive if file-backed
}

impl NvmeShard {
    fn new(path: &Path, capacity: usize) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        file.set_len(capacity as u64)?;
        let mmap = unsafe { MmapMut::map_mut(&file)? };

        Ok(Self {
            inner: RwLock::new(NvmeShardInner {
                mmap,
                map: HashMap::new(),
                active_keys: VecDeque::new(),
                write_offset: 0,
                capacity,
            }),
            // Keep the file open so optional uring fdatasync can target the segment (P2-8).
            _file: Some(file),
        })
    }

    fn new_anon(capacity: usize) -> std::io::Result<Self> {
        let mmap = MmapMut::map_anon(capacity)?;

        Ok(Self {
            inner: RwLock::new(NvmeShardInner {
                mmap,
                map: HashMap::new(),
                active_keys: VecDeque::new(),
                write_offset: 0,
                capacity,
            }),
            _file: None,
        })
    }

    pub fn get<'a>(&'a self, key: &Bytes) -> Option<NvmeReadGuard<'a>> {
        let inner = self.inner.read();
        if let Some(&meta) = inner.map.get(key) {
            // Verify magic
            let magic =
                u32::from_le_bytes(inner.mmap[meta.offset..meta.offset + 4].try_into().unwrap());
            if magic != BLOCK_MAGIC {
                return None;
            }
            let key_len = u32::from_le_bytes(
                inner.mmap[meta.offset + 4..meta.offset + 8]
                    .try_into()
                    .unwrap(),
            ) as usize;
            let val_len = u32::from_le_bytes(
                inner.mmap[meta.offset + 8..meta.offset + 12]
                    .try_into()
                    .unwrap(),
            ) as usize;

            let alignment = if inner.capacity >= 4096 { 4096 } else { 1 };
            let val_start =
                (meta.offset + HEADER_SIZE + key_len + alignment - 1) & !(alignment - 1);
            Some(NvmeReadGuard {
                _device: None,
                guard: inner,
                offset: val_start,
                len: val_len,
            })
        } else {
            None
        }
    }

    pub fn put(&self, key: Bytes, value: Bytes) -> Vec<(Bytes, Bytes)> {
        let key_len = key.len();
        let val_len = value.len();

        let (target_offset, val_offset, block_size, old_meta, evicted, mmap_ptr) = {
            let mut inner = self.inner.write();
            let alignment = if inner.capacity >= 4096 { 4096 } else { 1 };

            let mut target_offset = (inner.write_offset + alignment - 1) & !(alignment - 1);
            let val_offset =
                (target_offset + HEADER_SIZE + key_len + alignment - 1) & !(alignment - 1);
            let block_size = (val_offset - target_offset) + val_len;

            if block_size > inner.capacity {
                return Vec::new();
            }

            let mut evicted = Vec::new();

            // Check if we need to wrap around
            let capacity = inner.capacity;
            if target_offset + block_size > capacity {
                inner.evict_overlapping(target_offset, capacity, &mut evicted);
                target_offset = 0;
            }

            // Recompute after wrap-around to ensure aligned
            let target_offset = (target_offset + alignment - 1) & !(alignment - 1);
            let val_offset =
                (target_offset + HEADER_SIZE + key_len + alignment - 1) & !(alignment - 1);
            let block_size = (val_offset - target_offset) + val_len;

            // Evict overlapping blocks in the target range
            inner.evict_overlapping(target_offset, target_offset + block_size, &mut evicted);

            // Invalidate/remove key from map if it already existed
            let old_meta = inner.map.remove(&key);
            if old_meta.is_some() {
                inner.active_keys.retain(|k| k != &key);
            }

            inner.write_offset = target_offset + block_size;
            let mmap_ptr = inner.mmap.as_ptr() as *mut u8;
            (
                target_offset,
                val_offset,
                block_size,
                old_meta,
                evicted,
                mmap_ptr,
            )
        };

        // Perform memory copy and disk flushing OUTSIDE of the lock!
        unsafe {
            // Write magic
            std::ptr::copy_nonoverlapping(
                BLOCK_MAGIC.to_le_bytes().as_ptr(),
                mmap_ptr.add(target_offset),
                4,
            );
            // Write key len
            std::ptr::copy_nonoverlapping(
                (key_len as u32).to_le_bytes().as_ptr(),
                mmap_ptr.add(target_offset + 4),
                4,
            );
            // Write val len
            std::ptr::copy_nonoverlapping(
                (val_len as u32).to_le_bytes().as_ptr(),
                mmap_ptr.add(target_offset + 8),
                4,
            );
            // Write key
            std::ptr::copy_nonoverlapping(
                key.as_ptr(),
                mmap_ptr.add(target_offset + HEADER_SIZE),
                key_len,
            );
            // Write val
            std::ptr::copy_nonoverlapping(value.as_ptr(), mmap_ptr.add(val_offset), val_len);

            if let Some(ref old) = old_meta {
                std::ptr::write_bytes(mmap_ptr.add(old.offset), 0, 4);
                libc::msync(
                    mmap_ptr.add(old.offset) as *mut libc::c_void,
                    4,
                    libc::MS_ASYNC,
                );
            }

            libc::msync(
                mmap_ptr.add(target_offset) as *mut libc::c_void,
                block_size,
                libc::MS_ASYNC,
            );
        }

        // Phase 2: Insert into tracking map
        {
            let mut inner = self.inner.write();
            let meta = BlockMeta {
                offset: target_offset,
                len: block_size,
            };
            inner.map.insert(key.clone(), meta);
            inner.active_keys.push_back(key);
        }

        evicted
    }

    /// Admit `key` into the staging segment **without ever destroying live
    /// entries**. Staged payloads and active blocks are the sole copy of
    /// dirty data until promotion/upload, so shard exhaustion must surface
    /// as refusal (backpressure — the caller escalates to a durable direct
    /// upload), never as silent wrap-around eviction. First-fit over the
    /// gaps left by removed/replaced entries; the existing copy of `key` is
    /// tombstoned only after its replacement is fully written (never
    /// overwritten in place — a crash mid-write must leave one intact copy).
    ///
    /// Returns `true` when admitted.
    pub fn reserve_and_write(
        &self,
        key: Bytes,
        meta_len: u64,
        meta_bytes: &[u8],
        data: &[u8],
        align_data_to: Option<usize>,
    ) -> bool {
        let key_len = key.len();
        let header_size_written = 8 + meta_bytes.len();
        let padding_needed = if let Some(align) = align_data_to {
            align.saturating_sub(header_size_written)
        } else {
            0
        };
        let val_len = header_size_written + padding_needed + data.len();

        let (target_offset, val_offset, block_size, old_meta, mmap_ptr) = {
            let mut inner = self.inner.write();
            let alignment = if inner.capacity >= 4096 { 4096 } else { 1 };
            let capacity = inner.capacity;

            // Block geometry is alignment-invariant for aligned candidates:
            // val_offset - target_offset depends only on HEADER + key length.
            let val_delta = {
                let t0 = 0usize;
                let v0 = (t0 + HEADER_SIZE + key_len + alignment - 1) & !(alignment - 1);
                v0 - t0
            };
            let block_size = val_delta + val_len;
            if block_size > capacity {
                return false;
            }

            // Live extents (including any current copy of `key`: replacing
            // the sole copy in place would be torn by a crash mid-write).
            let mut live: Vec<(usize, usize)> = inner
                .map
                .values()
                .map(|m| (m.offset, m.offset + m.len))
                .collect();
            live.sort_unstable();

            // First-fit candidates: the write cursor, then the end of every
            // live extent (both wrapped) — this reuses the holes that
            // promotion/unlink punch into the ring.
            let cursor = (inner.write_offset + alignment - 1) & !(alignment - 1);
            let mut candidates: Vec<usize> = Vec::with_capacity(live.len() + 2);
            candidates.push(cursor);
            candidates.push(0);
            for &(_, end) in &live {
                candidates.push((end + alignment - 1) & !(alignment - 1));
            }
            candidates.sort_unstable();
            candidates.dedup();
            // Try candidates at/after the cursor first, then wrapped ones.
            let (after, before): (Vec<usize>, Vec<usize>) =
                candidates.into_iter().partition(|&t| t >= cursor);

            let fits = |t: usize| -> bool {
                let end = t + block_size;
                if end > capacity {
                    return false;
                }
                // live is sorted: find any overlap.
                !live
                    .iter()
                    .any(|&(b_start, b_end)| t < b_end && end > b_start)
            };

            let Some(target_offset) = after
                .into_iter()
                .chain(before.into_iter())
                .find(|&t| fits(t))
            else {
                return false;
            };
            let val_offset = target_offset + val_delta;

            // Invalidate/remove key from map if it already existed
            let old_meta = inner.map.remove(&key);
            if old_meta.is_some() {
                inner.active_keys.retain(|k| k != &key);
            }

            inner.write_offset = target_offset + block_size;
            let mmap_ptr = inner.mmap.as_ptr() as *mut u8;
            (target_offset, val_offset, block_size, old_meta, mmap_ptr)
        };

        // Perform memory copy and disk flushing OUTSIDE of the lock!
        unsafe {
            // Write magic
            std::ptr::copy_nonoverlapping(
                BLOCK_MAGIC.to_le_bytes().as_ptr(),
                mmap_ptr.add(target_offset),
                4,
            );
            // Write key len
            std::ptr::copy_nonoverlapping(
                (key_len as u32).to_le_bytes().as_ptr(),
                mmap_ptr.add(target_offset + 4),
                4,
            );
            // Write val len
            std::ptr::copy_nonoverlapping(
                (val_len as u32).to_le_bytes().as_ptr(),
                mmap_ptr.add(target_offset + 8),
                4,
            );
            // Write key
            std::ptr::copy_nonoverlapping(
                key.as_ptr(),
                mmap_ptr.add(target_offset + HEADER_SIZE),
                key_len,
            );

            // Write value parts directly to inner.mmap
            let mut cur = val_offset;
            std::ptr::copy_nonoverlapping(meta_len.to_be_bytes().as_ptr(), mmap_ptr.add(cur), 8);
            cur += 8;
            std::ptr::copy_nonoverlapping(meta_bytes.as_ptr(), mmap_ptr.add(cur), meta_bytes.len());
            cur += meta_bytes.len();
            if padding_needed > 0 {
                std::ptr::write_bytes(mmap_ptr.add(cur), 0, padding_needed);
                cur += padding_needed;
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), mmap_ptr.add(cur), data.len());

            // Tombstone the replaced copy only after the new one is fully
            // written: a crash in between leaves at most one live copy.
            if let Some(ref old) = old_meta {
                std::ptr::write_bytes(mmap_ptr.add(old.offset), 0, 4);
                libc::msync(
                    mmap_ptr.add(old.offset) as *mut libc::c_void,
                    4,
                    libc::MS_ASYNC,
                );
            }

            libc::msync(
                mmap_ptr.add(target_offset) as *mut libc::c_void,
                block_size,
                libc::MS_ASYNC,
            );
        }

        // Phase 2: Insert into tracking map
        {
            let mut inner = self.inner.write();
            let meta = BlockMeta {
                offset: target_offset,
                len: block_size,
            };
            inner.map.insert(key.clone(), meta);
            inner.active_keys.push_back(key);
        }

        true
    }

    /// Whether `key` currently has a live entry in this shard.
    pub fn has_key(&self, key: &Bytes) -> bool {
        self.inner.read().map.contains_key(key)
    }

    pub fn remove(&self, key: &Bytes) -> Option<Bytes> {
        let mut inner = self.inner.write();
        if let Some(meta) = inner.map.remove(key) {
            inner.active_keys.retain(|k| k != key);
            // Read value before invalidating
            let key_len = u32::from_le_bytes(
                inner.mmap[meta.offset + 4..meta.offset + 8]
                    .try_into()
                    .unwrap(),
            ) as usize;
            let val_len = u32::from_le_bytes(
                inner.mmap[meta.offset + 8..meta.offset + 12]
                    .try_into()
                    .unwrap(),
            ) as usize;
            let alignment = if inner.capacity >= 4096 { 4096 } else { 1 };
            let val_start =
                (meta.offset + HEADER_SIZE + key_len + alignment - 1) & !(alignment - 1);
            let val_end = val_start + val_len;
            let val = Bytes::copy_from_slice(&inner.mmap[val_start..val_end]);

            // Overwrite magic to invalidate on disk
            inner.mmap[meta.offset..meta.offset + 4].copy_from_slice(&0u32.to_le_bytes());
            let _ = inner.mmap.flush_range(meta.offset, 4);

            Some(val)
        } else {
            None
        }
    }

    pub fn active_keys(&self) -> Vec<Bytes> {
        self.inner.read().active_keys.iter().cloned().collect()
    }

    pub fn current_bytes(&self) -> usize {
        self.inner.read().map.values().map(|m| m.len).sum()
    }

    pub fn recover_index(&self) {
        let mut inner = self.inner.write();
        let mut offset = 0;
        let capacity = inner.capacity;

        inner.map.clear();
        inner.active_keys.clear();

        while offset + HEADER_SIZE <= capacity {
            let magic = u32::from_le_bytes(inner.mmap[offset..offset + 4].try_into().unwrap());
            if magic == BLOCK_MAGIC {
                let key_len =
                    u32::from_le_bytes(inner.mmap[offset + 4..offset + 8].try_into().unwrap())
                        as usize;
                let val_len =
                    u32::from_le_bytes(inner.mmap[offset + 8..offset + 12].try_into().unwrap())
                        as usize;
                let block_size = HEADER_SIZE + key_len + val_len;
                if block_size > HEADER_SIZE && offset + block_size <= capacity {
                    let key_start = offset + HEADER_SIZE;
                    let key_end = key_start + key_len;
                    let key = Bytes::copy_from_slice(&inner.mmap[key_start..key_end]);

                    let meta = BlockMeta {
                        offset,
                        len: block_size,
                    };
                    inner.map.insert(key.clone(), meta);
                    inner.active_keys.push_back(key);

                    offset += block_size;
                } else {
                    offset += 1;
                }
            } else {
                let mut skipped = false;
                if offset + 128 <= capacity
                    && inner.mmap[offset..offset + 128].iter().all(|&x| x == 0)
                {
                    offset += 128;
                    skipped = true;
                }
                if !skipped && offset + 8 <= capacity {
                    let chunk =
                        u64::from_ne_bytes(inner.mmap[offset..offset + 8].try_into().unwrap());
                    if chunk == 0 {
                        offset += 8;
                        skipped = true;
                    }
                }
                if !skipped {
                    offset += 1;
                }
            }
        }

        // Update write_offset to the end of the last active key, or 0 if empty
        if let Some(last_key) = inner.active_keys.back() {
            if let Some(meta) = inner.map.get(last_key) {
                inner.write_offset = meta.offset + meta.len;
            }
        }
    }

    pub fn sync_all(&self) -> std::io::Result<()> {
        if let Some(ref file) = self._file {
            file.sync_all()?;
        }
        Ok(())
    }
}

pub struct NvmeDevice {
    pub id: usize,
    pub path: std::path::PathBuf,
    pub capacity: usize,
    pub shards: Vec<NvmeShard>,
    pub online: Arc<AtomicBool>,
}

/// Highly concurrent NVMe-backed cache supporting multiple mount points (multi-rail),
/// round-robin writes, and dynamic online/offline migration.
pub struct NvmeCache {
    devices: RwLock<Vec<Arc<NvmeDevice>>>,
    write_counter: AtomicUsize,
    num_shards_per_device: usize,
}

impl NvmeCache {
    pub fn new(
        dirs: &[&Path],
        capacities: &[usize],
        num_shards_per_device: usize,
    ) -> std::io::Result<Self> {
        assert!(
            num_shards_per_device.is_power_of_two(),
            "Number of shards must be a power of two"
        );
        let mut devices = Vec::new();
        if dirs.is_empty() {
            let capacity = capacities.first().copied().unwrap_or(1024 * 1024 * 1024); // default 1GB
            let shard_capacity = capacity / num_shards_per_device;
            let mut shards = Vec::with_capacity(num_shards_per_device);
            for _ in 0..num_shards_per_device {
                shards.push(NvmeShard::new_anon(shard_capacity)?);
            }
            devices.push(Arc::new(NvmeDevice {
                id: 0,
                path: std::path::PathBuf::from("/memory"),
                capacity,
                shards,
                online: Arc::new(AtomicBool::new(true)),
            }));
        } else {
            for (i, (&dir, &capacity)) in dirs.iter().zip(capacities.iter()).enumerate() {
                std::fs::create_dir_all(dir)?;
                let shard_capacity = capacity / num_shards_per_device;
                let mut shards = Vec::with_capacity(num_shards_per_device);
                for j in 0..num_shards_per_device {
                    let filename = format!("segment_{}.bin", j);
                    let path = dir.join(filename);
                    shards.push(NvmeShard::new(&path, shard_capacity)?);
                }
                devices.push(Arc::new(NvmeDevice {
                    id: i,
                    path: dir.to_path_buf(),
                    capacity,
                    shards,
                    online: Arc::new(AtomicBool::new(true)),
                }));
            }
        }

        Ok(Self {
            devices: RwLock::new(devices),
            write_counter: AtomicUsize::new(0),
            num_shards_per_device,
        })
    }

    pub fn get<'a>(&'a self, key: &Bytes) -> Option<NvmeReadGuard<'a>> {
        let devices = self.devices.read();
        for dev in devices.iter() {
            if dev.online.load(Ordering::Relaxed) {
                let shard_idx = (xxh3_64(key) as usize) % dev.shards.len();
                if let Some(mut guard) = dev.shards[shard_idx].get(key) {
                    guard._device = Some(dev.clone());
                    // SAFETY: transmuting NvmeReadGuard to extend lifetime to 'a is safe
                    // because the cloned Arc<NvmeDevice> inside it guarantees the underlying
                    // shard and memory map are kept alive.
                    let extended_guard = unsafe {
                        std::mem::transmute::<NvmeReadGuard<'_>, NvmeReadGuard<'a>>(guard)
                    };
                    return Some(extended_guard);
                }
            }
        }
        None
    }

    pub fn get_static(&self, key: &Bytes) -> Option<NvmeCacheReadGuard> {
        let devices = self.devices.read();
        for dev in devices.iter() {
            if dev.online.load(Ordering::Relaxed) {
                let shard_idx = (xxh3_64(key) as usize) % dev.shards.len();
                let shard = &dev.shards[shard_idx];
                let inner = shard.inner.read();
                if let Some(&meta) = inner.map.get(key) {
                    let magic = u32::from_le_bytes(
                        inner.mmap[meta.offset..meta.offset + 4]
                            .try_into()
                            .unwrap_or([0; 4]),
                    );
                    if magic != BLOCK_MAGIC {
                        return None;
                    }
                    let key_len = u32::from_le_bytes(
                        inner.mmap[meta.offset + 4..meta.offset + 8]
                            .try_into()
                            .unwrap_or([0; 4]),
                    ) as usize;
                    let val_len = u32::from_le_bytes(
                        inner.mmap[meta.offset + 8..meta.offset + 12]
                            .try_into()
                            .unwrap_or([0; 4]),
                    ) as usize;

                    let alignment = if inner.capacity >= 4096 { 4096 } else { 1 };
                    let val_start =
                        (meta.offset + HEADER_SIZE + key_len + alignment - 1) & !(alignment - 1);

                    // SAFETY: Erasing lifetime of RwLockReadGuard is safe because
                    // NvmeCacheReadGuard holds a cloned Arc<NvmeDevice> which keeps
                    // the device, its shards, and the memory map alive.
                    let static_guard = unsafe {
                        std::mem::transmute::<
                            RwLockReadGuard<'_, NvmeShardInner>,
                            RwLockReadGuard<'static, NvmeShardInner>,
                        >(inner)
                    };
                    return Some(NvmeCacheReadGuard {
                        _device: dev.clone(),
                        guard: static_guard,
                        offset: val_start,
                        len: val_len,
                    });
                }
            }
        }
        None
    }

    pub fn put(&self, key: Bytes, value: Bytes) -> Vec<(Bytes, Bytes)> {
        let active_devices = {
            let guard = self.devices.read();
            guard
                .iter()
                .filter(|d| d.online.load(Ordering::Relaxed))
                .cloned()
                .collect::<Vec<_>>()
        };

        if active_devices.is_empty() {
            return Vec::new();
        }

        let idx = self.write_counter.fetch_add(1, Ordering::Relaxed) % active_devices.len();
        let dev = &active_devices[idx];
        let shard_idx = (xxh3_64(&key) as usize) % dev.shards.len();
        dev.shards[shard_idx].put(key, value)
    }

    /// Admit `key` without ever destroying live entries (see the shard-level
    /// doc). Key-affine: an existing copy is replaced on its own device
    /// (readers scan devices in order — divergent duplicates would serve
    /// stale data); new keys take the first device with room. Returns `true`
    /// when admitted.
    pub fn reserve_and_write(
        &self,
        key: Bytes,
        meta_len: u64,
        meta_bytes: &[u8],
        data: &[u8],
        align_data_to: Option<usize>,
    ) -> bool {
        let active_devices = {
            let guard = self.devices.read();
            guard
                .iter()
                .filter(|d| d.online.load(Ordering::Relaxed))
                .cloned()
                .collect::<Vec<_>>()
        };

        if active_devices.is_empty() {
            return false;
        }

        // Replace in place on the device that already holds the key.
        for dev in &active_devices {
            let shard_idx = (xxh3_64(&key) as usize) % dev.shards.len();
            if dev.shards[shard_idx].has_key(&key) {
                return dev.shards[shard_idx].reserve_and_write(
                    key,
                    meta_len,
                    meta_bytes,
                    data,
                    align_data_to,
                );
            }
        }

        // New key: spread by insertion order, falling back to any device
        // with room before refusing.
        let start = self.write_counter.fetch_add(1, Ordering::Relaxed) % active_devices.len();
        for step in 0..active_devices.len() {
            let dev = &active_devices[(start + step) % active_devices.len()];
            let shard_idx = (xxh3_64(&key) as usize) % dev.shards.len();
            if dev.shards[shard_idx].reserve_and_write(
                key.clone(),
                meta_len,
                meta_bytes,
                data,
                align_data_to,
            ) {
                return true;
            }
        }
        false
    }

    pub fn remove(&self, key: &Bytes) -> Option<Bytes> {
        let devices = self.devices.read();
        for dev in devices.iter() {
            let shard_idx = (xxh3_64(key) as usize) % dev.shards.len();
            if let Some(val) = dev.shards[shard_idx].remove(key) {
                return Some(val);
            }
        }
        None
    }

    pub async fn offline_device(&self, id: usize) -> Vec<(Bytes, Bytes)> {
        let target = {
            let guard = self.devices.read();
            guard.iter().find(|d| d.id == id).cloned()
        };

        let mut evicted = Vec::new();

        if let Some(dev) = target {
            // 1. Mark offline
            dev.online.store(false, Ordering::Relaxed);

            // 2. Scan shards for active keys
            for shard in &dev.shards {
                let keys = shard.active_keys();
                for key in keys {
                    if let Some(val) = shard.remove(&key) {
                        let evs = self.put(key.clone(), val.clone());
                        let no_other_online = {
                            let guard = self.devices.read();
                            !guard
                                .iter()
                                .any(|d| d.id != id && d.online.load(Ordering::Relaxed))
                        };
                        if evs.is_empty() && no_other_online {
                            evicted.push((key, val));
                        } else {
                            evicted.extend(evs);
                        }
                    }
                }
            }
        }
        evicted
    }

    pub async fn online_device(
        &self,
        id: usize,
        dir: &Path,
        capacity: usize,
    ) -> std::io::Result<()> {
        let existing = {
            let guard = self.devices.read();
            guard.iter().find(|d| d.id == id).cloned()
        };

        let new_dev = if let Some(dev) = existing {
            dev.online.store(true, Ordering::Relaxed);
            dev
        } else {
            std::fs::create_dir_all(dir)?;
            let shard_capacity = capacity / self.num_shards_per_device;
            let mut shards = Vec::with_capacity(self.num_shards_per_device);
            for j in 0..self.num_shards_per_device {
                let filename = format!("segment_{}.bin", j);
                let path = dir.join(filename);
                shards.push(NvmeShard::new(&path, shard_capacity)?);
            }
            let dev = Arc::new(NvmeDevice {
                id,
                path: dir.to_path_buf(),
                capacity,
                shards,
                online: Arc::new(AtomicBool::new(true)),
            });
            self.devices.write().push(dev.clone());
            dev
        };

        // Rebalance: move some keys from other online devices to this newly onlined one
        let other_devs = {
            let guard = self.devices.read();
            guard
                .iter()
                .filter(|d| d.id != id && d.online.load(Ordering::Relaxed))
                .cloned()
                .collect::<Vec<_>>()
        };

        if !other_devs.is_empty() {
            let share_fraction = other_devs.len() + 1;
            for other in other_devs {
                for shard in &other.shards {
                    let keys = shard.active_keys();
                    for (i, key) in keys.into_iter().enumerate() {
                        if i % share_fraction == 0 {
                            if let Some(val) = shard.remove(&key) {
                                let shard_idx = (xxh3_64(&key) as usize) % new_dev.shards.len();
                                new_dev.shards[shard_idx].put(key, val);
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    pub fn current_bytes(&self) -> usize {
        let devices = self.devices.read();
        let mut total = 0;
        for dev in devices.iter() {
            if dev.online.load(Ordering::Relaxed) {
                for shard in &dev.shards {
                    total += shard.current_bytes();
                }
            }
        }
        total
    }

    pub fn list_keys(&self) -> Vec<Bytes> {
        let mut keys = Vec::new();
        let devices = self.devices.read();
        for dev in devices.iter() {
            if dev.online.load(Ordering::Relaxed) {
                for shard in &dev.shards {
                    keys.extend(shard.active_keys());
                }
            }
        }
        keys
    }

    pub fn recover_index(&self) {
        let devices = self.devices.read();
        for dev in devices.iter() {
            for shard in &dev.shards {
                shard.recover_index();
            }
        }
    }

    pub async fn sync_all(&self) -> Result<(), crate::error::SqueezefsError> {
        let devices = {
            let guard = self.devices.read();
            guard.clone()
        };
        for dev in devices {
            if dev.online.load(Ordering::Relaxed) {
                for shard_idx in 0..dev.shards.len() {
                    let dev_clone = dev.clone();
                    tokio::task::spawn_blocking(move || dev_clone.shards[shard_idx].sync_all())
                        .await
                        .map_err(|e| {
                            crate::error::SqueezefsError::Io(std::io::Error::other(e.to_string()))
                        })??;
                }
            }
        }
        Ok(())
    }

    pub async fn sync_key(&self, key: &Bytes) -> Result<(), crate::error::SqueezefsError> {
        let devices = {
            let guard = self.devices.read();
            guard.clone()
        };
        for dev in devices {
            if dev.online.load(Ordering::Relaxed) {
                let shard_idx = (xxh3_64(key) as usize) % dev.shards.len();
                let exists = dev.shards[shard_idx].get(key).is_some();
                if exists {
                    let dev_clone = dev.clone();
                    tokio::task::spawn_blocking(move || dev_clone.shards[shard_idx].sync_all())
                        .await
                        .map_err(|e| {
                            crate::error::SqueezefsError::Io(std::io::Error::other(e.to_string()))
                        })??;
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_nvme_cache_basic() {
        let dir = tempdir().unwrap();
        let cache = NvmeCache::new(&[dir.path()], &[1024], 2).unwrap();

        let k1 = Bytes::from("block1");
        let v1 = Bytes::from(vec![9u8; 128]);
        let k2 = Bytes::from("block2");
        let v2 = Bytes::from(vec![7u8; 256]);

        assert!(cache.put(k1.clone(), v1.clone()).is_empty());
        assert!(cache.put(k2.clone(), v2.clone()).is_empty());

        let g1 = cache.get(&k1).unwrap();
        assert_eq!(&g1.guard.mmap[g1.offset..g1.offset + g1.len], v1.as_ref());
        let g2 = cache.get(&k2).unwrap();
        assert_eq!(&g2.guard.mmap[g2.offset..g2.offset + g2.len], v2.as_ref());
    }

    #[test]
    fn test_nvme_cache_eviction_ring() {
        let dir = tempdir().unwrap();
        // 200 bytes capacity, 1 shard to guarantee eviction order is FIFO
        let cache = NvmeCache::new(&[dir.path()], &[200], 1).unwrap();

        // Block size: 12 (header) + 6 (key) + 80 (val) = 98 bytes
        let k1 = Bytes::from("block1");
        let v1 = Bytes::from(vec![1u8; 80]);

        // Block size: 12 (header) + 6 (key) + 80 (val) = 98 bytes
        let k2 = Bytes::from("block2");
        let v2 = Bytes::from(vec![2u8; 80]);

        cache.put(k1.clone(), v1.clone());
        cache.put(k2.clone(), v2.clone());

        // Putting a 3rd block (98 bytes) will exceed capacity (196 + 98 = 294 > 200).
        // It must wrap around and evict block1.
        let k3 = Bytes::from("block3");
        let v3 = Bytes::from(vec![3u8; 80]);

        let evicted = cache.put(k3.clone(), v3.clone());
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].0, k1);
        assert_eq!(evicted[0].1, v1);

        assert!(cache.get(&k1).is_none());
        let g2 = cache.get(&k2).unwrap();
        assert_eq!(&g2.guard.mmap[g2.offset..g2.offset + g2.len], v2.as_ref());
        let g3 = cache.get(&k3).unwrap();
        assert_eq!(&g3.guard.mmap[g3.offset..g3.offset + g3.len], v3.as_ref());
    }

    /// Re-`put` of a key must not leave divergent duplicates across
    /// devices, and `remove` must purge every copy: the round-robin `put`
    /// could land a re-cache of the same block key on a different device,
    /// and first-hit `remove` then left a stale survivor that `get` (device
    /// scan order) served after a purge — dead bytes after a displaced-key
    /// invalidation.
    #[test]
    fn test_multi_device_reput_and_remove_leave_no_stale_duplicate() {
        let dir1 = tempdir().unwrap();
        let dir2 = tempdir().unwrap();
        let cache =
            NvmeCache::new(&[dir1.path(), dir2.path()], &[4096, 4096], 1).unwrap();

        let key = Bytes::from("block_dup");
        let v1 = Bytes::from(vec![0x11u8; 64]);
        let v2 = Bytes::from(vec![0x22u8; 64]);

        // Interleave with other keys so the round-robin counter points at a
        // different device for the re-put of the same key.
        cache.put(key.clone(), v1.clone());
        cache.put(Bytes::from("filler_a"), Bytes::from(vec![0u8; 16]));
        cache.put(key.clone(), v2.clone());

        // Whatever device it lives on, the current value must be v2.
        {
            let g = cache.get(&key).expect("key must be resident");
            assert_eq!(
                &g.guard.mmap[g.offset..g.offset + g.len],
                v2.as_ref(),
                "get served a stale duplicate from another device"
            );
        }

        // A purge must remove EVERY copy on every device.
        assert!(cache.remove(&key).is_some());
        assert!(
            cache.get(&key).is_none(),
            "a stale duplicate survived remove() on another device"
        );
    }

    #[tokio::test]
    async fn test_nvme_cache_multi_rail_migration() {
        let dir1 = tempdir().unwrap();
        let dir2 = tempdir().unwrap();

        // 2 NVMe devices, each has 1000 capacity, 1 shard
        let cache = NvmeCache::new(&[dir1.path(), dir2.path()], &[1000, 1000], 1).unwrap();

        let k1 = Bytes::from("k1");
        let v1 = Bytes::from("val1");
        let k2 = Bytes::from("k2");
        let v2 = Bytes::from("val2");

        cache.put(k1.clone(), v1.clone());
        cache.put(k2.clone(), v2.clone());

        {
            let g1 = cache.get(&k1).unwrap();
            assert_eq!(&g1.guard.mmap[g1.offset..g1.offset + g1.len], v1.as_ref());
            let g2 = cache.get(&k2).unwrap();
            assert_eq!(&g2.guard.mmap[g2.offset..g2.offset + g2.len], v2.as_ref());
        }

        // Offline device 0. Its keys should migrate to device 1
        let evicted = cache.offline_device(0).await;
        assert!(evicted.is_empty());

        // Both keys should still be readable
        assert!(cache.get(&k1).is_some());
        assert!(cache.get(&k2).is_some());

        // Online device 0 again. Rebalancing should run
        cache.online_device(0, dir1.path(), 1000).await.unwrap();

        assert!(cache.get(&k1).is_some());
        assert!(cache.get(&k2).is_some());
    }
}
