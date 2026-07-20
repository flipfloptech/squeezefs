use crate::dlm::MetaClient;
use crate::error::Result;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

/// Physical allocation stride of every data-volume allocator: block
/// offsets are minted as `block_idx * CHUNK_SIZE`, so a stored block
/// image longer than this tramples the NEXT chunk's bytes on the device
/// (the FIND-RW4-A neighbor-corruption mechanism). Every producer of a
/// stored block image must satisfy `image_len <= CHUNK_SIZE` — see
/// [`ensure_stored_block_image_fits`].
pub const CHUNK_SIZE: u64 = 4 * 1024 * 1024;

/// FIND-RW4-A defense in depth: refuse — loudly, never by silent
/// truncation or a silent overflow write — any stored block image that
/// would exceed its allocator chunk. Post-fix geometry (the store-raw
/// escape + the format-time block-size headroom clamp + the mount
/// geometry gate) makes this unreachable on in-contract volumes; if it
/// fires, a write path produced an image the volume geometry cannot hold
/// and the write MUST fail rather than corrupt the neighboring chunk.
/// Deliberately an error, not an assert: a mis-geometried volume must
/// degrade to a loud EIO, never abort the daemon.
pub fn ensure_stored_block_image_fits(
    stored_len: usize,
    chunk_size: u64,
    context: &str,
) -> crate::error::Result<()> {
    if stored_len as u64 > chunk_size {
        let msg = format!(
            "stored block image ({stored_len} B) exceeds the {chunk_size} B allocator \
             chunk at {context}: refusing the write — landing it would corrupt the \
             neighboring chunk (FIND-RW4-A guard)"
        );
        log::error!("{msg}");
        return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            msg,
        )));
    }
    Ok(())
}

/// Outcome of [`BlockAllocator::pin_block_validated`] (§5.1 clone
/// validate-after-pin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinOutcome {
    /// Reference taken and the incarnation observed stable — the pin holds.
    Pinned,
    /// Reference taken but the incarnation is UNSTABLE (a patch may be
    /// mid-flight): the caller must unpin this block too, refetch the
    /// authoritative map, and retry.
    PinnedUnstable,
    /// Reference NOT taken (freed/untracked offset) — the caller must
    /// re-resolve, never proceed unpinned.
    Refused,
}

pub struct BlockAllocator {
    _client: Arc<MetaClient>,
    _volume_id: Box<str>,
    chunk_size: u64,
    free_blocks: dashmap::DashSet<u64>,
    highest_block: AtomicU64,
    /// Device capacity in whole chunks (0 = unbounded: offline tools /
    /// tests without a real device). Set at mount registration from the
    /// backing device/file size. `allocate_block` refuses to mint offsets
    /// past it: on a real block device the write would fail EIO/ENOSPC at
    /// DMA time; on a FILE-backed volume it silently GREW the file past
    /// its provisioned size — both discovered by the multi-volume bench
    /// EIO investigation.
    capacity_blocks: AtomicU64,
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
            chunk_size: CHUNK_SIZE,
            free_blocks: dashmap::DashSet::new(),
            highest_block: AtomicU64::new(0),
            capacity_blocks: AtomicU64::new(0),
            refcounts: scc::HashMap::new(),
            incarnations: scc::HashMap::new(),
        })
    }

    /// gen+1, stable=0 — offset owned by a writer whose data is not yet on the
    /// device (or retired by a free). Cache fills must not publish. Protocol
    /// core: [`crate::incarnation_core`] (loom-model-checked).
    fn mark_incarnation_unstable(&self, offset: u64) {
        match self.incarnations.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(occ) => {
                crate::incarnation_core::retire(occ.get());
            }
            scc::hash_map::Entry::Vacant(vac) => {
                let _ = vac.insert_entry(AtomicU64::new(crate::incarnation_core::UNSTABLE_FIRST));
            }
        }
    }

    /// Writer's durable device write for this incarnation completed — cache
    /// fills that observe an unchanged stable word may publish.
    pub fn publish_block(&self, offset: u64) {
        match self.incarnations.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(occ) => {
                crate::incarnation_core::publish(occ.get());
            }
            scc::hash_map::Entry::Vacant(vac) => {
                let _ = vac.insert_entry(AtomicU64::new(crate::incarnation_core::STABLE_FIRST));
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
            .read_sync(&offset, |_, v| crate::incarnation_core::snapshot(v))
        {
            Some(snap) => snap,
            None => Some(crate::incarnation_core::UNKNOWN_STABLE),
        }
    }

    /// True if the incarnation word is unchanged since [`Self::fill_incarnation`]
    /// (no allocate/publish/free transitioned the offset during the fill's
    /// device read).
    pub fn fill_incarnation_still(&self, offset: u64, before: u64) -> bool {
        match self
            .incarnations
            .read_sync(&offset, |_, v| crate::incarnation_core::still(v, before))
        {
            Some(unchanged) => unchanged,
            None => before == crate::incarnation_core::UNKNOWN_STABLE,
        }
    }

    /// Take one reference on `offset`'s block. Returns `false` (loudly) when
    /// the reference was NOT taken: the count already hit its terminal zero
    /// (racing free) or the offset is untracked — callers must treat the
    /// block as gone and re-resolve, never proceed unpinned.
    #[must_use]
    pub fn increment_refcount(&self, offset: u64) -> bool {
        match self.refcounts.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(occ) => {
                // Acquire-from-nonzero (loom-modeled, crate::refcount_core):
                // a plain fetch_add could resurrect a count a concurrent
                // free_block just took to zero, leaving this clone holding a
                // freed (reallocatable) offset.
                let taken = crate::refcount_core::try_acquire(occ.get());
                if !taken {
                    log::warn!(
                        "increment_refcount raced a free for offset {offset}: reference not taken"
                    );
                }
                taken
            }
            scc::hash_map::Entry::Vacant(_) => {
                // Unknown offset: it was never allocated by this process or
                // its count already hit zero and was removed. Fabricating a
                // fresh count for a possibly free-listed offset would alias
                // a future allocation — refuse loudly instead.
                log::warn!("increment_refcount on untracked offset {offset}: reference not taken");
                false
            }
        }
    }

    /// Read-only refcount accessor (design-random-small-writes §5.1
    /// predicate 4 / review Issue 10 — RW2 adds it; only
    /// [`Self::increment_refcount`] existed before). `None` = untracked
    /// offset (never allocated by this process / already freed) — callers
    /// must treat it as NOT provably sole-owned.
    pub fn refcount(&self, offset: u64) -> Option<u32> {
        self.refcounts
            .read_sync(&offset, |_, v| crate::refcount_core::peek(v))
    }

    /// W1 patch fence, steps 1a+1b of the §5.1 mechanism (the normative
    /// clone/patch fence — docs/design-random-small-writes.md): retire the
    /// offset's incarnation word (racing validated read-tier fills of this
    /// key now fail their seqlock re-check instead of publishing mid-patch
    /// bytes), interpose the cross-word `SeqCst` fence (the store-buffering
    /// closure against `clone_file`'s pin-CAS → fence → snapshot side),
    /// then re-read the refcount. `true` ⇔ this writer is provably the
    /// sole owner and the in-place DMA may proceed.
    ///
    /// On `false` — the count grew (a clone pinned the block) or the
    /// offset is untracked — the caller MUST [`Self::publish_block`] to
    /// re-stabilize (content never changed) and fall back to the CoW path.
    ///
    /// Unstable-for-existing-offsets audit: `mark_incarnation_unstable`
    /// was built for *ownership transitions* (allocate / free) of an
    /// offset; the patch reuses it for a *content transition* of a LIVE
    /// mapped offset. That reuse is sound because the word's contract is
    /// "content is changing under this key — fills must not publish", not
    /// "the key is dead": `fill_incarnation` returns `None` while
    /// unstable, `fill_incarnation_still` fails across the retire→publish
    /// generation bump, and `publish_block` (after the DMA, or on the
    /// back-off/error paths) restores stability under a NEW generation so
    /// no fill that snapshotted the old generation can validate.
    pub fn begin_patch_sole_owner(&self, offset: u64) -> bool {
        self.mark_incarnation_unstable(offset);
        crate::patch_clone_core::cross_word_fence();
        self.refcount(offset) == Some(1)
    }

    /// §5.1 clone amendment (a): pin + **validate-after-pin**. Takes one
    /// reference exactly like [`Self::increment_refcount`]; on success it
    /// interposes the cross-word `SeqCst` fence and snapshots the offset's
    /// incarnation word. An UNSTABLE word means a patch may be mid-flight
    /// on the pinned block: the caller must unpin (free_block = one
    /// decrement), refetch the authoritative map, and retry — the exact
    /// shape of the existing refused-pin retry loop, extended from "pin
    /// refused" to "pin unvalidated" (bounded by the same `attempt >= 3`
    /// loud refusal).
    ///
    /// The fence sits before the word *lookup* (not inside a
    /// found-the-word arm) so the absent-word case — an offset whose first
    /// in-process patch races this pin after a remount — is ordered too;
    /// absent words are otherwise stable-by-definition (written before
    /// this process, `UNKNOWN_STABLE`).
    pub fn pin_block_validated(&self, offset: u64) -> PinOutcome {
        if !self.increment_refcount(offset) {
            return PinOutcome::Refused;
        }
        crate::patch_clone_core::cross_word_fence();
        let stable = self
            .incarnations
            .read_sync(&offset, |_, v| {
                crate::incarnation_core::snapshot(v).is_some()
            })
            .unwrap_or(true);
        if stable {
            PinOutcome::Pinned
        } else {
            PinOutcome::PinnedUnstable
        }
    }

    pub fn volume_id(&self) -> &str {
        &self._volume_id
    }

    pub fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    /// Bound this allocator to a device of `bytes` capacity (whole chunks).
    /// Zero leaves it unbounded (offline tools / tests).
    pub fn set_capacity_bytes(&self, bytes: u64) {
        self.capacity_blocks
            .store(bytes / self.chunk_size, Ordering::Relaxed);
    }

    /// The bound capacity in bytes (`0` = unbounded) — the
    /// `volume_states` capacity gauge source.
    pub fn capacity_bytes(&self) -> u64 {
        self.capacity_blocks
            .load(Ordering::Relaxed)
            .saturating_mul(self.chunk_size)
    }

    /// Advance the fresh-block cursor by one, refusing to mint an offset at
    /// or past the device capacity (when bounded). CAS loop: a refused
    /// racer must not bump the cursor.
    fn next_fresh_block(&self) -> Result<u64> {
        let cap = self.capacity_blocks.load(Ordering::Relaxed);
        loop {
            let cur = self.highest_block.load(Ordering::Relaxed);
            if cap != 0 && cur >= cap {
                return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    format!(
                        "data volume '{}' full: {} of {} blocks allocated",
                        self._volume_id, cur, cap
                    ),
                )));
            }
            if self
                .highest_block
                .compare_exchange(cur, cur + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(cur);
            }
        }
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
                self.next_fresh_block()?
            }
        } else {
            self.next_fresh_block()?
        };
        let offset = block_idx * self.chunk_size;
        let _ = self.refcounts.insert_sync(offset, AtomicU32::new(1));
        // New incarnation, not yet durable: cache fills must not publish until
        // the owner calls `publish_block` after its device write.
        self.mark_incarnation_unstable(offset);
        Ok(offset)
    }

    /// Release one reference. On the TERMINAL release (count hit zero, or
    /// the offset was untracked) the offset's incarnation is retired and
    /// `true` is returned — but the offset is **not yet reallocatable**:
    /// the freer owns it until [`Self::finish_free`], which is what makes
    /// destructive post-free device work (the router's hole punch) safe to
    /// run in between — it strictly happens-before any new owner's DMA.
    /// Non-terminal releases return `false` and release nothing else.
    pub fn begin_free(&self, offset: u64) -> bool {
        let should_free = if let Some(terminal) = self
            .refcounts
            .read_sync(&offset, |_, v| crate::refcount_core::release(v))
        {
            if terminal {
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
        }
        should_free
    }

    /// Publish a [`Self::begin_free`]-retired offset for reuse. Only after
    /// this can `allocate_block` hand the offset to a new owner.
    pub fn finish_free(&self, offset: u64) {
        let block_idx = offset / self.chunk_size;
        self.free_blocks.insert(block_idx);
        crate::fuse_client::METRICS
            .del_obj
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Release one reference and, when terminal, immediately publish the
    /// offset for reuse (begin + finish with no destructive work between).
    pub async fn free_block(&self, offset: u64) -> Result<()> {
        if self.begin_free(offset) {
            self.finish_free(offset);
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

    /// Block refcount recovery over a metadata volume — walk the live
    /// inode tree (paged range scans) and feed each live ino's `"layout"`
    /// xattr through the shared per-layout recovery body.
    pub async fn recover_active_blocks_v3(
        &self,
        kv: &crate::meta_backend::kv::backend::KvMetaBackend,
        backend_router: &crate::routing::BackendRouter,
    ) -> Result<()> {
        use crate::meta_backend::kv::record::{decode_inode_key, inode_key, InodeValue};
        let inodes = kv.trees()[0];
        let mut checked = 0u64;
        let mut valid_inodes = 0u64;
        let mut layouts_found = 0u64;
        let mut cursor: Vec<u8> = inode_key(1).to_vec();
        let end = inode_key(u64::MAX - 1);
        loop {
            let page = inodes.range(&cursor, &end, 512).await.map_err(|e| {
                crate::error::SqueezefsError::InvalidOperation(format!(
                    "v3 recovery inode walk failed: {e}"
                ))
            })?;
            let Some((last_key, _)) = page.last() else {
                break;
            };
            cursor = crate::meta_backend::kv::node::key_successor(last_key);
            for (k, v) in &page {
                let Ok(ino) = decode_inode_key(k) else {
                    continue;
                };
                let Ok(val) = InodeValue::decode(v) else {
                    continue;
                };
                checked += 1;
                if val.nlink == 0 {
                    continue;
                }
                valid_inodes += 1;
                if let Ok(Some(bytes)) = kv.getxattr(ino, "layout").await {
                    layouts_found += 1;
                    let layout_opt: Option<crate::routing::LayoutMetadata> =
                        if bytes.starts_with(b"{") {
                            serde_json::from_slice(&bytes).ok()
                        } else {
                            bincode::deserialize(&bytes).ok()
                        };
                    self.recover_from_layout(backend_router, layout_opt).await;
                }
            }
        }
        // log, not stdout: this walk also runs under `squeezefs df`, whose
        // `--json` output must stay machine-parseable.
        log::info!(
            "Recovery scan summary (v3): checked={}, valid_inodes={}, layouts_found={}",
            checked,
            valid_inodes,
            layouts_found
        );
        Ok(())
    }

    /// The shared per-layout refcount recovery body (extracted verbatim
    /// from the v2 scan; both format walks feed it).
    async fn recover_from_layout(
        &self,
        backend_router: &crate::routing::BackendRouter,
        layout_opt: Option<crate::routing::LayoutMetadata>,
    ) {
        {
            {
                {
                    {
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
                                        // Read the indirect block to recover its entries.
                                        // Entries carry backend-true key strings (versioned
                                        // v1 blob): recover ONLY the offsets THIS volume
                                        // owns — the same alias-aware matching the inline
                                        // branch below applies.
                                        let block_size =
                                            backend_router.block_size.load(Ordering::Relaxed)
                                                as usize;
                                        if let Ok(raw_bytes) = backend_router
                                            .read_block(indirect_key, block_size)
                                            .await
                                        {
                                            match crate::routing::decode_indirect_block_map(
                                                &raw_bytes,
                                            ) {
                                                Ok(entries) => {
                                                    for (_b, key) in entries {
                                                        let key =
                                                            crate::routing::clean_block_key(&key);
                                                        let Ok((be_id, offset)) =
                                                            backend_router.parse_block_key(&key)
                                                        else {
                                                            continue;
                                                        };
                                                        let owned = be_id
                                                            == self._volume_id.as_ref()
                                                            || ((be_id == "backend_0"
                                                                || be_id == "squeezefs")
                                                                && (self._volume_id.as_ref()
                                                                    == "squeezefs"
                                                                    || self._volume_id.as_ref()
                                                                        == backend_router
                                                                            .default_allocator
                                                                            .volume_id()));
                                                        if owned {
                                                            let block_idx =
                                                                offset / self.chunk_size;
                                                            let _ =
                                                                self.recover_block(block_idx).await;
                                                        }
                                                    }
                                                }
                                                Err(e) => {
                                                    log::warn!(
                                                        "refcount recovery: undecodable indirect \
                                                         block map at '{}': {}",
                                                        indirect_key,
                                                        e
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                                // 2. Check for inline block map. Stored
                                // values are backend-true key strings with an
                                // optional `:extra` trailer after the offset —
                                // strip the trailer with `clean_block_key`
                                // (NEVER a bare `split(':')`, which mangles
                                // prefixed keys: `oss2://123` → `oss2`) and
                                // recover ONLY the offsets THIS volume owns —
                                // the same alias-aware matching as the
                                // indirect branch above.
                                if let Some(ref bm) = layout.block_map {
                                    for offset_str in bm.values() {
                                        let block_key = crate::routing::clean_block_key(offset_str);
                                        let Ok((be_id, offset)) =
                                            backend_router.parse_block_key(&block_key)
                                        else {
                                            continue;
                                        };
                                        let owned = be_id == self._volume_id.as_ref()
                                            || ((be_id == "backend_0" || be_id == "squeezefs")
                                                && (self._volume_id.as_ref() == "squeezefs"
                                                    || self._volume_id.as_ref()
                                                        == backend_router
                                                            .default_allocator
                                                            .volume_id()));
                                        if owned {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Capacity contract (the multi-volume bench EIO investigation): a
    /// bounded allocator must refuse to mint offsets past its device end —
    /// on real block devices the write would EIO/ENOSPC at DMA time, and on
    /// FILE-backed volumes it silently grew the file past its provisioned
    /// size. Freed blocks make the offset pool reusable again; capacity 0
    /// stays unbounded for offline tools.
    #[tokio::test]
    async fn allocate_block_respects_device_capacity() {
        let dlm = crate::dlm::DlmClient::new("local").unwrap();
        let a = BlockAllocator::new(dlm.meta_client().clone(), "cap_test")
            .await
            .unwrap();
        a.set_capacity_bytes(3 * a.chunk_size());

        let o0 = a.allocate_block().await.expect("block 0");
        let o1 = a.allocate_block().await.expect("block 1");
        let o2 = a.allocate_block().await.expect("block 2");
        assert_eq!((o0, o1, o2), (0, a.chunk_size(), 2 * a.chunk_size()));

        let refused = a.allocate_block().await;
        match refused {
            Err(crate::error::SqueezefsError::Io(ref e))
                if e.kind() == std::io::ErrorKind::StorageFull => {}
            other => panic!("allocation past device capacity must fail StorageFull, got {other:?}"),
        }

        // A freed block re-opens exactly one slot.
        a.free_block(o1).await.unwrap();
        let again = a.allocate_block().await.expect("reuse freed block");
        assert_eq!(again, o1);
        assert!(
            a.allocate_block().await.is_err(),
            "pool exhausted again after the freed slot was reused"
        );
    }
}
