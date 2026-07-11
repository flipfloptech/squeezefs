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
    /// IN-FLIGHT placements: extents whose payload copy runs OUTSIDE the
    /// shard lock and which are not yet indexed in `map`. Every placement
    /// decision (first-fit `fits`, put's cursor walk, eviction targets)
    /// must treat these as occupied: a concurrent writer that reuses an
    /// in-flight extent (wrap-around or a punched hole next to it) rewrites
    /// the bytes BEFORE the first writer's phase-2 index insert, and the
    /// first key then serves the second key's payload verbatim — observed
    /// live as a staged file reading back another file's whole fill under
    /// capacity churn. Pushed in phase 1 (locked), removed in phase 2
    /// (locked).
    pending: Vec<(usize, usize)>,
}

impl NvmeShardInner {
    fn overlaps(w_start: usize, w_end: usize, b_start: usize, b_end: usize) -> bool {
        w_start < b_end && w_end > b_start
    }

    /// Remove every live entry overlapping `[w_start, w_end)` from the
    /// index. With `collect = Some(vec)` each victim's payload is copied out
    /// of the mmap into the vec (an mmap page-in + memcpy of the full value,
    /// under the caller-held write lock); with `None` victims are dropped
    /// index-only — no victim byte is ever touched. The hot read-cache put
    /// path discards evictions, so it must use `None` (the copy was 44% of
    /// daemon CPU + ~6x spurious device reads on the elbencho O_DIRECT
    /// sequential-read row).
    fn evict_overlapping(
        &mut self,
        w_start: usize,
        w_end: usize,
        mut collect: Option<&mut Vec<(Bytes, Bytes)>>,
    ) {
        // GEOMETRY-COMPLETE eviction: remove EVERY live entry overlapping
        // the placement range. The historical front-run walk over
        // `active_keys` assumed queue order == ring-position order; a
        // same-key replace (remove + push_back) and out-of-order concurrent
        // placements both break that, so the walk stopped at the first
        // non-overlapping FRONT entry while an overlapping live entry sat
        // deeper in the queue — the caller's memcpy then CLOBBERED the
        // still-indexed entry's bytes, and readers served another block's
        // content under a perfectly valid key + incarnation + binding (the
        // generic/074 fstest.3 stale-fill corruption; budget-dependent
        // because only wrapping shards evict). The map is the authority;
        // the queue is bookkeeping.
        let victims: Vec<Bytes> = self
            .map
            .iter()
            .filter(|(_, m)| Self::overlaps(w_start, w_end, m.offset, m.offset + m.len))
            .map(|(k, _)| k.clone())
            .collect();
        for key in victims {
            let Some(meta) = self.map.remove(&key) else {
                continue;
            };
            self.active_keys.retain(|k| k != &key);

            let Some(evicted) = collect.as_deref_mut() else {
                continue;
            };
            let b_start = meta.offset;
            let val_len = self.get_val_len_at(b_start);
            if meta.len < HEADER_SIZE + val_len {
                continue;
            }
            let k_end = b_start + HEADER_SIZE + (meta.len - HEADER_SIZE - val_len);
            if k_end > self.mmap.len() || b_start + meta.len > self.mmap.len() {
                continue;
            }
            let v_start = k_end;
            let v_end = b_start + meta.len;
            let val_bytes = Bytes::copy_from_slice(&self.mmap[v_start..v_end]);
            evicted.push((key, val_bytes));
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

/// Owned (`'static`) read guard over one shard's mmap value slice, backed by
/// a cloned `Arc<NvmeDevice>` keep-alive.
///
/// **Hold discipline (zero-copy write-path design §5.5, risk R3).** While a
/// guard lives, its shard's write lock is unacquirable: every same-shard
/// writer/evictor (`put` / `reserve_and_write` / `remove`) waits, and a task
/// that takes a same-shard write lock while *itself* holding the guard
/// self-deadlocks (parking_lot read→write upgrade). Two sanctioned holders,
/// both bounded:
///
/// - **Read replies**: the guard rides as the reply backing across one FUSE
///   reply (the read-path precedent).
/// - **Write-only staged flush** (`cache::nvme::StagedDmaSource`, consumed
///   by value by `cache::nvme::write_block_from_staging`): held across
///   exactly one crypto transform or one DMA — plus the sampled read-back
///   verify on `--write-verification` mounts — and provably dead before any
///   same-shard mutation (`remove_active_block`). A guard-backed `Bytes`
///   must never enter a cache: LRU entries have unbounded lifetime and
///   would pin the shard until eviction.
///
/// **Pre-agreed fallback** (§5.5): if the bounded hold ever shows up in
/// staging eviction-wait gauges, replace the shard read lock held across the
/// DMA with a per-entry pin count (`AtomicU32` in `BlockMeta`; evictors skip
/// pinned entries) — same externally visible bound, finer blocking scope.
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

/// One mmap segment + its index, guarded by a parking_lot `RwLock`.
///
/// **Shard-lock acquisition invariant (the Hang-1 CFR wedge fix).**
/// parking_lot's `RwLock` is WRITER-PREFERRING: once a writer is queued, a
/// plain `read()` PARKS the calling thread. §5.5 read guards
/// ([`NvmeCacheReadGuard`]) are held across awaits (DMA, reply), so a plain
/// read on an async executor thread closes a dependency cycle: the parked
/// executor can no longer poll the guard-holding future the queued writer
/// is waiting on — the observed total-daemon wedge (fsx `copy_file_range`;
/// gdb: handler thread in `lock_shared` under `LocalSet::tick`,
/// blocking-pool writer in `wait_for_readers`). Rules, enforced across this
/// module and pinned by `tests/staging_shard_deadlock_tests.rs`:
///
/// 1. **Reads never park behind a QUEUED writer** — every shared
///    acquisition uses `read_recursive()`. An executor thread then waits
///    only for an ACTIVE writer's bounded, executor-independent critical
///    section (a sync index/memcpy op on a blocking-pool thread).
/// 2. **Writers never run on async executor threads** — `put` /
///    `reserve_and_write` / `remove` reach the staging cache via
///    `spawn_blocking` (see `cache::nvme::NvmeStaging`), because a writer
///    legitimately waits for §5.5 guards with await-side lifetimes.
///
/// Writer starvation is not a concern: a writer becomes ACTIVE as soon as
/// the reader count gaps to zero, non-guard reads are µs-scale, and §5.5
/// guards are bounded (one transform-or-DMA / one reply).
pub struct NvmeShard {
    inner: RwLock<NvmeShardInner>,
    _file: Option<File>, // Keep file handle alive if file-backed
}

impl NvmeShard {
    /// Return a DEAD extent's pages to the OS. Superseded/removed entries
    /// are tombstoned but their pages otherwise stay mapped-dirty: first-fit
    /// placement walks the segment, so a re-stage churn drags resident
    /// memory toward the whole segment size — unreclaimable when the
    /// staging dir sits on tmpfs (the observed ~24 MB/min fsx churn creep,
    /// OOM-killing capped daemons). File-backed shards punch a hole
    /// (`FALLOC_FL_PUNCH_HOLE` frees page cache AND tmpfs blocks); anonymous
    /// shards `madvise(MADV_DONTNEED)`. Both leave the range reading zeros —
    /// no `BLOCK_MAGIC`, so `recover_index` never resurrects the dead copy
    /// (strictly better than the 4-byte tombstone alone).
    ///
    /// MUST be called with the shard write lock held: §5.5 read guards on
    /// the dying extent hold the shard read lock, so the caller's write lock
    /// proves no reader is inside the range.
    fn reclaim_extent(&self, inner: &mut NvmeShardInner, offset: usize, len: usize) {
        if len == 0 {
            return;
        }
        debug_assert!(offset + len <= inner.capacity);
        if let Some(ref file) = self._file {
            use std::os::unix::io::AsRawFd;
            let res = unsafe {
                libc::fallocate(
                    file.as_raw_fd(),
                    libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                    offset as libc::off_t,
                    len as libc::off_t,
                )
            };
            if res == 0 {
                return;
            }
            // Filesystem without punch support: fall through to the
            // tombstone + madvise path below.
        }
        // Zero the header so recovery can never decode the dead copy, then
        // drop the page range (anon: frees pages; file-backed fallback:
        // best-effort PTE drop).
        inner.mmap[offset..offset + HEADER_SIZE.min(len)].fill(0);
        let page = 4096usize;
        let aligned_start = offset.div_ceil(page) * page;
        let aligned_end = ((offset + len) / page) * page;
        if aligned_end > aligned_start {
            unsafe {
                libc::madvise(
                    inner.mmap.as_ptr().add(aligned_start) as *mut libc::c_void,
                    aligned_end - aligned_start,
                    libc::MADV_DONTNEED,
                );
            }
        }
    }

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
                pending: Vec::new(),
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
                pending: Vec::new(),
            }),
            _file: None,
        })
    }

    pub fn get<'a>(&'a self, key: &Bytes) -> Option<NvmeReadGuard<'a>> {
        // read_recursive: never park behind a queued writer (shard-lock
        // invariant — see the `NvmeShard` doc).
        let inner = self.inner.read_recursive();
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
        self.put_impl(key, value, true)
    }

    /// [`Self::put`] for callers that discard evictions (the read-cache hot
    /// path): identical placement/eviction/index semantics, but victims are
    /// dropped index-only — no mmap page-in, no memcpy, nothing returned.
    pub fn put_discard_evicted(&self, key: Bytes, value: Bytes) {
        let _ = self.put_impl(key, value, false);
    }

    fn put_impl(&self, key: Bytes, value: Bytes, materialize_evicted: bool) -> Vec<(Bytes, Bytes)> {
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
                if materialize_evicted {
                    inner.evict_overlapping(target_offset, capacity, Some(&mut evicted));
                } else {
                    inner.evict_overlapping(target_offset, capacity, None);
                }
                target_offset = 0;
            }

            // In-flight placements are UNTOUCHABLE (see the `pending` field
            // doc): bump the cursor past any overlapping one — their copies
            // land outside the lock and eviction cannot see them. Bounded:
            // on a pathological layout, skip the put (a cache put is
            // best-effort; callers treat a miss as a refetch).
            let mut bumps = inner.pending.len() + 2;
            loop {
                let conflict = inner
                    .pending
                    .iter()
                    .copied()
                    .find(|&(s, e)| target_offset < e && target_offset + block_size > s);
                let Some((_, conflict_end)) = conflict else {
                    break;
                };
                if bumps == 0 {
                    return Vec::new();
                }
                bumps -= 1;
                target_offset = (conflict_end + alignment - 1) & !(alignment - 1);
                if target_offset + block_size > capacity {
                    if materialize_evicted {
                        inner.evict_overlapping(target_offset, capacity, Some(&mut evicted));
                    } else {
                        inner.evict_overlapping(target_offset, capacity, None);
                    }
                    target_offset = 0;
                }
            }

            // Recompute after wrap-around to ensure aligned
            let target_offset = (target_offset + alignment - 1) & !(alignment - 1);
            let val_offset =
                (target_offset + HEADER_SIZE + key_len + alignment - 1) & !(alignment - 1);
            let block_size = (val_offset - target_offset) + val_len;

            // Evict overlapping blocks in the target range
            if materialize_evicted {
                inner.evict_overlapping(
                    target_offset,
                    target_offset + block_size,
                    Some(&mut evicted),
                );
            } else {
                inner.evict_overlapping(target_offset, target_offset + block_size, None);
            }

            // Invalidate/remove key from map if it already existed
            let old_meta = inner.map.remove(&key);
            if old_meta.is_some() {
                inner.active_keys.retain(|k| k != &key);
            }

            // Reserve the in-flight extent against concurrent placements.
            inner
                .pending
                .push((target_offset, target_offset + block_size));
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
            inner.pending.retain(|&(s, _)| s != target_offset);
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
            // the sole copy in place would be torn by a crash mid-write)
            // PLUS in-flight placements (`pending`): their copies run
            // outside the lock and are not yet indexed — reusing one would
            // rewrite bytes another key is about to publish (see the
            // `pending` field doc).
            let mut live: Vec<(usize, usize)> = inner
                .map
                .values()
                .map(|m| (m.offset, m.offset + m.len))
                .chain(inner.pending.iter().copied())
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

            // Same-key replace is ATOMIC for readers: the existing entry
            // STAYS in the index (and in `live`, so the new placement never
            // overlaps it) while the replacement is copied. The index flips
            // old→new in phase 2 under the write lock — a concurrent `get`
            // resolves one intact copy at every instant, never None (the
            // fstests 074/127/616 transient-zeros window: a read that
            // missed here fell into the zeros-degrade leg).
            let old_meta = inner.map.get(&key).copied();

            // Reserve the in-flight extent against concurrent placements.
            inner
                .pending
                .push((target_offset, target_offset + block_size));
            inner.write_offset = target_offset + block_size;
            let mmap_ptr = inner.mmap.as_ptr() as *mut u8;
            (target_offset, val_offset, block_size, old_meta, mmap_ptr)
        };

        // Perform memory copy and disk flushing OUTSIDE of the lock!
        //
        // SAFETY of the unlocked copy: the target range overlaps no live
        // extent (`fits`), including the old copy of `key` (still indexed),
        // and concurrent placements serialize on the write lock with this
        // range reserved via `write_offset`/`map` once phase 2 lands — see
        // the placement comment above. Readers can only reach the range
        // after phase 2 publishes it.
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

            libc::msync(
                mmap_ptr.add(target_offset) as *mut libc::c_void,
                block_size,
                libc::MS_ASYNC,
            );
        }

        // Phase 2: flip the index to the fully-written replacement, then
        // reclaim the superseded copy — both under the write lock, so no
        // reader holds a guard on the old extent (guards pin the read lock)
        // and no `get` window exists where the key is absent. Crash story
        // unchanged: the old copy dies only after the new one is complete
        // (a crash in between leaves duplicates; `recover_index` dedupes
        // last-wins).
        {
            let mut inner = self.inner.write();
            let meta = BlockMeta {
                offset: target_offset,
                len: block_size,
            };
            inner.pending.retain(|&(s, _)| s != target_offset);
            if inner.map.insert(key.clone(), meta).is_some() {
                inner.active_keys.retain(|k| k != &key);
            }
            inner.active_keys.push_back(key);
            if let Some(old) = old_meta {
                self.reclaim_extent(&mut inner, old.offset, old.len);
            }
        }

        true
    }

    /// Whether `key` currently has a live entry in this shard.
    pub fn has_key(&self, key: &Bytes) -> bool {
        self.inner.read_recursive().map.contains_key(key)
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

            // Overwrite magic to invalidate on disk, then return the dead
            // extent's pages to the OS (RSS creep fix — see reclaim_extent).
            inner.mmap[meta.offset..meta.offset + 4].copy_from_slice(&0u32.to_le_bytes());
            let _ = inner.mmap.flush_range(meta.offset, 4);
            self.reclaim_extent(&mut inner, meta.offset, meta.len);

            Some(val)
        } else {
            None
        }
    }

    pub fn active_keys(&self) -> Vec<Bytes> {
        self.inner
            .read_recursive()
            .active_keys
            .iter()
            .cloned()
            .collect()
    }

    pub fn current_bytes(&self) -> usize {
        self.inner
            .read_recursive()
            .map
            .values()
            .map(|m| m.len)
            .sum()
    }

    /// Rebuild the in-RAM index from the mmapped segment after a crash
    /// remount, using the WRITERS' true block geometry ([`Self::put`] /
    /// [`Self::reserve_and_write`]): blocks start at alignment boundaries,
    /// the value starts at the next alignment boundary past header+key,
    /// and the footprint includes that gap. Two crash-consistency rules:
    ///
    /// - **Aligned candidates only.** Writers only ever place headers at
    ///   alignment boundaries, and a recovered entry's whole footprint is
    ///   skipped, so value interiors (arbitrary payload bytes that can
    ///   alias `BLOCK_MAGIC`) are never interpreted as headers. The
    ///   historical byte-wise crawl fabricated entries from payload bytes
    ///   and then skipped REAL entries — a lost staged payload (EIO class).
    /// - **True footprints.** The historical scan recorded
    ///   `header+key+value` without the alignment gap, so first-fit
    ///   placement after recovery landed new writes INSIDE recovered
    ///   values, clobbering the sole copy of staged data.
    ///
    /// Undecodable headers (torn appends, stale bytes aliasing the magic)
    /// are discarded loudly and the scan resyncs at the next candidate —
    /// per the D0 staging contract: recover what is intact, discard-and-log
    /// what is not, never fail the mount.
    pub fn recover_index(&self) {
        let mut inner = self.inner.write();
        let capacity = inner.capacity;
        let alignment = if capacity >= 4096 { 4096 } else { 1 };

        inner.map.clear();
        inner.active_keys.clear();

        let mut recovered = 0u64;
        let mut duplicates = 0u64;
        let mut dropped = 0u64;
        let mut max_end = 0usize;
        let mut offset = 0usize;
        while offset + HEADER_SIZE <= capacity {
            let magic = u32::from_le_bytes(inner.mmap[offset..offset + 4].try_into().unwrap());
            if magic != BLOCK_MAGIC {
                offset += alignment;
                continue;
            }
            let key_len =
                u32::from_le_bytes(inner.mmap[offset + 4..offset + 8].try_into().unwrap()) as usize;
            let val_len =
                u32::from_le_bytes(inner.mmap[offset + 8..offset + 12].try_into().unwrap())
                    as usize;
            // Writers' geometry: value at the next alignment boundary past
            // header+key; footprint spans the gap.
            let val_delta = (HEADER_SIZE + key_len + alignment - 1) & !(alignment - 1);
            let block_size = val_delta.saturating_add(val_len);
            let sane = key_len > 0
                && key_len <= capacity.saturating_sub(offset + HEADER_SIZE)
                && offset + block_size <= capacity;
            if !sane {
                dropped += 1;
                log::warn!(
                    "segment index recovery: dropping undecodable block header at offset \
                     {offset} (key_len={key_len}, val_len={val_len}, capacity={capacity}) — \
                     crash-torn or stale bytes; resyncing at the next candidate"
                );
                offset += alignment;
                continue;
            }
            let key_start = offset + HEADER_SIZE;
            let key = Bytes::copy_from_slice(&inner.mmap[key_start..key_start + key_len]);
            let meta = BlockMeta {
                offset,
                len: block_size,
            };
            if inner.map.insert(key.clone(), meta).is_some() {
                // Crash window between a replacement's full write and the
                // old copy's tombstone: both images verify. Keep the later
                // scan position deterministically (last-wins, matching the
                // historical index behavior); the value-shape parse at read
                // time arbitrates a torn survivor.
                duplicates += 1;
                inner.active_keys.retain(|k| k != &key);
            }
            inner.active_keys.push_back(key);
            recovered += 1;
            max_end = max_end.max(offset + block_size);
            // Skip the WHOLE footprint (aligned): value interiors are never
            // scanned for headers.
            offset = (offset + block_size + alignment - 1) & !(alignment - 1);
        }

        inner.write_offset = max_end;
        if dropped > 0 || duplicates > 0 {
            log::warn!(
                "segment index recovery: recovered {recovered} entries, dropped {dropped} \
                 undecodable headers, {duplicates} crash-window duplicate keys (last-wins)"
            );
        } else {
            log::debug!("segment index recovery: recovered {recovered} entries");
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
                // read_recursive: never park behind a queued writer (shard-
                // lock invariant — see the `NvmeShard` doc). This is the
                // §5.5 guard acquisition itself AND the executor-side probe
                // that closed the Hang-1 cycle when it parked.
                let inner = shard.inner.read_recursive();
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
        match self.route_put(&key) {
            Some((dev, shard_idx)) => dev.shards[shard_idx].put(key, value),
            None => Vec::new(),
        }
    }

    /// [`Self::put`] for callers that discard evictions (the read-cache hot
    /// path, `cache_read_block`): same device affinity and eviction
    /// semantics, but victims are dropped index-only — never paged in,
    /// copied, or returned.
    pub fn put_discard_evicted(&self, key: Bytes, value: Bytes) {
        if let Some((dev, shard_idx)) = self.route_put(&key) {
            dev.shards[shard_idx].put_discard_evicted(key, value);
        }
    }

    /// Shared placement routing for the put flavors. Key-affine: replace an
    /// existing copy on its own device — readers scan devices in order, so a
    /// round-robin re-put of a resident key would leave a divergent stale
    /// duplicate that survives remove(). New keys spread round-robin.
    fn route_put(&self, key: &Bytes) -> Option<(Arc<NvmeDevice>, usize)> {
        let active_devices = {
            let guard = self.devices.read();
            guard
                .iter()
                .filter(|d| d.online.load(Ordering::Relaxed))
                .cloned()
                .collect::<Vec<_>>()
        };

        if active_devices.is_empty() {
            return None;
        }

        for dev in &active_devices {
            let shard_idx = (xxh3_64(key) as usize) % dev.shards.len();
            if dev.shards[shard_idx].has_key(key) {
                return Some((dev.clone(), shard_idx));
            }
        }

        let idx = self.write_counter.fetch_add(1, Ordering::Relaxed) % active_devices.len();
        let dev = active_devices[idx].clone();
        let shard_idx = (xxh3_64(key) as usize) % dev.shards.len();
        Some((dev, shard_idx))
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

    /// Remove `key` from EVERY device (pre-affinity rings can hold
    /// duplicates; a purge must not leave a stale survivor). Returns the
    /// most recent value found, favoring later devices only after earlier
    /// ones are cleared.
    pub fn remove(&self, key: &Bytes) -> Option<Bytes> {
        let devices = self.devices.read();
        let mut removed = None;
        for dev in devices.iter() {
            let shard_idx = (xxh3_64(key) as usize) % dev.shards.len();
            if let Some(val) = dev.shards[shard_idx].remove(key) {
                removed.get_or_insert(val);
            }
        }
        removed
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
        // Sub-4096 capacities keep byte-alignment (>=4096 switches the ring
        // to 4 KiB block alignment and these tiny entries no longer fit).
        let cache = NvmeCache::new(&[dir1.path(), dir2.path()], &[2048, 2048], 1).unwrap();

        let key = Bytes::from("block_dup");
        let v1 = Bytes::from(vec![0x11u8; 64]);
        let v2 = Bytes::from(vec![0x22u8; 64]);

        // Interleave with other keys so the round-robin counter points at a
        // DIFFERENT device for the re-put of the same key (counter: key->A,
        // filler_a->B, filler_b->A, re-put key->B without affinity).
        cache.put(key.clone(), v1.clone());
        cache.put(Bytes::from("filler_a"), Bytes::from(vec![0u8; 16]));
        cache.put(Bytes::from("filler_b"), Bytes::from(vec![0u8; 16]));
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

    /// The read-cache hot path (`cache_read_block`) discards `put`'s evicted
    /// vec, yet the ring still materialized every victim payload — a 4 MiB
    /// mmap page-in + memcpy per eviction, INSIDE the shard write lock
    /// (readers of that shard stall behind it; measured 44% daemon CPU in
    /// memcpy + ~100 GiB/16 GiB spurious device reads on the elbencho
    /// O_DIRECT read row). `put_discard_evicted` must keep the exact same
    /// index/eviction semantics as `put` while never touching victim bytes.
    #[test]
    fn test_put_discard_evicted_same_eviction_semantics_no_materialize() {
        let dir = tempdir().unwrap();
        // 200 bytes capacity, 1 shard: FIFO ring identical to
        // test_nvme_cache_eviction_ring so semantics stay comparable.
        let cache = NvmeCache::new(&[dir.path()], &[200], 1).unwrap();

        let k1 = Bytes::from("block1");
        let v1 = Bytes::from(vec![1u8; 80]);
        let k2 = Bytes::from("block2");
        let v2 = Bytes::from(vec![2u8; 80]);
        let k3 = Bytes::from("block3");
        let v3 = Bytes::from(vec![3u8; 80]);

        cache.put_discard_evicted(k1.clone(), v1.clone());
        cache.put_discard_evicted(k2.clone(), v2.clone());
        // Third put wraps and must evict block1 from the index — without
        // returning (or reading) its payload.
        cache.put_discard_evicted(k3.clone(), v3.clone());

        assert!(
            cache.get(&k1).is_none(),
            "victim must leave the index exactly as with materializing put"
        );
        let g2 = cache.get(&k2).expect("survivor entry must stay readable");
        assert_eq!(&g2.guard.mmap[g2.offset..g2.offset + g2.len], v2.as_ref());
        let g3 = cache.get(&k3).expect("new entry must be readable");
        assert_eq!(&g3.guard.mmap[g3.offset..g3.offset + g3.len], v3.as_ref());
    }

    /// Device affinity must be identical between the two put flavors: a
    /// re-put of a resident key through `put_discard_evicted` replaces the
    /// copy on its OWN device (no divergent stale duplicate on another
    /// rail), and `remove` purges it everywhere.
    #[test]
    fn test_put_discard_evicted_keeps_key_affinity_no_stale_duplicate() {
        let dir1 = tempdir().unwrap();
        let dir2 = tempdir().unwrap();
        let cache = NvmeCache::new(&[dir1.path(), dir2.path()], &[2048, 2048], 1).unwrap();

        let key = Bytes::from("block_dup");
        let v1 = Bytes::from(vec![0x11u8; 64]);
        let v2 = Bytes::from(vec![0x22u8; 64]);

        // Interleave so the round-robin counter would point elsewhere for
        // the re-put (same shape as the materializing-put affinity test).
        cache.put_discard_evicted(key.clone(), v1.clone());
        cache.put_discard_evicted(Bytes::from("filler_a"), Bytes::from(vec![0u8; 16]));
        cache.put_discard_evicted(Bytes::from("filler_b"), Bytes::from(vec![0u8; 16]));
        cache.put_discard_evicted(key.clone(), v2.clone());

        {
            let g = cache.get(&key).expect("key must be resident");
            assert_eq!(
                &g.guard.mmap[g.offset..g.offset + g.len],
                v2.as_ref(),
                "get served a stale duplicate from another device"
            );
        }

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
