use crate::error::{Result, SqueezefsError};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zerocopy::{FromBytes, Immutable, IntoBytes};

pub const MAGIC_VALUE: &[u8; 8] = b"METALV01";
pub const SECTOR_SIZE: usize = 4096;

/// Number of shards in the per-sector lock array. Matches `active_inode_locks` /
/// `BLOCK_FLUSH_LOCKS` (`fuse_client`). See the transaction_lock-removal design.
pub const SECTOR_LOCK_SHARDS: usize = 4096;

// The sector-sharded commit (design PR 4) is the sole writer model since PR 8
// deleted the legacy `transaction_lock` path and its `SQUEEZEFS_META_SECTOR_LOCKS`
// rollback flag; rollback is now `git revert` of the PR chain.

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
    /// Lock-free in-RAM inode allocator — the sole allocator (derived from the
    /// inode table; seeded on mount) — see `crate::meta_backend::alloc`.
    pub inode_alloc: Arc<crate::meta_backend::alloc::InodeAllocator>,
    /// Per-4KiB-sector RwLock array (keyed by sector-aligned byte offset).
    /// Read guard = consistent non-tx reads; write guard = commit-time RMW+apply.
    /// Wired into the sector-sharded commit in PR 4; constructed here (PR 3).
    pub sector_locks:
        Arc<crate::stripe_locks::StripeLocks<tokio::sync::RwLock<()>, SECTOR_LOCK_SHARDS>>,
    /// Per-bucket lock protecting the in-RAM dentry chain index (design §3.6).
    /// Held from a dentry op's chain-read **through** the post-commit in-RAM
    /// index apply, so concurrent same-bucket ops cannot traverse a stale chain.
    /// Inner `Arc<Mutex>` so a transaction can take an **owned** guard and carry
    /// it (via [`TX_STATE`]) from the closure across the commit.
    pub dentry_bucket_locks:
        Arc<crate::stripe_locks::StripeLocks<Arc<tokio::sync::Mutex<()>>, SECTOR_LOCK_SHARDS>>,
    pub dentry_index: Arc<scc::HashMap<u64, Vec<(u64, crate::meta_backend::dentry::DiskDentry)>>>,
    pub dentry_by_offset: Arc<scc::HashMap<u64, (u64, crate::meta_backend::dentry::DiskDentry)>>,
    pub dentry_occupied_offsets: Arc<std::sync::Mutex<std::collections::HashSet<u64>>>,
    pub dentry_index_initialized: Arc<tokio::sync::OnceCell<()>>,
}

tokio::task_local! {
    pub static ACTIVE_TX: std::sync::Arc<std::sync::Mutex<Vec<(std::path::PathBuf, u64, Vec<u8>)>>>;
    pub static FORCE_SYNC_TX: bool;
    /// Per-transaction lock + rollback state for the sector-sharded commit.
    /// Set alongside [`ACTIVE_TX`] by `run_transaction`. Carries the
    /// owned dentry-bucket guards (held closure→post-commit), the in-RAM index
    /// undo log (restored on commit failure so the index never diverges from
    /// disk, design R4), and the inodes allocated this tx (freed on failure,
    /// design §3.5).
    pub static TX_STATE: std::sync::Arc<std::sync::Mutex<TxState>>;
}

/// Per-transaction mutable state for the sector-sharded commit (see [`TX_STATE`]).
#[derive(Default)]
pub struct TxState {
    /// Owned dentry-bucket guards, keyed by **shard index** (not bucket number)
    /// and deduped, held from the closure's chain-read through the post-commit
    /// in-RAM index apply. Keying by shard index prevents re-locking a shard two
    /// distinct buckets happen to share (self-deadlock on the non-reentrant lock).
    pub bucket_guards: std::collections::HashMap<usize, tokio::sync::OwnedMutexGuard<()>>,
    /// Pre-transaction snapshot of touched `dentry_index` entries (first-write-
    /// wins). `None` == the key was absent. Restored verbatim on commit failure.
    pub undo_index:
        std::collections::HashMap<u64, Option<Vec<(u64, crate::meta_backend::dentry::DiskDentry)>>>,
    /// Pre-transaction snapshot of touched `dentry_by_offset` entries.
    pub undo_by_offset:
        std::collections::HashMap<u64, Option<(u64, crate::meta_backend::dentry::DiskDentry)>>,
    /// Pre-transaction membership of touched `dentry_occupied_offsets` slots.
    pub undo_occupied: std::collections::HashMap<u64, bool>,
    /// Inodes allocated by this transaction; freed on commit failure so a leaked
    /// bit does not survive to the next mount reconciliation (design §3.5).
    pub allocated_inos: Vec<u64>,
}

/// Number of inode slots a metadata volume of `size` bytes can hold. The xattr
/// region begins at 72 MiB and each inode reserves a 32 KiB xattr block, so the
/// inode count is `(size - 72 MiB) / 32 KiB`. Shared by `max_inodes` and the
/// inode-allocator sizing in `open` so both agree on the range.
pub(crate) fn inodes_for_size(size: u64) -> u64 {
    const XATTR_BLOCK_START: u64 = 1024 * 1024 * 72;
    if size > XATTR_BLOCK_START {
        (size - XATTR_BLOCK_START) / 32768
    } else {
        0
    }
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

        // Effective device size after any set_len above; sizes the inode
        // allocator (min(20000, max_inodes)).
        let effective_size =
            if !meta.file_type().is_block_device() && dev_size < size_limit && size_limit > 0 {
                size_limit
            } else {
                dev_size
            };
        let inode_limit = std::cmp::min(20000, inodes_for_size(effective_size));

        let storage = Self {
            path: path_ref.to_path_buf(),
            inode_alloc: Arc::new(crate::meta_backend::alloc::InodeAllocator::new(inode_limit)),
            sector_locks: Arc::new(crate::stripe_locks::StripeLocks::new()),
            dentry_bucket_locks: Arc::new(crate::stripe_locks::StripeLocks::new()),
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

    /// Ascending-deduped sector-lock shard indices covering
    /// `[offset, offset + len)` — the commit protocol's acquisition order
    /// (design §3.3, [`crate::stripe_locks::StripeLocks::shard_index`]): the
    /// fixed stripe array means distinct sectors can share a shard, so the only
    /// valid total order is ascending shard index over deduped instances.
    pub fn sector_shard_indices(&self, offset: u64, len: usize) -> Vec<usize> {
        let end = offset + len as u64;
        let mut sector = Self::sector_of(offset);
        let mut indices = Vec::new();
        while sector < end {
            indices.push(self.sector_locks.shard_index(sector));
            sector += SECTOR_SIZE as u64;
        }
        indices.sort_unstable();
        indices.dedup();
        indices
    }

    /// Write guards over every sector shard intersecting `[offset, offset+len)`,
    /// acquired in ascending shard-index order (mutually exclusive with the
    /// sector-sharded commit and with same-range readers/writers).
    /// multi-sector direct RMWs (xattr blocks, superblock) hold these across
    /// their read→modify→write.
    pub async fn lock_sectors_write(
        &self,
        offset: u64,
        len: usize,
    ) -> Vec<tokio::sync::RwLockWriteGuard<'_, ()>> {
        let mut guards = Vec::new();
        for idx in self.sector_shard_indices(offset, len) {
            guards.push(self.sector_locks.get_by_index(idx).write().await);
        }
        guards
    }

    /// Read guards over every sector shard intersecting `[offset, offset+len)`
    /// (same order as [`Self::lock_sectors_write`]) — a consistent, untorn view
    /// of a multi-sector region against concurrent commits/direct writers.
    pub async fn lock_sectors_read(
        &self,
        offset: u64,
        len: usize,
    ) -> Vec<tokio::sync::RwLockReadGuard<'_, ()>> {
        let mut guards = Vec::new();
        for idx in self.sector_shard_indices(offset, len) {
            guards.push(self.sector_locks.get_by_index(idx).read().await);
        }
        guards
    }

    /// Read the Superblock at offset 0
    ///
    /// Sector-0 **read** lock: the superblock is one sector in the sector
    /// scheme; commits that stage superblock patches lock the same shard.
    pub async fn read_superblock(&self) -> Result<Superblock> {
        let _sector = self.sector_lock(0).read().await;
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
    ///
    /// Sector-0 **write** lock — mutually exclusive with readers and with
    /// commits applying staged superblock patches (invariant R1: no mutator
    /// writes a sector outside its sector lock).
    pub async fn write_superblock(&self, sb: &Superblock) -> Result<()> {
        let _sector = self.sector_lock(0).write().await;
        let mut buf = [0u8; SECTOR_SIZE];
        let sb_bytes = sb.as_bytes();
        buf[..sb_bytes.len()].copy_from_slice(sb_bytes);

        crate::uring_fs::write_at(&self.path, 0, bytes::Bytes::copy_from_slice(&buf)).await?;
        Ok(())
    }

    /// Sector-aligned byte offset containing `offset`.
    #[inline]
    pub fn sector_of(offset: u64) -> u64 {
        offset & !(SECTOR_SIZE as u64 - 1)
    }

    /// The per-sector RwLock for `sector_offset` (a splitmix-striped array).
    /// Read guard = consistent non-tx reads; write guard = commit-time RMW+apply.
    #[inline]
    pub fn sector_lock(&self, sector_offset: u64) -> &tokio::sync::RwLock<()> {
        self.sector_locks.get_inode_lock(sector_offset)
    }

    /// The (cloneable) per-bucket dentry-chain lock for `bucket`.
    #[inline]
    pub fn dentry_bucket_lock(&self, bucket: u64) -> Arc<tokio::sync::Mutex<()>> {
        self.dentry_bucket_locks.get_inode_lock(bucket).clone()
    }

    /// Acquire the dentry-bucket lock for `bucket` as an **owned** guard held by
    /// the current transaction ([`TX_STATE`]) from now through the post-commit
    /// in-RAM index apply. Idempotent per bucket (dedup) so it is safe to call
    /// from multiple dentry ops in one transaction and is reentrancy-safe against
    /// the non-reentrant tokio mutex. Errors if called outside a sector-locked
    /// transaction (a logic error — in-tx dentry ops only).
    pub async fn tx_acquire_bucket(&self, bucket: u64) -> Result<()> {
        let state = TX_STATE.try_with(|s| s.clone()).map_err(|_| {
            SqueezefsError::InvalidOperation(
                "tx_acquire_bucket called outside a sector-locked transaction".to_string(),
            )
        })?;
        // Dedup by shard index (distinct buckets may share a shard; re-locking it
        // would self-deadlock). Check-then-acquire without holding the std mutex
        // across the await (single task per tx, so `state` has no concurrent user).
        let shard = self.dentry_bucket_locks.shard_index(bucket);
        let already = state.lock().unwrap().bucket_guards.contains_key(&shard);
        if already {
            return Ok(());
        }
        let guard = self
            .dentry_bucket_locks
            .get_by_index(shard)
            .clone()
            .lock_owned()
            .await;
        state.lock().unwrap().bucket_guards.insert(shard, guard);
        Ok(())
    }

    /// Snapshot the pre-transaction value of `dentry_index[parent]` (once), for
    /// rollback on commit failure. No-op outside a transaction.
    pub fn tx_snapshot_index(&self, parent: u64) {
        let _ = TX_STATE.try_with(|s| {
            let mut st = s.lock().unwrap();
            if !st.undo_index.contains_key(&parent) {
                let cur = self.dentry_index.read_sync(&parent, |_, v| v.clone());
                st.undo_index.insert(parent, cur);
            }
        });
    }

    /// Snapshot the pre-transaction value of `dentry_by_offset[offset]` (once).
    pub fn tx_snapshot_by_offset(&self, offset: u64) {
        let _ = TX_STATE.try_with(|s| {
            let mut st = s.lock().unwrap();
            if !st.undo_by_offset.contains_key(&offset) {
                let cur = self.dentry_by_offset.read_sync(&offset, |_, v| *v);
                st.undo_by_offset.insert(offset, cur);
            }
        });
    }

    /// Snapshot whether `offset` is currently in `dentry_occupied_offsets` (once).
    pub fn tx_snapshot_occupied(&self, offset: u64) {
        let _ = TX_STATE.try_with(|s| {
            let mut st = s.lock().unwrap();
            if let std::collections::hash_map::Entry::Vacant(e) = st.undo_occupied.entry(offset) {
                let present = self
                    .dentry_occupied_offsets
                    .lock()
                    .unwrap()
                    .contains(&offset);
                e.insert(present);
            }
        });
    }

    /// Record a known pre-transaction membership for `offset` in the occupied set
    /// (first-write-wins). Used when a slot is reserved while the occupied lock is
    /// already held (so [`Self::tx_snapshot_occupied`], which re-reads current
    /// membership, would capture the post-reservation state). No-op outside a tx.
    pub fn tx_note_occupied_snapshot(&self, offset: u64, was_present: bool) {
        let _ = TX_STATE.try_with(|s| {
            s.lock()
                .unwrap()
                .undo_occupied
                .entry(offset)
                .or_insert(was_present);
        });
    }

    /// Record an inode allocated by this transaction so it is freed if the
    /// transaction fails after allocation (design §3.5). No-op outside a tx.
    pub fn tx_record_alloc(&self, ino: u64) {
        let _ = TX_STATE.try_with(|s| s.lock().unwrap().allocated_inos.push(ino));
    }

    /// Roll back the in-RAM dentry index + occupied set to their pre-transaction
    /// snapshots and free any inodes this transaction allocated. Called by
    /// `run_transaction` on commit failure while the bucket guards are still held
    /// (design R4 / §3.5). Restoring each key to its captured value is
    /// order-independent (each snapshot is an absolute pre-tx value).
    pub fn restore_tx_undo(&self, st: &TxState) {
        for (parent, snap) in &st.undo_index {
            match snap {
                Some(v) => match self.dentry_index.entry_sync(*parent) {
                    scc::hash_map::Entry::Occupied(mut o) => *o.get_mut() = v.clone(),
                    scc::hash_map::Entry::Vacant(vac) => {
                        vac.insert_entry(v.clone());
                    }
                },
                None => {
                    self.dentry_index.remove_sync(parent);
                }
            }
        }
        for (offset, snap) in &st.undo_by_offset {
            match snap {
                Some(v) => match self.dentry_by_offset.entry_sync(*offset) {
                    scc::hash_map::Entry::Occupied(mut o) => *o.get_mut() = *v,
                    scc::hash_map::Entry::Vacant(vac) => {
                        vac.insert_entry(*v);
                    }
                },
                None => {
                    self.dentry_by_offset.remove_sync(offset);
                }
            }
        }
        {
            let mut occ = self.dentry_occupied_offsets.lock().unwrap();
            for (offset, was_present) in &st.undo_occupied {
                if *was_present {
                    occ.insert(*offset);
                } else {
                    occ.remove(offset);
                }
            }
        }
        for ino in &st.allocated_inos {
            self.inode_alloc.free(*ino);
        }
    }

    /// Block read at a sector-aligned offset, with read-your-own-writes overlay.
    ///
    /// Reads the on-disk sector, then overlays **every** staged patch of the
    /// current transaction that intersects `[offset, offset+buf.len())`, applied
    /// in stage order (last writer wins per byte). This generalizes the previous
    /// "match a full-sector image at the exact offset" logic — for today's
    /// full-sector staging the result is identical, and it is forward-compatible
    /// with the sub-sector patch staging the sector-sharded commit introduces
    /// (design §3.4 / review Issue 6). Outside a transaction it is a plain direct
    /// read.
    pub async fn read_blocks(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if offset % SECTOR_SIZE as u64 != 0 {
            return Err(SqueezefsError::InvalidOperation(format!(
                "Read offset {} must be sector-aligned",
                offset
            )));
        }

        // Base image from disk (also the whole answer when not inside a tx).
        self.read_blocks_direct(offset, buf).await?;

        // Overlay this transaction's staged patches (read-your-own-writes).
        let rlo = offset;
        let rhi = offset + buf.len() as u64;
        let _ = ACTIVE_TX.try_with(|tx| {
            let guard = tx.lock().unwrap();
            for (path, off, data) in guard.iter() {
                if path != &self.path {
                    continue;
                }
                let lo = *off;
                let hi = off + data.len() as u64;
                if lo < rhi && rlo < hi {
                    // Intersection [s, e) in absolute byte coordinates.
                    let s = lo.max(rlo);
                    let e = hi.min(rhi);
                    buf[(s - rlo) as usize..(e - rlo) as usize]
                        .copy_from_slice(&data[(s - lo) as usize..(e - lo) as usize]);
                }
            }
        });
        Ok(())
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
        inodes_for_size(self.get_size()) as usize
    }

    /// Scan the on-disk inode table and invoke `visit(ino)` for every in-use
    /// (`magic == NODE`) slot in the allocatable range `[2, limit)`. Reads raw
    /// sectors directly; empty/invalid slots are skipped, never errored. Shared
    /// by allocator seeding and bitmap reconciliation so both derive from the
    /// same authoritative source (the inode table).
    async fn scan_used_inodes<F: FnMut(u64)>(&self, mut visit: F) -> Result<()> {
        use crate::meta_backend::alloc::FIRST_ALLOCATABLE_INO;
        use crate::meta_backend::inode::{
            DiskInode, INODES_PER_SECTOR, INODE_SLOT_SIZE, INODE_TABLE_START,
        };
        use zerocopy::IntoBytes;
        const NODE_MAGIC: u32 = 0x4E4F4445;

        let limit = self.inode_alloc.limit();
        if limit <= FIRST_ALLOCATABLE_INO {
            return Ok(());
        }
        let mut sector = [0u8; SECTOR_SIZE];
        let mut idx: u64 = 0;
        while idx < limit {
            let sector_offset =
                INODE_TABLE_START + (idx / INODES_PER_SECTOR as u64) * SECTOR_SIZE as u64;
            self.read_blocks_direct(sector_offset, &mut sector).await?;
            for slot in 0..INODES_PER_SECTOR {
                let cur = idx + slot as u64;
                if cur >= limit {
                    break;
                }
                if cur < FIRST_ALLOCATABLE_INO {
                    continue;
                }
                let base = slot * INODE_SLOT_SIZE;
                let mut di = DiskInode::new_zeroed();
                di.as_mut_bytes()
                    .copy_from_slice(&sector[base..base + INODE_SLOT_SIZE]);
                if di.magic == NODE_MAGIC {
                    visit(cur);
                }
            }
            idx += INODES_PER_SECTOR as u64;
        }
        Ok(())
    }

    /// Rebuild the in-RAM [`crate::meta_backend::alloc::InodeAllocator`] from the
    /// on-disk inode table (a bit set for every magic-valid slot). Called on
    /// mount before serving FUSE (design §3.9) so `alloc` never hands out a live
    /// inode number.
    pub async fn seed_inode_alloc_from_table(&self) -> Result<()> {
        self.scan_used_inodes(|ino| self.inode_alloc.set(ino)).await
    }

    /// Rewrite the on-disk free-inode bitmap sector (offset 4096) directly from
    /// the authoritative inode table. Run on mount and clean unmount so the
    /// on-disk bitmap stays consistent for pre-PR-8 binaries, which read it to
    /// allocate (design Key Decision 9 /
    /// review Issues 5, 18). Mirrors `format`'s convention of marking bits 0
    /// (reserved) and 1 (root) as allocated. Derives from the table — never from
    /// the in-RAM allocator — so it is correct regardless of which allocator ran
    /// during the session.
    pub async fn refresh_bitmap_from_table(&self) -> Result<()> {
        let mut bitmap = [0u8; SECTOR_SIZE];
        bitmap[0] = 0b0000_0011; // reserved index 0 + root (ino 1), matching `format`
        self.scan_used_inodes(|ino| {
            bitmap[(ino / 8) as usize] |= 1 << (ino % 8);
        })
        .await?;

        // §Observability (PR 7): bits the table-derived rebuild changed on disk
        // (leaked allocations healed, or table-backed bits the bitmap lost).
        // Non-zero on a clean mount is an investigate signal (design alerting).
        let mut prior = [0u8; SECTOR_SIZE];
        self.read_blocks_direct(4096, &mut prior).await?;
        let healed: u64 = prior
            .iter()
            .zip(bitmap.iter())
            .map(|(old, new)| (old ^ new).count_ones() as u64)
            .sum();
        if healed > 0 {
            crate::fuse_client::METRICS
                .meta_inode_alloc_reconciled
                .fetch_add(healed, std::sync::atomic::Ordering::Relaxed);
        }

        self.write_blocks_direct(4096, &bitmap).await
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
    /// Write `buf` at `offset`. Inside a transaction this **stages a patch**
    /// `(offset, buf)` (offset need not be sector-aligned — this is what allows
    /// sub-sector patches such as a 16-byte parent-timestamp update); outside a
    /// transaction it is a direct, sector-aligned write. The alignment invariant
    /// therefore lives on the direct path (`write_blocks_direct`), not on staging.
    pub async fn write_blocks(&self, offset: u64, buf: &[u8]) -> Result<()> {
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

    /// Direct sector-aligned writes as ONE uring-fs message: the entries fan
    /// out into parallel SQEs and the call returns when all have landed (any
    /// failure fails the whole batch). One commit = one queue round-trip
    /// instead of one per sector image / WAL record.
    pub async fn write_blocks_direct_batch(&self, ops: Vec<(u64, bytes::Bytes)>) -> Result<()> {
        for (offset, _) in &ops {
            if offset % SECTOR_SIZE as u64 != 0 {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "Write offset {} must be sector-aligned",
                    offset
                )));
            }
        }
        crate::uring_fs::write_at_batch(&self.path, ops).await
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
