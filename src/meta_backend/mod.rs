pub mod alloc;
pub(crate) mod alloc_core;
pub mod atomicity;
pub mod dentry;
pub mod dlm;
pub mod inode;
pub mod kv;
pub mod storage;
pub mod sync_coalescer;
pub mod xattr;

use crate::error::Result;

pub type Ino = u64;

/// Parse the unix-seconds heartbeat timestamp from a `client:{id}` registration
/// value (`{"ts":<secs>,"pid":<pid>}`). Returns `None` for legacy/unparseable
/// values, which callers treat as stale.
fn parse_client_registration_ts(val: &[u8]) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_slice(val).ok()?;
    v.get("ts")?.as_u64()
}

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
    /// Group-commit `fdatasync` coalescer for this volume's device.
    pub sync_coalescer: sync_coalescer::SyncCoalescer,
    /// Deferred-durability flush interval (§4.2). `0` = strict
    /// sync-on-commit: every commit ends with a post-apply coalesced
    /// device barrier. Resolved once at construction from
    /// `SQUEEZEFS_META_FLUSH_INTERVAL_MS` (canonical) with
    /// `SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS` as the legacy alias — the new
    /// name wins if both are set. Default 50 ms.
    flush_interval_ms: u64,
    /// Set by every successful commit apply; the per-volume flusher task
    /// swaps it and issues one `fdatasync` per interval tick.
    needs_flush: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Lazily spawns the flusher on the first deferred commit (a runtime is
    /// guaranteed there; construction sites need not be async).
    flusher_started: tokio::sync::OnceCell<()>,
    /// Mount-time atomicity classification (§4.6): set once by the mount
    /// probe, surfaced on the stats inode as `meta_volume_atomicity`.
    /// Unset (`"unprobed"` on the surface) for harness-built backends.
    pub atomicity_class: std::sync::OnceLock<atomicity::AtomicityClass>,
}

/// Resolve the deferred-flush interval knob: canonical name first, legacy
/// alias second, default 50 ms (§4.2 — a `JOURNAL_`-named knob controlling a
/// flusher with no journal is a permanent naming wart; the alias keeps old
/// operator scripts working).
pub(crate) fn resolve_flush_interval_ms() -> u64 {
    let parse = |name: &str| {
        std::env::var(name)
            .ok()
            .and_then(|val| val.parse::<u64>().ok())
    };
    parse("SQUEEZEFS_META_FLUSH_INTERVAL_MS")
        .or_else(|| parse("SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS"))
        .unwrap_or(50)
}

impl MetaLvBackend {
    pub fn new(storage: storage::MetaLvStorage) -> Self {
        Self {
            storage,
            dlm: dlm::DlmLockManager::new(),
            sync_coalescer: sync_coalescer::SyncCoalescer::new(),
            flush_interval_ms: resolve_flush_interval_ms(),
            needs_flush: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            flusher_started: tokio::sync::OnceCell::new(),
            atomicity_class: std::sync::OnceLock::new(),
        }
    }

    /// Spawn (once) the per-volume deferred flusher: every tick, if a commit
    /// flagged `needs_flush`, issue one `fdatasync` for the whole interval's
    /// commits. Lifecycle is tied to the backend via the `Arc` strong-count
    /// sentinel — when the backend drops, the task observes `<= 1` clones
    /// and exits (dismount cannot leak it; unit-tested below via `Weak`).
    async fn ensure_flusher(&self) {
        debug_assert!(self.flush_interval_ms > 0);
        self.flusher_started
            .get_or_init(|| {
                let needs_flush = self.needs_flush.clone();
                let device_path = self.storage.device_path().to_path_buf();
                let interval_ms = self.flush_interval_ms;
                async move {
                    tokio::spawn(async move {
                        let mut interval =
                            tokio::time::interval(std::time::Duration::from_millis(interval_ms));
                        loop {
                            interval.tick().await;
                            if std::sync::Arc::strong_count(&needs_flush) <= 1 {
                                break;
                            }
                            if needs_flush.swap(false, std::sync::atomic::Ordering::SeqCst) {
                                crate::fuse_client::METRICS
                                    .meta_flush_deferred
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                let _ = crate::uring_fs::fdatasync(&device_path).await;
                            }
                        }
                    });
                }
            })
            .await;
    }

    /// Dentry removal + nlink decrement with the full lock set already held
    /// (`unlink` two-phase). `parent_shared` = the parent inode lock is held
    /// shared (regular-file unlink): parent times go through the 16-byte
    /// field patch — a full-slot parent RMW under a shared lock would clobber
    /// concurrent patchers (design §3.8).
    async fn unlink_locked(
        &self,
        parent: Ino,
        name: &str,
        ino: Ino,
        parent_shared: bool,
    ) -> Result<Ino> {
        self.run_transaction(|| async {
            dentry::remove_dentry(&self.storage, parent, name).await?;

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            if parent_shared {
                inode::stage_parent_time_patch(&self.storage, parent, now).await?;
            } else if let Ok(mut parent_inode) = inode::read_inode(&self.storage, parent).await {
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
    }

    /// Durability barrier for this volume's device, coalesced with concurrent
    /// callers (group commit): N in-flight fsyncs share one `fdatasync`.
    pub async fn sync_device(&self) -> Result<()> {
        let path = self.storage.device_path().to_path_buf();
        self.sync_coalescer
            .barrier(|| {
                let path = path.clone();
                async move {
                    crate::fuse_client::METRICS
                        .meta_device_syncs
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    crate::uring_fs::fdatasync(path).await
                }
            })
            .await
    }

    pub async fn run_transaction<F, Fut, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<R>>,
    {
        use crate::meta_backend::storage::ACTIVE_TX;
        // Nested tx: reuse the parent's staging + lock/rollback state.
        if ACTIVE_TX.try_with(|_| ()).is_ok() {
            return f().await;
        }
        self.run_transaction_sector_locked(f).await
    }

    /// Restore this transaction's staged in-RAM dentry-index deltas and free any
    /// inodes it allocated (design R4 / §3.5). Called on any commit-stage failure
    /// while the bucket guards in `state` are still held.
    fn rollback_tx(
        &self,
        state: &std::sync::Arc<std::sync::Mutex<crate::meta_backend::storage::TxState>>,
    ) {
        let st = state.lock().unwrap();
        self.storage.restore_tx_undo(&st);
    }

    /// Sector-sharded commit (design §3.3): run the closure with no global lock,
    /// then per distinct 4 KiB sector in ascending offset order acquire the
    /// sector write lock, RMW-read the current sector and overlay this tx's
    /// sub-sector patches into a full-sector image, append all images as one WAL
    /// record (byte-identical format), and apply in place — all with the sector
    /// (and dentry-bucket) locks held so per-sector WAL order == apply order and
    /// no sibling slot is clobbered (invariant #1). Transactions touching disjoint
    /// sectors commit fully concurrently.
    async fn run_transaction_sector_locked<F, Fut, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<R>>,
    {
        use crate::meta_backend::storage::{
            MetaLvStorage, TxState, ACTIVE_TX, SECTOR_SIZE, TX_STATE,
        };
        use std::collections::BTreeMap;

        // §Observability (PR 7): live in-flight gauge + peak watermark. The
        // Drop guard decrements on every exit path (closure error, commit
        // error, success) so the gauge can never leak upward.
        struct TxConcurrencyGauge;
        impl Drop for TxConcurrencyGauge {
            fn drop(&mut self) {
                crate::fuse_client::METRICS
                    .meta_tx_concurrency
                    .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let inflight = crate::fuse_client::METRICS
            .meta_tx_concurrency
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        crate::fuse_client::METRICS
            .meta_tx_concurrency_peak
            .fetch_max(inflight, std::sync::atomic::Ordering::Relaxed);
        let _tx_gauge = TxConcurrencyGauge;

        let tx: std::sync::Arc<std::sync::Mutex<Vec<(std::path::PathBuf, u64, Vec<u8>)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let state: std::sync::Arc<std::sync::Mutex<TxState>> =
            std::sync::Arc::new(std::sync::Mutex::new(TxState::default()));

        // Run the closure: stages sub-sector patches into ACTIVE_TX and records
        // bucket guards / undo / alloc'd inodes into TX_STATE. NO global locks.
        let res = TX_STATE
            .scope(state.clone(), ACTIVE_TX.scope(tx.clone(), f()))
            .await;

        let ret = match res {
            Ok(r) => r,
            Err(e) => {
                self.rollback_tx(&state);
                return Err(e);
            }
        };

        let ops = std::mem::take(&mut *tx.lock().unwrap());
        if ops.is_empty() {
            // Nothing staged (e.g. a pure metadata read tx). Guards + undo drop
            // with `state` at return; nothing to commit.
            return Ok(ret);
        }

        // Split meta-device patches from ad-hoc foreign-file patches.
        let mut meta_patches: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut foreign: std::collections::HashMap<std::path::PathBuf, Vec<(u64, Vec<u8>)>> =
            std::collections::HashMap::new();
        for (path, offset, buf) in ops {
            if path == self.storage.path {
                meta_patches.push((offset, buf));
            } else {
                foreign.entry(path).or_default().push((offset, buf));
            }
        }

        // Group by sector, ascending (BTreeMap) => total lock-acquisition order.
        // R1: a patch must never write past its sector (the lock it will hold),
        // so multi-sector patches (e.g. a staged 32 KiB xattr block, PR 6) are
        // split into per-sector fragments here. All fragments of a patch commit
        // in this same transaction under *all* their sector locks (guards held
        // across WAL + apply), so the original patch still applies atomically
        // with respect to the sector scheme.
        let mut by_sector: BTreeMap<u64, Vec<(u64, Vec<u8>)>> = BTreeMap::new();
        for (off, buf) in meta_patches {
            let mut off = off;
            let mut buf = buf;
            loop {
                let sector = MetaLvStorage::sector_of(off);
                let room = (sector + SECTOR_SIZE as u64 - off) as usize;
                if buf.len() <= room {
                    by_sector.entry(sector).or_default().push((off, buf));
                    break;
                }
                let rest = buf.split_off(room);
                by_sector.entry(sector).or_default().push((off, buf));
                off += room as u64;
                buf = rest;
            }
        }

        // Distinct sector-lock SHARDS this tx needs, in ascending shard-index
        // order (dedup). `StripeLocks` is a fixed array, so two distinct sector
        // offsets can share a shard; acquiring by ascending sector *offset* would
        // both re-lock a shared shard (self-deadlock) and risk cross-tx order
        // inversion. Ascending shard index over distinct instances is the real
        // total order (design §3.3 refined for the fixed stripe array).
        let mut shard_indices: Vec<usize> = by_sector
            .keys()
            .map(|&s| self.storage.sector_locks.shard_index(s))
            .collect();
        shard_indices.sort_unstable();
        shard_indices.dedup();

        // Commit: hold the sector write locks across [RMW-read → WAL → apply].
        let commit: Result<()> = async {
            // §Observability (PR 7): time the whole sector-guard acquisition and
            // flag the commit as contended if any needed shard was busy (the
            // try-lock miss is the same-sector contention signal).
            let lock_wait_start = std::time::Instant::now();
            let mut contended = false;
            let mut guards = Vec::with_capacity(shard_indices.len());
            for &idx in &shard_indices {
                let lock = self.storage.sector_locks.get_by_index(idx);
                match lock.try_write() {
                    Ok(g) => guards.push(g),
                    Err(_) => {
                        contended = true;
                        guards.push(lock.write().await);
                    }
                }
            }
            crate::fuse_client::METRICS
                .meta_sector_lock_wait_ns
                .record(lock_wait_start.elapsed());
            if contended {
                crate::fuse_client::METRICS
                    .meta_sector_lock_contended
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }

            // RMW reads for all touched sectors in flight together (the
            // pipelined uring-fs workers service them concurrently).
            let mut images: Vec<(u64, Vec<u8>)> =
                futures::future::try_join_all(by_sector.keys().map(|&sector| async move {
                    let mut image = vec![0u8; SECTOR_SIZE];
                    self.storage.read_blocks_direct(sector, &mut image).await?;
                    Ok::<_, crate::error::SqueezefsError>((sector, image))
                }))
                .await?;
            for (sector, image) in images.iter_mut() {
                if let Some(patches) = by_sector.get(sector) {
                    for (off, bytes) in patches {
                        let start = (off - *sector) as usize;
                        image[start..start + bytes.len()].copy_from_slice(bytes);
                    }
                }
            }

            // Apply every sector image as one batched uring-fs message. No
            // WAL round-trip: the write-only journal carried no acked
            // durability (§2.3) and its unsound reader was deleted in PR 8 —
            // the commit critical section is exactly [RMW read → apply], the
            // window invariant #1 requires (design-wal-crash-consistency
            // §4.2, Key Decision 1).
            crate::fuse_client::METRICS
                .meta_commit_sectors
                .record(images.len());
            self.storage
                .write_blocks_direct_batch(
                    images
                        .into_iter()
                        .map(|(sector, image)| (sector, bytes::Bytes::from(image)))
                        .collect(),
                )
                .await?;

            // Foreign (non-metadata) files: direct uring writes (unjournaled, as
            // previously). In practice a meta tx never stages these.
            for (path, path_ops) in &foreign {
                for (offset, buf) in path_ops {
                    crate::uring_fs::write_at(path, *offset, bytes::Bytes::copy_from_slice(buf))
                        .await?;
                }
            }
            // `guards` (sector write locks) drop here, after apply.
            Ok(())
        }
        .await;

        match commit {
            Ok(()) => {
                // Durability step, AFTER the sector guards dropped (they
                // protect RAM consistency, not durability — §4.2): hot
                // sectors stay unblocked while the device barriers.
                if self.flush_interval_ms == 0 {
                    // Strict sync-on-commit: the barrier covers the APPLY
                    // bytes (the §2.5 fix — it used to cover only the WAL
                    // record, pre-apply). Coalesced: concurrent strict
                    // commits share one fdatasync. A failed barrier fails
                    // the op WITHOUT rollback — the apply landed, so the
                    // in-RAM dentry index matches disk and reverting it
                    // would diverge them; the caller just cannot be given
                    // the durability promise.
                    if let Err(e) = self.sync_device().await {
                        log::error!(
                            "strict-mode post-apply barrier failed (state applied, durability unacked): {e:?}"
                        );
                        return Err(e);
                    }
                } else {
                    self.needs_flush
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    self.ensure_flusher().await;
                }
                Ok(ret)
            }
            Err(e) => {
                self.rollback_tx(&state);
                Err(e)
            }
        }
    }

    pub async fn get_allocated_inode_count(&self) -> usize {
        // The on-disk bitmap is a lazily-reconciled cache (kept only for
        // mountability of pre-PR-8 binaries), not authoritative — popcount the
        // in-RAM allocator (design §3.5 / review Issue 9).
        self.storage.inode_alloc.allocated_count() as usize
    }

    /// No-side-effect format gate (also run standalone by the CLI across ALL
    /// volumes before ANY volume is wiped, so a refused multi-volume format
    /// leaves everything intact — the only mutation is reaping provably stale
    /// client registrations).
    ///
    /// Policy:
    /// - blank / foreign volume (no valid superblock): formatting allowed;
    /// - already-formatted volume: refused without `force` — even idle — so a
    ///   fat-fingered format cannot silently destroy a filesystem;
    /// - **live** client registrations (fresh heartbeat) refuse format even
    ///   WITH `force`: reformatting under an active mount is never safe;
    /// - stale registrations (heartbeat older than
    ///   [`crate::fuse_client::CLIENT_STALE_TTL_SECS`], i.e. crashed clients)
    ///   never block and are reaped.
    pub async fn format_preflight(storage: &storage::MetaLvStorage, force: bool) -> Result<()> {
        if storage.read_superblock().await.is_err() {
            // Never formatted (or unrecognizable): nothing to protect.
            return Ok(());
        }

        if let Ok(attrs) = crate::meta_backend::xattr::list_xattrs(storage, 1).await {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let ttl = crate::fuse_client::CLIENT_STALE_TTL_SECS;
            let mut live = Vec::new();
            let mut stale = Vec::new();
            for k in attrs.iter().filter(|k| k.starts_with("client:")) {
                let fresh = match crate::meta_backend::xattr::get_xattr(storage, 1, k).await {
                    Ok(Some(val)) => parse_client_registration_ts(&val)
                        .map(|ts| now.saturating_sub(ts) <= ttl)
                        .unwrap_or(false),
                    _ => false,
                };
                if fresh {
                    live.push(k.clone());
                } else {
                    stale.push(k.clone());
                }
            }
            // Best-effort reap of crashed-client registrations so a crashed
            // mount never wedges the volume permanently.
            for k in &stale {
                let _ = crate::meta_backend::xattr::remove_xattr(storage, 1, k).await;
            }
            if !live.is_empty() {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "Cannot format: metadata volume is actively mounted by clients: {:?}",
                    live
                )));
            }
        }

        if !force {
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "Metadata volume is already formatted as SqueezeFS; refusing to destroy it. \
                 Pass --force to reformat."
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// The v2 formatter, retained as **test surface only** from PR K6a on
    /// (design-cow-kv-metadata §6.2, resolved OQ 4): the CLI `format`
    /// produces v3 (`kv::builder::format_v3`), but the dual-format safety
    /// net requires *creating* v2 volumes for as long as v2 **mount**
    /// support exists — the K6a/K6b trait-conformance suites run against
    /// both backends, the kill-9 soak parameterizes over both formats
    /// (Rollout 4), and `kv_migrate_tests` (K9) needs populated-v2
    /// sources.
    ///
    /// AGENTS.md no-dead-code exception (documented, per the
    /// "exceptions only" clause): live, test-called code — not parked —
    /// required by the dual-format contract suite and migrate tests.
    /// **Deletion trigger** (resolved OQ 4): fleet telemetry showing
    /// `meta_format_version == "2"` at zero — delete together with v2
    /// mount support (Rollout 5d).
    #[doc(hidden)]
    pub async fn format_v2_for_tests(
        storage: &storage::MetaLvStorage,
        quick: bool,
        force: bool,
        pb: Option<indicatif::ProgressBar>,
    ) -> Result<()> {
        // Fail loud on a volume too small to hold any inode: the table
        // begins past the xattr region (72 MiB), so a smaller device yields
        // an allocator with limit 0 and every create fails "Inode table
        // full" at runtime — far from the actual mistake.
        if storage.inode_alloc.limit() <= alloc::FIRST_ALLOCATABLE_INO {
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "Metadata volume too small: 0 allocatable inodes (the inode \
                 table starts past the 72 MiB xattr region; use a volume of \
                 at least ~80 MiB)"
                    .to_string(),
            ));
        }
        Self::format_preflight(storage, force).await?;

        // Zero-wipe the entire metadata volume first to prevent stale garbage issues
        storage.wipe(quick, pb).await?;

        // Initialize Superblock. The real checksum (xxh3_64, field zeroed) is
        // computed here explicitly — and `write_superblock` stamps it as the
        // choke point anyway — so a freshly formatted volume always carries a
        // verifiable superblock (PR 1, resolved Open Question 4).
        let mut sb = storage::Superblock {
            magic: *storage::MAGIC_VALUE,
            version: 2,
            inode_count: 1000000,
            free_inode_bitmap_root: 4096,
            dentry_root: dentry::DENTRY_TABLE_START,
            journal_start: storage::JOURNAL_REGION_START,
            journal_size: storage::JOURNAL_REGION_SIZE,
            checksum: 0,
        };
        sb.checksum = sb.compute_checksum();
        storage.write_superblock(&sb).await?;

        // Zero-initialize the free-inode bitmap sector (sector 1, starting at 4096)
        let mut bitmap_sector = [0u8; 4096];
        // Mark index 0 and 1 as allocated
        bitmap_sector[0] = 0b0000_0011;
        // Quarantine marks (§4.4, PR 2): set bits 1024–1151 at format time so
        // a volume formatted by this binary is protected even if handed
        // straight to a pre-PR-2b legacy-allocator binary without ever being
        // mounted by a new one (round-2 review Issue 1).
        for ino in xattr::QUARANTINE_INO_START..xattr::QUARANTINE_INO_END {
            bitmap_sector[(ino / 8) as usize] |= 1 << (ino % 8);
        }
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
        // Resolve the dentry under the D-guard, but DROP it before getattr:
        // getattr takes an I-lock, and holding a D-stripe while waiting on an
        // I-stripe inverts the canonical class order (ABBA against unlink's
        // held child-I under stripe collisions). Snapshot semantics are
        // unchanged — lookup→getattr was never atomic (the child can be
        // renamed between the two under exact keys as well).
        let child_ino = {
            let _guard = self.dlm.lock_dentry_shared(parent, name).await;
            match dentry::find_dentry(&self.storage, parent, name).await? {
                Some(dentry) => dentry.child_ino,
                None => {
                    return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("Dentry {} not found in parent {}", name, parent),
                    )))
                }
            }
        };
        self.getattr(child_ino).await
    }

    async fn create(
        &self,
        parent: Ino,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Inode> {
        let _parent_guard = self.dlm.lock_inode_exclusive(parent).await;
        let _dentry_guard = self.dlm.lock_dentry_exclusive(parent, name).await;

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
            // Atomic in-RAM allocation (no lock, no bitmap write); recorded for
            // free-on-failure (design §3.5).
            let new_ino = {
                let ino = self.storage.inode_alloc.alloc()?;
                self.storage.tx_record_alloc(ino);
                let disk_inode = inode::DiskInode::new(ino, final_mode, uid, final_gid);
                inode::write_inode_raw(&self.storage, ino, &disk_inode).await?;
                ino
            };

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
        // Two-phase child discovery (see dlm.rs canonical-order rules): the
        // child ino is only known after reading the dentry, and taking its
        // I-lock while holding the D-lock inverts the class order (ABBA on
        // stripe collisions). Phase 1 reads the child under {I parent, D};
        // phase 2 re-locks the full set canonically and revalidates.
        //
        // Parent lock mode mirrors create (design §3.8 / PR 5): a regular
        // unlink changes only the parent's mtime/ctime (16-byte field
        // patch) — SHARED parent, so same-directory delete storms overlap.
        // Directory removal mutates parent nlink (full-slot RMW) — EXCLUSIVE.
        loop {
            let phase1 = self
                .dlm
                .lock_many(
                    &[(parent, dlm::LockMode::Shared)],
                    &[(parent, name, dlm::LockMode::Exclusive)],
                )
                .await;
            let Some(dentry) = dentry::find_dentry(&self.storage, parent, name).await? else {
                return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Dentry not found",
                )));
            };
            let ino = dentry.child_ino;
            // Self-references and directories take the exclusive-parent
            // path; a regular file can never be its own parent.
            let parent_mode = if dentry.file_type == libc::S_IFDIR || parent == ino {
                dlm::LockMode::Exclusive
            } else {
                dlm::LockMode::Shared
            };
            // Re-lock with the child included (and the final parent mode),
            // then re-validate that the dentry still names this child (it
            // may have been renamed or replaced while no locks were held).
            drop(phase1);
            let inode_set: &[(u64, dlm::LockMode)] = if parent == ino {
                &[(parent, dlm::LockMode::Exclusive)]
            } else {
                &[(parent, parent_mode), (ino, dlm::LockMode::Exclusive)]
            };
            let _full = self
                .dlm
                .lock_many(inode_set, &[(parent, name, dlm::LockMode::Exclusive)])
                .await;
            match dentry::find_dentry(&self.storage, parent, name).await? {
                Some(cur) if cur.child_ino == ino => {
                    return self
                        .unlink_locked(parent, name, ino, parent_mode == dlm::LockMode::Shared)
                        .await;
                }
                _ => continue, // dentry changed under us — rediscover
            }
        }
    }

    async fn link(&self, ino: Ino, new_parent: Ino, new_name: &str) -> Result<Inode> {
        // Both inodes are parameters: take the whole set upfront in
        // canonical order (the old sequence acquired I{child} *after* the
        // D-guard — an ABBA inversion under stripe collisions).
        let _guards = self
            .dlm
            .lock_many(
                &[
                    (new_parent, dlm::LockMode::Exclusive),
                    (ino, dlm::LockMode::Exclusive),
                ],
                &[(new_parent, new_name, dlm::LockMode::Exclusive)],
            )
            .await;

        if let Some(_) = dentry::find_dentry(&self.storage, new_parent, new_name).await? {
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "File already exists".to_string(),
            ));
        }

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

        // Canonical multi-lock: both parents + both dentry names, deduped by
        // stripe (string dedup missed distinct names on a shared stripe —
        // a self-deadlock under collision).
        let _guards = self
            .dlm
            .lock_many(
                &[
                    (old_parent, dlm::LockMode::Exclusive),
                    (new_parent, dlm::LockMode::Exclusive),
                ],
                &[
                    (old_parent, old_name, dlm::LockMode::Exclusive),
                    (new_parent, new_name, dlm::LockMode::Exclusive),
                ],
            )
            .await;

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
                // Acquire both dentry buckets in ascending order first, so a
                // concurrent multi-bucket rename cannot invert the order (§3.6).
                dentry::tx_prelock_buckets(
                    &self.storage,
                    &[(old_parent, old_name), (new_parent, new_name)],
                )
                .await?;
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
                    dentry::tx_prelock_buckets(
                        &self.storage,
                        &[(old_parent, old_name), (new_parent, new_name)],
                    )
                    .await?;
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
        let _guard = self.dlm.lock_inode_shared(dir).await;
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
        let _guard = self.dlm.lock_inode_shared(ino).await;
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
        let _guard = self.dlm.lock_inode_exclusive(ino).await;
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
        let _guard = self.dlm.lock_inode_shared(ino).await;
        xattr::get_xattr(&self.storage, ino, name).await
    }

    async fn setxattr(&self, ino: Ino, name: &str, value: &[u8]) -> Result<()> {
        let _guard = self.dlm.lock_inode_exclusive(ino).await;
        self.run_transaction(|| async { xattr::set_xattr(&self.storage, ino, name, value).await })
            .await
    }

    async fn removexattr(&self, ino: Ino, name: &str) -> Result<()> {
        let _guard = self.dlm.lock_inode_exclusive(ino).await;
        self.run_transaction(|| async { xattr::remove_xattr(&self.storage, ino, name).await })
            .await
    }

    async fn listxattr(&self, ino: Ino) -> Result<Vec<String>> {
        let _guard = self.dlm.lock_inode_shared(ino).await;
        xattr::list_xattrs(&self.storage, ino).await
    }

    async fn destroy_inode(&self, ino: Ino) -> Result<()> {
        // Single destroy == a size-1 batch: one code path (design §4.5).
        self.destroy_inodes(std::slice::from_ref(&ino)).await
    }
}

impl MetaLvBackend {
    /// Reclaim group-commit (design-wal-crash-consistency §4.5, Key
    /// Decision 8): destroy a batch of inos as **one** transaction.
    ///
    /// - DLM exclusive locks on ALL inos via the canonical [`dlm::DlmLockManager::lock_many`]
    ///   order (stripe-deduped ascending — the audited rename pattern, no
    ///   new level in P1-9; a stripe collision degrades to fewer guards,
    ///   never deadlock).
    /// - `nlink`/existence revalidated per ino UNDER the locks (preserving
    ///   the P0-8 TOCTOU check): live or already-zeroed inos are skipped,
    ///   exactly like today's per-ino path.
    /// - ONE `run_transaction` stages every slot zero — patches to the same
    ///   sector merge into one image (up to 16 destroys per sector, one
    ///   apply write).
    /// - `inode_alloc.free()` runs strictly AFTER the durable commit (Key
    ///   Decision 11): there is no DLM exclusion between a destroy and a
    ///   future create reusing the ino, so a premature free could let a
    ///   concurrent create write the slot before the zero applied.
    ///
    /// All-or-nothing per call: on commit failure nothing is freed — the
    /// reclaim consumer bisects the batch down to singletons (§4.5).
    pub async fn destroy_inodes(&self, inos: &[Ino]) -> Result<()> {
        if inos.is_empty() {
            return Ok(());
        }
        let lock_plan: Vec<(u64, dlm::LockMode)> = inos
            .iter()
            .map(|&ino| (ino, dlm::LockMode::Exclusive))
            .collect();
        let _guards = self.dlm.lock_many(&lock_plan, &[]).await;

        // Revalidate under the exclusive locks (P0-8, per ino).
        let mut doomed = Vec::with_capacity(inos.len());
        for &ino in inos {
            match inode::read_inode(&self.storage, ino).await {
                Ok(disk_inode) if disk_inode.nlink > 0 => {
                    log::debug!(
                        "destroy_inodes: ino {} has nlink = {}, skipping destruction",
                        ino,
                        disk_inode.nlink
                    );
                }
                Ok(_) => doomed.push(ino),
                Err(_) => {
                    // Missing or already zeroed (magic 0): nothing to do.
                }
            }
        }
        if doomed.is_empty() {
            return Ok(());
        }
        crate::fuse_client::METRICS
            .meta_reclaim_batch_size
            .record(doomed.len());

        self.run_transaction(|| async {
            for &ino in &doomed {
                let empty = inode::DiskInode::new_zeroed();
                inode::write_inode_raw(&self.storage, ino, &empty).await?;
                // Kill the corpse's xattr block header (magic + num_entries)
                // in the SAME commit: an 8-byte staged patch instead of the
                // per-corpse `removexattr("layout")` transaction reclaim used
                // to issue — which doubled meta-commit traffic under delete
                // storms and collided with foreground unlinks on the shared
                // inode-table sectors. Also guarantees a reused ino can never
                // resurrect the corpse's layout xattr. Quarantined inos are
                // skipped: their block is journal-region territory (§4.4).
                if !xattr::is_quarantined(ino) {
                    let block_offset =
                        xattr::XATTR_BLOCK_START + ino * xattr::XATTR_BLOCK_SIZE as u64;
                    self.storage.write_blocks(block_offset, &[0u8; 8]).await?;
                }
            }
            Ok(())
        })
        .await?;
        for &ino in &doomed {
            self.storage.inode_alloc.free(ino);
        }
        Ok(())
    }
}

/// One metadata volume behind the dual-format dispatch (PR K6a;
/// design-cow-kv-metadata §4.9 "Volume routing", §6.1): static dispatch,
/// no `dyn`, routed by superblock version at open — mixed-version volume
/// sets are legal (migrate one volume at a time, PR K9).
///
/// K6a scope: construction ([`VolumeBackend::open_for_mount`]) and the
/// **read** dispatch (`lookup`/`getattr`/`readdir`/`getxattr`/
/// `listxattr`) — what the mount bootstrap (format-config read) and the
/// dual-backend conformance suite consume. PR K6b extends the dispatch to
/// the full mutating `Metadata` surface and folds it into
/// `RoutedMetaBackend`.
#[derive(Clone)]
pub enum VolumeBackend {
    /// Format version 2: the fixed-geometry `MetaLvBackend` (frozen
    /// behavior; its whole test suite pins it).
    V2(std::sync::Arc<MetaLvBackend>),
    /// Format version 3: the CoW KV node layer (read side since K6a,
    /// read-write since K6b).
    V3(std::sync::Arc<kv::backend::KvMetaBackend>),
}

impl std::fmt::Debug for VolumeBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VolumeBackend::V2(_) => f.write_str("VolumeBackend::V2"),
            VolumeBackend::V3(be) => f.debug_tuple("VolumeBackend::V3").field(be).finish(),
        }
    }
}

impl VolumeBackend {
    /// Open `path` routed by the sector-0 version gate
    /// (`kv::superblock::classify_volume`): v2 superblocks take the
    /// unchanged v2 path (`MetaLvStorage::open` + `validate_superblock`,
    /// byte-identical policy); v3 superblocks mount the KV read side
    /// (SB → ledger → bitmap → replay). Blank volumes fail loud with the
    /// actionable "run `squeezefs format` first"; foreign magic, torn
    /// superblocks, versions above 3, and unknown incompat feature bits
    /// fail loud from the gate itself.
    pub async fn open_for_mount(path: &str) -> Result<Self> {
        match kv::superblock::classify_volume(std::path::Path::new(path)).await? {
            kv::superblock::VolumeFormat::Blank => {
                Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "Metadata volume {path} is not formatted (zeroed superblock) — run \
                     `squeezefs format` first"
                )))
            }
            kv::superblock::VolumeFormat::V2(_) => {
                // The unchanged v2 path, byte-identical policy: open (the
                // v2 size floor included) + full superblock validation.
                let storage = storage::MetaLvStorage::open(path, 128 * 1024 * 1024)?;
                storage.validate_superblock().await?;
                Ok(VolumeBackend::V2(std::sync::Arc::new(MetaLvBackend::new(
                    storage,
                ))))
            }
            kv::superblock::VolumeFormat::V3(_) => Ok(VolumeBackend::V3(
                kv::backend::KvMetaBackend::open(std::path::Path::new(path)).await?,
            )),
        }
    }

    /// The volume's on-disk format version (the `meta_format_version`
    /// stats string's source, design §10).
    pub fn format_version(&self) -> u32 {
        match self {
            VolumeBackend::V2(_) => 2,
            VolumeBackend::V3(_) => 3,
        }
    }

    /// The volume's backing device path (stats surface, sync targets).
    pub fn device_path(&self) -> &std::path::Path {
        match self {
            VolumeBackend::V2(be) => be.storage.device_path(),
            VolumeBackend::V3(be) => be.device_path(),
        }
    }

    /// `meta_volume_atomicity` (design §10 / resolved OQ 2): the probed
    /// class on v2 (that IS the contract there); the constant
    /// `cow-checksummed` contract class on v3 (§4.10, by construction).
    pub fn atomicity_contract(&self) -> &'static str {
        match self {
            VolumeBackend::V2(be) => be
                .atomicity_class
                .get()
                .map(|c| c.as_str())
                .unwrap_or("unprobed"),
            VolumeBackend::V3(be) => be.atomicity_contract(),
        }
    }

    /// `meta_volume_atomicity_physical` (resolved OQ 2's second field):
    /// the hardware probe on both formats.
    pub fn atomicity_physical(&self) -> &'static str {
        match self {
            VolumeBackend::V2(be) => be
                .atomicity_class
                .get()
                .map(|c| c.as_str())
                .unwrap_or("unprobed"),
            VolumeBackend::V3(be) => be.atomicity_physical(),
        }
    }

    /// Read dispatch: `Metadata::lookup` shape.
    pub async fn lookup(&self, parent: Ino, name: &str) -> Result<Inode> {
        match self {
            VolumeBackend::V2(be) => be.lookup(parent, name).await,
            VolumeBackend::V3(be) => be.lookup(parent, name).await,
        }
    }

    /// Read dispatch: `Metadata::getattr` shape.
    pub async fn getattr(&self, ino: Ino) -> Result<Inode> {
        match self {
            VolumeBackend::V2(be) => be.getattr(ino).await,
            VolumeBackend::V3(be) => be.getattr(ino).await,
        }
    }

    /// Read dispatch: `Metadata::readdir` shape (v2 ignores
    /// `offset`/`max` — its documented behavior; v3 honors them, §5.1).
    pub async fn readdir(&self, dir: Ino, offset: u64, max: usize) -> Result<Vec<DirEntry>> {
        match self {
            VolumeBackend::V2(be) => be.readdir(dir, offset, max).await,
            VolumeBackend::V3(be) => be.readdir(dir, offset, max).await,
        }
    }

    /// PR K7 (§5.1): cookie-paged readdir — `Some((cookie, entry), …)`
    /// on v3 (each entry paired with its resume cookie,
    /// `3 + ((hash54 << 8) | coll_seq)`), `None` on v2, whose positional
    /// FUSE offsets predate the cookie contract and stay byte-identical.
    pub async fn readdir_page(
        &self,
        dir: Ino,
        offset: u64,
        max: usize,
    ) -> Result<Option<Vec<(u64, DirEntry)>>> {
        match self {
            VolumeBackend::V2(_) => Ok(None),
            VolumeBackend::V3(be) => Ok(Some(be.readdir_page(dir, offset, max).await?)),
        }
    }

    /// Read dispatch: `Metadata::getxattr` shape.
    pub async fn getxattr(&self, ino: Ino, name: &str) -> Result<Option<Vec<u8>>> {
        match self {
            VolumeBackend::V2(be) => be.getxattr(ino, name).await,
            VolumeBackend::V3(be) => be.getxattr(ino, name).await,
        }
    }

    /// Read dispatch: `Metadata::listxattr` shape.
    pub async fn listxattr(&self, ino: Ino) -> Result<Vec<String>> {
        match self {
            VolumeBackend::V2(be) => be.listxattr(ino).await,
            VolumeBackend::V3(be) => be.listxattr(ino).await,
        }
    }

    // -----------------------------------------------------------------
    // PR K6b: the mutating dispatch — every op the v2 backend serves,
    // routed by format (design §4.9 "Volume routing"; the dual-format
    // conformance suite runs the same assertions against both arms).
    // -----------------------------------------------------------------

    /// The per-volume metadata lock manager (level 4a of P1-9) — the
    /// routed layer's lock phases are format-agnostic.
    pub fn dlm(&self) -> &dlm::DlmLockManager {
        match self {
            VolumeBackend::V2(be) => &be.dlm,
            VolumeBackend::V3(be) => be.dlm(),
        }
    }

    /// Mutating dispatch: `Metadata::create` shape.
    pub async fn create(
        &self,
        parent: Ino,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Inode> {
        match self {
            VolumeBackend::V2(be) => be.create(parent, name, mode, uid, gid).await,
            VolumeBackend::V3(be) => be.create(parent, name, mode, uid, gid).await,
        }
    }

    /// Mutating dispatch: `Metadata::unlink` shape.
    pub async fn unlink(&self, parent: Ino, name: &str) -> Result<Ino> {
        match self {
            VolumeBackend::V2(be) => be.unlink(parent, name).await,
            VolumeBackend::V3(be) => Metadata::unlink(be.as_ref(), parent, name).await,
        }
    }

    /// Mutating dispatch: `Metadata::link` shape.
    pub async fn link(&self, ino: Ino, new_parent: Ino, new_name: &str) -> Result<Inode> {
        match self {
            VolumeBackend::V2(be) => be.link(ino, new_parent, new_name).await,
            VolumeBackend::V3(be) => Metadata::link(be.as_ref(), ino, new_parent, new_name).await,
        }
    }

    /// Mutating dispatch: `Metadata::rename` shape.
    pub async fn rename(
        &self,
        old_parent: Ino,
        old_name: &str,
        new_parent: Ino,
        new_name: &str,
        flags: u32,
    ) -> Result<()> {
        match self {
            VolumeBackend::V2(be) => {
                be.rename(old_parent, old_name, new_parent, new_name, flags)
                    .await
            }
            VolumeBackend::V3(be) => {
                Metadata::rename(
                    be.as_ref(),
                    old_parent,
                    old_name,
                    new_parent,
                    new_name,
                    flags,
                )
                .await
            }
        }
    }

    /// Mutating dispatch: `Metadata::setattr` shape.
    #[allow(clippy::too_many_arguments)] // the trait's signature
    pub async fn setattr(
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
        match self {
            VolumeBackend::V2(be) => {
                be.setattr(ino, mode, uid, gid, size, atime, mtime, ctime)
                    .await
            }
            VolumeBackend::V3(be) => {
                Metadata::setattr(be.as_ref(), ino, mode, uid, gid, size, atime, mtime, ctime).await
            }
        }
    }

    /// Mutating dispatch: `Metadata::setxattr` shape.
    pub async fn setxattr(&self, ino: Ino, name: &str, value: &[u8]) -> Result<()> {
        match self {
            VolumeBackend::V2(be) => be.setxattr(ino, name, value).await,
            VolumeBackend::V3(be) => Metadata::setxattr(be.as_ref(), ino, name, value).await,
        }
    }

    /// Mutating dispatch: `Metadata::removexattr` shape.
    pub async fn removexattr(&self, ino: Ino, name: &str) -> Result<()> {
        match self {
            VolumeBackend::V2(be) => be.removexattr(ino, name).await,
            VolumeBackend::V3(be) => Metadata::removexattr(be.as_ref(), ino, name).await,
        }
    }

    /// Mutating dispatch: `Metadata::destroy_inode` shape.
    pub async fn destroy_inode(&self, ino: Ino) -> Result<()> {
        match self {
            VolumeBackend::V2(be) => be.destroy_inode(ino).await,
            VolumeBackend::V3(be) => Metadata::destroy_inode(be.as_ref(), ino).await,
        }
    }

    /// Batched destroy (the reclaim group-commit surface).
    pub async fn destroy_inodes(&self, inos: &[Ino]) -> Result<()> {
        match self {
            VolumeBackend::V2(be) => be.destroy_inodes(inos).await,
            VolumeBackend::V3(be) => be.destroy_inodes(inos).await,
        }
    }

    /// Coalesced durability barrier for this volume's device.
    pub async fn sync_device(&self) -> Result<()> {
        match self {
            VolumeBackend::V2(be) => be.sync_device().await,
            VolumeBackend::V3(be) => be.sync_device().await,
        }
    }

    /// Layout xattr + size persist (the fsync/release writeback path).
    /// On v3 this is ONE two-record transaction (§5.3).
    pub async fn set_layout_and_size(&self, ino: Ino, layout: &[u8], size: u64) -> Result<()> {
        match self {
            VolumeBackend::V2(be) => {
                let _guard = be.dlm.lock_inode_exclusive(ino).await;
                xattr::set_xattr(&be.storage, ino, "layout", layout).await?;
                let mut disk_inode = inode::read_inode(&be.storage, ino).await?;
                disk_inode.size = size;
                disk_inode.ctime = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                inode::write_inode(&be.storage, ino, &disk_inode).await?;
                Ok(())
            }
            VolumeBackend::V3(be) => be.set_layout_and_size(ino, layout, size).await,
        }
    }

    /// Clean-unmount teardown: v3 volumes run a final checkpoint and
    /// drain the checkpoint task; v2 volumes reconcile the on-disk inode
    /// bitmap (the pre-existing unmount behavior, moved behind the
    /// dispatch).
    pub async fn shutdown_for_unmount(&self) -> Result<()> {
        match self {
            VolumeBackend::V2(be) => be.storage.refresh_bitmap_from_table().await,
            VolumeBackend::V3(be) => Ok(be.shutdown().await?),
        }
    }
}

/// PR K6b (design §4.9 "Volume routing"): the routed backend holds the
/// dual-format dispatch — `volumes` is a `Vec<VolumeBackend>` and every
/// op routes per arm. `Arc`-shared (not `Clone`): the `VolumeBackend`
/// arms own live backends (checkpoint tasks, caches) that must not fork.
pub struct RoutedMetaBackend {
    pub volumes: Vec<VolumeBackend>,
    pub disabled_volumes: std::sync::Arc<dashmap::DashMap<usize, bool, ahash::RandomState>>,
    pub redirections: std::sync::Arc<dashmap::DashMap<usize, usize, ahash::RandomState>>,
}

impl RoutedMetaBackend {
    /// The all-v2 convenience constructor (the pre-K6b signature; the v2
    /// fixtures and tests keep using it).
    pub fn new(volumes: Vec<std::sync::Arc<MetaLvBackend>>) -> Self {
        Self::new_dispatch(volumes.into_iter().map(VolumeBackend::V2).collect())
    }

    /// PR K6b: construct over the dual-format dispatch — the volume set
    /// may mix v2 and v3 arms (design §4.9: mixed-version sets are legal;
    /// migrate one volume at a time).
    pub fn new_dispatch(volumes: Vec<VolumeBackend>) -> Self {
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

    /// The v2 arm of volume `idx` — the v2-only surfaces (recovery scans,
    /// bitmap reconciliation, the v2 test fixtures). Panics on a v3 arm:
    /// callers are v2-specific by construction.
    pub fn v2_volume(&self, idx: usize) -> &std::sync::Arc<MetaLvBackend> {
        match &self.volumes[idx] {
            VolumeBackend::V2(be) => be,
            VolumeBackend::V3(_) => {
                panic!("volume {idx} is format v3; this surface is v2-only")
            }
        }
    }

    /// Format-generic routed dentry read: `(stored child ino (global),
    /// S_IFMT bits)`.
    async fn find_dentry_routed(
        &self,
        idx: usize,
        local_parent: Ino,
        name: &str,
    ) -> Result<Option<(Ino, u32)>> {
        match &self.volumes[idx] {
            VolumeBackend::V2(be) => Ok(dentry::find_dentry(&be.storage, local_parent, name)
                .await?
                .map(|d| (d.child_ino, d.file_type))),
            VolumeBackend::V3(be) => be.routed_find_dentry(local_parent, name).await,
        }
    }

    /// §4.4 pt 4 escalation mirror: after any v3 mutation error, latch
    /// the volume into `disabled_volumes` iff its backend has fail-
    /// stopped (repeated journal write failures) — the existing
    /// mechanism `check_volume_enabled` consults.
    fn mirror_v3_failure(&self, idx: usize) {
        if let VolumeBackend::V3(be) = &self.volumes[idx] {
            if be.is_failed() {
                self.disabled_volumes.insert(idx, true);
            }
        }
    }

    /// Rename fragment: pure dentry removal on one volume (the caller
    /// holds the D-guard).
    async fn remove_dentry_routed(&self, idx: usize, local_parent: Ino, name: &str) -> Result<()> {
        match &self.volumes[idx] {
            VolumeBackend::V2(be) => dentry::remove_dentry(&be.storage, local_parent, name).await,
            VolumeBackend::V3(be) => {
                let out = be
                    .routed_remove_dentry(local_parent, name, kv::backend::RoutedParentUpdate::None)
                    .await;
                if out.is_err() {
                    self.mirror_v3_failure(idx);
                }
                out
            }
        }
    }

    /// Rename fragment: pure dentry insertion on one volume (`ft_bits` =
    /// `mode & S_IFMT`).
    async fn insert_dentry_routed(
        &self,
        idx: usize,
        local_parent: Ino,
        global_child: Ino,
        name: &str,
        ft_bits: u32,
    ) -> Result<()> {
        match &self.volumes[idx] {
            VolumeBackend::V2(be) => {
                dentry::insert_dentry(&be.storage, local_parent, global_child, name, ft_bits).await
            }
            VolumeBackend::V3(be) => {
                let out = be
                    .routed_add_dentry(
                        local_parent,
                        name,
                        global_child,
                        ft_bits,
                        kv::backend::RoutedParentUpdate::None,
                    )
                    .await;
                if out.is_err() {
                    self.mirror_v3_failure(idx);
                }
                out
            }
        }
    }

    /// Rename fragment: directory-move parent nlink shift, best-effort
    /// (the v2 `if let Ok` shape).
    async fn parent_nlink_delta_routed(
        &self,
        idx: usize,
        local_parent: Ino,
        delta: i64,
    ) -> Result<()> {
        match &self.volumes[idx] {
            VolumeBackend::V2(be) => {
                if let Ok(mut p) = inode::read_inode(&be.storage, local_parent).await {
                    if delta > 0 {
                        p.nlink += delta as u32;
                    } else if p.nlink > 2 {
                        p.nlink -= (-delta) as u32;
                    }
                    let _ = inode::write_inode(&be.storage, local_parent, &p).await;
                }
                Ok(())
            }
            VolumeBackend::V3(be) => {
                let out = be.routed_parent_nlink_delta(local_parent, delta).await;
                if out.is_err() {
                    self.mirror_v3_failure(idx);
                }
                out
            }
        }
    }

    /// Rename fragment: destination-inode replacement accounting —
    /// ENOTEMPTY probe for directories, nlink dec + ctime (the v2 shape,
    /// best-effort on a missing inode).
    async fn dest_replace_routed(&self, idx: usize, local_dest: Ino) -> Result<()> {
        match &self.volumes[idx] {
            VolumeBackend::V2(be) => {
                if let Ok(mut dest_inode) = inode::read_inode(&be.storage, local_dest).await {
                    // Check if destination is a directory and is not empty
                    if (dest_inode.mode & libc::S_IFMT) == libc::S_IFDIR {
                        let dentries = dentry::list_dentries(&be.storage, local_dest).await?;
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
                    let _ = inode::write_inode(&be.storage, local_dest, &dest_inode).await;
                }
                Ok(())
            }
            VolumeBackend::V3(be) => {
                let out = be.routed_dest_replace(local_dest).await;
                if out.is_err() {
                    self.mirror_v3_failure(idx);
                }
                out
            }
        }
    }

    /// Format-generic LOCK-FREE inode read (no DLM acquisition — safe
    /// under held routed I-guards, where a re-entrant stripe read can
    /// deadlock against a queued writer).
    async fn read_inode_routed(&self, idx: usize, local_ino: Ino) -> Result<Inode> {
        match &self.volumes[idx] {
            VolumeBackend::V2(be) => {
                let di = inode::read_inode(&be.storage, local_ino).await?;
                Ok(Inode {
                    ino: local_ino,
                    mode: di.mode,
                    uid: di.uid,
                    gid: di.gid,
                    size: di.size,
                    nlink: di.nlink,
                    atime: di.atime,
                    mtime: di.mtime,
                    ctime: di.ctime,
                    flags: di.flags,
                })
            }
            VolumeBackend::V3(be) => be.getattr(local_ino).await,
        }
    }

    /// The per-volume metadata lock manager (tests force stripe collisions
    /// through its public stripe accessors).
    pub fn volume_dlm(&self, idx: usize) -> &dlm::DlmLockManager {
        self.volumes[idx].dlm()
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
        static HEALTH_CACHE: once_cell::sync::Lazy<scc::HashMap<usize, (u32, std::time::Instant)>> =
            once_cell::sync::Lazy::new(scc::HashMap::new);
        if let Some(v) = HEALTH_CACHE.read_sync(&idx, |_, v| *v) {
            if v.1.elapsed() < std::time::Duration::from_millis(500) {
                return v.0;
            }
        }
        // Resolved OQ 5 (design §4.9): normalize by estimated
        // remaining-capacity FRACTION — free inos / limit on v2
        // (unchanged formula), free extents / total on v3 — the one unit
        // both formats honestly report, so mixed sets compare.
        let free_factor = match &self.volumes[idx] {
            VolumeBackend::V2(vol) => {
                let allocated = vol.get_allocated_inode_count().await;
                let max_inodes = 20000;
                if max_inodes > allocated {
                    (max_inodes - allocated) as f64 / max_inodes as f64
                } else {
                    0.0
                }
            }
            VolumeBackend::V3(be) => {
                let total = be.superblock().total_extents();
                if total > 0 {
                    be.free_extents() as f64 / total as f64
                } else {
                    0.0
                }
            }
        };

        let score = (free_factor * 1000.0) as u32;
        let score = score.min(1000);
        let _ = HEALTH_CACHE.upsert_sync(idx, (score, std::time::Instant::now()));
        score
    }

    /// Batched reclaim (design §4.5): group the inos by owning volume and
    /// destroy each volume's group as one `destroy_inodes` transaction.
    /// All-or-nothing per call — the first failing volume aborts and the
    /// caller bisects (volume grouping is deterministic, so halves re-route
    /// consistently and converge to singletons).
    pub async fn destroy_inodes(&self, inos: &[Ino]) -> Result<()> {
        let mut per_volume: std::collections::HashMap<usize, Vec<Ino>> =
            std::collections::HashMap::new();
        for &ino in inos {
            let (v_idx, local_ino) = self.route_ino(ino);
            per_volume.entry(v_idx).or_default().push(local_ino);
        }
        for (v_idx, locals) in per_volume {
            self.check_volume_enabled(v_idx)?;
            self.volumes[v_idx].destroy_inodes(&locals).await?;
        }
        Ok(())
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

    /// PR K7 (§5.1): whether `ino` routes to a format-v3 volume — the
    /// FUSE layer's streaming-vs-snapshot readdir dispatch (v3 streams by
    /// cookie; v2 keeps the whole-dir snapshot path byte-identical).
    pub fn ino_routes_to_v3(&self, ino: Ino) -> bool {
        let (v_idx, _) = self.route_ino(ino);
        matches!(&self.volumes[v_idx], VolumeBackend::V3(_))
    }

    /// PR K8 (design-cow-kv-metadata §5.3): the per-ino xattr value cap — the
    /// largest inline xattr value `ino`'s volume can hold. A *non-trait*
    /// capability accessor (the `Metadata` trait stays unchanged, §5.1). The
    /// v3 layout inline-spill decision (`DataRouter::save_metadata_to_backend`)
    /// consults it, so a v3 volume set with differing `node_size` spills per
    /// volume by serialized size; v2 fixed-geometry volumes keep their legacy
    /// 32-entry inline ceiling (§6.1 byte-identical), for which this reports
    /// the fixed 8 KiB xattr value slot. v2 → 8_192; v3 → the record-value cap
    /// `min(65_536, node_size/4)` (§4.2).
    pub fn xattr_value_cap(&self, ino: Ino) -> usize {
        /// The v2 `xattr::XattrEntry::val` field capacity (`[[u8; 32]; 256]`).
        const V2_XATTR_VALUE_CAP: usize = 8192;
        let (v_idx, _) = self.route_ino(ino);
        match &self.volumes[v_idx] {
            VolumeBackend::V2(_) => V2_XATTR_VALUE_CAP,
            VolumeBackend::V3(be) => be.record_value_cap(),
        }
    }

    /// PR K7 (§5.1): one cookie-paged readdir step against `dir`'s
    /// volume — `Some` pages of at most `max` `(resume_cookie, entry)`
    /// pairs on v3 (dentry child inos are stored global, so no mapping),
    /// `None` when `dir` routes to a v2 volume (the caller falls back to
    /// the positional snapshot path). Takes the same per-volume shared
    /// inode guard as the trait `readdir`.
    pub async fn readdir_stream(
        &self,
        dir: Ino,
        offset: u64,
        max: usize,
    ) -> Result<Option<Vec<(u64, DirEntry)>>> {
        let (v_idx, local_dir) = self.route_ino(dir);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx].dlm().lock_inode_shared(local_dir).await;
        self.volumes[v_idx]
            .readdir_page(local_dir, offset, max)
            .await
    }
}

#[async_trait::async_trait]
impl Metadata for RoutedMetaBackend {
    async fn lookup(&self, parent: Ino, name: &str) -> Result<Inode> {
        let (v_idx, local_parent) = self.route_ino(parent);
        self.check_volume_enabled(v_idx)?;
        // Drop the D-guard before getattr's I-lock (canonical class order —
        // see MetaLvBackend::lookup).
        let child_ino = {
            let _guard = self.volumes[v_idx]
                .dlm()
                .lock_dentry_shared(local_parent, name)
                .await;
            match self.find_dentry_routed(v_idx, local_parent, name).await? {
                Some((child_ino, _ft)) => child_ino,
                None => {
                    return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("Dentry {} not found in parent {}", name, parent),
                    )))
                }
            }
        };
        self.getattr(child_ino).await
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

        // Directories need the EXCLUSIVE parent lock (parent nlink RMW). Regular
        // creates (PR 5) take a SHARED parent lock: they only stage a 16-byte
        // parent mtime/ctime field patch (never nlink/mode), merged at commit
        // under the parent sector lock — so same-dir regular creates run
        // concurrently, while the shared lock still serializes against any
        // exclusive parent mutator (mkdir/setattr/unlink/rename), preventing a
        // field patch from racing a full-slot parent write (design §3.8).
        let _parent_guard = if is_dir {
            self.volumes[parent_v_idx]
                .dlm()
                .lock_inode_exclusive(local_parent)
                .await
        } else {
            self.volumes[parent_v_idx]
                .dlm()
                .lock_inode_shared(local_parent)
                .await
        };
        let _dentry_guard = self.volumes[parent_v_idx]
            .dlm()
            .lock_dentry_exclusive(local_parent, name)
            .await;

        if parent_v_idx == target_v_idx {
            // v3 arm: ONE whole-tx journal entry with the routed
            // semantics (§4.4; the v2 arm below is byte-identical
            // pre-K6b behavior).
            if let VolumeBackend::V3(be) = &self.volumes[target_v_idx] {
                let out = be
                    .routed_create_local(local_parent, name, mode, uid, gid, |local| {
                        self.make_global_ino(local, target_v_idx)
                    })
                    .await;
                if out.is_err() {
                    self.mirror_v3_failure(target_v_idx);
                }
                return out;
            }
            let backend = self.v2_volume(target_v_idx);
            let is_dir_flag = is_dir;

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

                    let parent_inode = inode::read_inode(&backend.storage, local_parent).await?;
                    let mut final_gid = gid;
                    let mut final_mode = mode;
                    if (parent_inode.mode & libc::S_ISGID) != 0 {
                        final_gid = parent_inode.gid;
                        if (mode & libc::S_IFMT) == libc::S_IFDIR {
                            final_mode |= libc::S_ISGID;
                        }
                    }

                    // Unified path for regular files AND directories: atomic
                    // in-RAM alloc (freed on failure), child nlink=2 only for
                    // directories.
                    let new_local_ino = backend.storage.inode_alloc.alloc()?;
                    backend.storage.tx_record_alloc(new_local_ino);
                    let disk_inode = {
                        let mut di =
                            inode::DiskInode::new(new_local_ino, final_mode, uid, final_gid);
                        if is_dir_flag {
                            di.nlink = 2;
                        }
                        inode::write_inode_raw(&backend.storage, new_local_ino, &di).await?;
                        di
                    };

                    let global_child_ino = self.make_global_ino(new_local_ino, target_v_idx);

                    dentry::insert_dentry(
                        &backend.storage,
                        local_parent,
                        global_child_ino,
                        name,
                        final_mode & libc::S_IFMT,
                    )
                    .await?;

                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos() as u64;
                    if is_dir_flag {
                        // Directory (exclusive I{parent}): full-slot RMW to bump
                        // nlink + update times (design §3.8 / Issue 1).
                        let mut parent_inode =
                            inode::read_inode(&backend.storage, local_parent).await?;
                        parent_inode.mtime = now;
                        parent_inode.ctime = now;
                        parent_inode.nlink += 1;
                        inode::write_inode(&backend.storage, local_parent, &parent_inode).await?;
                    } else {
                        // Regular file (shared I{parent}): stage only a
                        // 16-byte mtime/ctime field patch — preserves parent
                        // nlink/mode/size, so concurrent same-dir creates commit
                        // without clobbering each other (design §3.8, PR 5).
                        inode::stage_parent_time_patch(&backend.storage, local_parent, now).await?;
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
                })
                .await?;
            Ok(inode)
        } else {
            // Cross-volume create: never mix unjournaled direct sector writes with
            // concurrent run_transaction applies on the same volume (stale sector
            // snapshots wipe sibling slots → magic 0 / ESTALE). Mutate each volume
            // only inside that volume's run_transaction (holds transaction_lock).
            // Each side dispatches per format (mixed sets are legal, §4.9).
            if self
                .find_dentry_routed(parent_v_idx, local_parent, name)
                .await?
                .is_some()
            {
                return Err(crate::error::SqueezefsError::InvalidOperation(
                    "File already exists".to_string(),
                ));
            }

            let parent_inode = self.read_inode_routed(parent_v_idx, local_parent).await?;
            let mut final_gid = gid;
            let mut final_mode = mode;
            if (parent_inode.mode & libc::S_ISGID) != 0 {
                final_gid = parent_inode.gid;
                if (mode & libc::S_IFMT) == libc::S_IFDIR {
                    final_mode |= libc::S_ISGID;
                }
            }

            let is_dir_flag = is_dir;
            // Target side: mint the child inode record.
            let (new_local_ino, child_inode) = match &self.volumes[target_v_idx] {
                VolumeBackend::V2(target) => {
                    let target = target.clone();
                    target
                        .run_transaction(|| async {
                            let new_local_ino = {
                                let ino = target.storage.inode_alloc.alloc()?;
                                target.storage.tx_record_alloc(ino);
                                ino
                            };
                            let mut disk_inode =
                                inode::DiskInode::new(new_local_ino, final_mode, uid, final_gid);
                            if is_dir_flag {
                                disk_inode.nlink = 2;
                            }
                            inode::write_inode_raw(&target.storage, new_local_ino, &disk_inode)
                                .await?;
                            Ok((
                                new_local_ino,
                                Inode {
                                    ino: new_local_ino,
                                    mode: disk_inode.mode,
                                    uid: disk_inode.uid,
                                    gid: disk_inode.gid,
                                    size: disk_inode.size,
                                    nlink: disk_inode.nlink,
                                    atime: disk_inode.atime,
                                    mtime: disk_inode.mtime,
                                    ctime: disk_inode.ctime,
                                    flags: disk_inode.flags,
                                },
                            ))
                        })
                        .await?
                }
                VolumeBackend::V3(be) => {
                    let local = be.allocate_ino();
                    let out = be
                        .routed_mint_inode(local, final_mode, uid, final_gid)
                        .await;
                    if out.is_err() {
                        self.mirror_v3_failure(target_v_idx);
                    }
                    let v = out?;
                    (
                        local,
                        Inode {
                            ino: local,
                            mode: v.mode,
                            uid: v.uid,
                            gid: v.gid,
                            size: v.size,
                            nlink: v.nlink,
                            atime: v.atime,
                            mtime: v.mtime,
                            ctime: v.ctime,
                            flags: v.flags,
                        },
                    )
                }
            };

            let global_child_ino = self.make_global_ino(new_local_ino, target_v_idx);

            // Parent side: the dentry (global child ino) + parent update.
            match &self.volumes[parent_v_idx] {
                VolumeBackend::V2(parent_be) => {
                    let parent_be = parent_be.clone();
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

                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_nanos() as u64;
                            if is_dir_flag {
                                // Directory (exclusive I{parent}): full-slot RMW (nlink + times).
                                let mut parent_inode =
                                    inode::read_inode(&parent_be.storage, local_parent).await?;
                                parent_inode.mtime = now;
                                parent_inode.ctime = now;
                                parent_inode.nlink += 1;
                                inode::write_inode(&parent_be.storage, local_parent, &parent_inode)
                                    .await?;
                            } else {
                                // Regular file (shared I{parent}): 16-byte mtime/ctime
                                // field patch, consistent with the same-volume path
                                // (design §3.8, PR 5).
                                inode::stage_parent_time_patch(
                                    &parent_be.storage,
                                    local_parent,
                                    now,
                                )
                                .await?;
                            }
                            Ok(())
                        })
                        .await?;
                }
                VolumeBackend::V3(be) => {
                    let update = if is_dir_flag {
                        kv::backend::RoutedParentUpdate::ExclusiveTimesBump
                    } else {
                        kv::backend::RoutedParentUpdate::SharedTimes
                    };
                    let out = be
                        .routed_add_dentry(
                            local_parent,
                            name,
                            global_child_ino,
                            final_mode & libc::S_IFMT,
                            update,
                        )
                        .await;
                    if out.is_err() {
                        self.mirror_v3_failure(parent_v_idx);
                    }
                    out?;
                }
            }

            Ok(Inode {
                ino: global_child_ino,
                ..child_inode
            })
        }
    }

    async fn unlink(&self, parent: Ino, name: &str) -> Result<Ino> {
        let (parent_v_idx, local_parent) = self.route_ino(parent);
        self.check_volume_enabled(parent_v_idx)?;

        // Two-phase child discovery (see dlm.rs): the child's I-lock may
        // live on any volume and must never be taken while holding this
        // volume's D-lock. Phase 1 reads the child; phase 2 re-locks the
        // full set — per-volume sets in ascending volume order, each set
        // internally canonical — and revalidates the dentry.
        //
        // Parent lock mode mirrors create (design §3.8 / PR 5): regular
        // unlink patches only the parent's mtime/ctime — SHARED parent, so
        // same-directory delete storms overlap. Directory removal mutates
        // parent nlink (full-slot RMW) — EXCLUSIVE.
        let (file_type, global_child_ino, child_v_idx, local_child, parent_shared, _guards) = loop {
            let phase1 = self.volumes[parent_v_idx]
                .dlm()
                .lock_many(
                    &[(local_parent, dlm::LockMode::Shared)],
                    &[(local_parent, name, dlm::LockMode::Exclusive)],
                )
                .await;
            let Some((global_child_ino, file_type)) = self
                .find_dentry_routed(parent_v_idx, local_parent, name)
                .await?
            else {
                return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Dentry not found",
                )));
            };
            let (child_v_idx, local_child) = self.route_ino(global_child_ino);
            self.check_volume_enabled(child_v_idx)?;

            // Self-references and directories take the exclusive-parent
            // path; a regular file can never be its own parent.
            let parent_shared = file_type != libc::S_IFDIR && parent != global_child_ino;
            let parent_mode = if parent_shared {
                dlm::LockMode::Shared
            } else {
                dlm::LockMode::Exclusive
            };
            drop(phase1);

            let mut guards = Vec::new();
            if parent == global_child_ino {
                guards.extend(
                    self.volumes[parent_v_idx]
                        .dlm()
                        .lock_many(
                            &[(local_parent, dlm::LockMode::Exclusive)],
                            &[(local_parent, name, dlm::LockMode::Exclusive)],
                        )
                        .await,
                );
            } else if child_v_idx == parent_v_idx {
                guards.extend(
                    self.volumes[parent_v_idx]
                        .dlm()
                        .lock_many(
                            &[
                                (local_parent, parent_mode),
                                (local_child, dlm::LockMode::Exclusive),
                            ],
                            &[(local_parent, name, dlm::LockMode::Exclusive)],
                        )
                        .await,
                );
            } else if child_v_idx < parent_v_idx {
                guards.extend(
                    self.volumes[child_v_idx]
                        .dlm()
                        .lock_many(&[(local_child, dlm::LockMode::Exclusive)], &[])
                        .await,
                );
                guards.extend(
                    self.volumes[parent_v_idx]
                        .dlm()
                        .lock_many(
                            &[(local_parent, parent_mode)],
                            &[(local_parent, name, dlm::LockMode::Exclusive)],
                        )
                        .await,
                );
            } else {
                guards.extend(
                    self.volumes[parent_v_idx]
                        .dlm()
                        .lock_many(
                            &[(local_parent, parent_mode)],
                            &[(local_parent, name, dlm::LockMode::Exclusive)],
                        )
                        .await,
                );
                guards.extend(
                    self.volumes[child_v_idx]
                        .dlm()
                        .lock_many(&[(local_child, dlm::LockMode::Exclusive)], &[])
                        .await,
                );
            }

            match self
                .find_dentry_routed(parent_v_idx, local_parent, name)
                .await?
            {
                Some((cur_child, _)) if cur_child == global_child_ino => {
                    break (
                        file_type,
                        global_child_ino,
                        child_v_idx,
                        local_child,
                        parent_shared,
                        guards,
                    );
                }
                _ => continue, // dentry changed under us — rediscover
            }
        };

        {
            if parent_v_idx == child_v_idx {
                let is_dir = file_type == libc::S_IFDIR;
                // v3 arm: ONE whole-tx entry with the routed semantics.
                if let VolumeBackend::V3(be) = &self.volumes[parent_v_idx] {
                    let out = be
                        .routed_unlink_local(local_parent, name, local_child, is_dir, parent_shared)
                        .await;
                    if out.is_err() {
                        self.mirror_v3_failure(parent_v_idx);
                    }
                    out?;
                    return Ok(global_child_ino);
                }
                let backend = self.v2_volume(parent_v_idx);
                backend
                    .run_transaction(|| async {
                        dentry::remove_dentry(&backend.storage, local_parent, name).await?;

                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos() as u64;
                        if parent_shared {
                            // Regular file (shared I{parent}): 16-byte
                            // mtime/ctime field patch (design §3.8) — a
                            // full-slot RMW here would clobber concurrent
                            // patchers.
                            inode::stage_parent_time_patch(&backend.storage, local_parent, now)
                                .await?;
                        } else {
                            // Directory (exclusive I{parent}): full-slot RMW
                            // (times + nlink).
                            let mut parent_inode =
                                inode::read_inode(&backend.storage, local_parent).await?;
                            parent_inode.mtime = now;
                            parent_inode.ctime = now;
                            if is_dir && parent_inode.nlink > 2 {
                                parent_inode.nlink -= 1;
                            }
                            inode::write_inode(&backend.storage, local_parent, &parent_inode)
                                .await?;
                        }

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
                        inode::write_inode(&backend.storage, local_child, &disk_inode).await?;
                        Ok(global_child_ino)
                    })
                    .await
            } else {
                let is_dir = file_type == libc::S_IFDIR;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;

                // Parent side: dentry removal + parent update.
                match &self.volumes[parent_v_idx] {
                    VolumeBackend::V2(be) => {
                        dentry::remove_dentry(&be.storage, local_parent, name).await?;
                        if parent_shared {
                            // Regular file (shared I{parent}): field patch (§3.8).
                            inode::stage_parent_time_patch(&be.storage, local_parent, now).await?;
                        } else {
                            // Directory (exclusive I{parent}): full-slot RMW.
                            let mut parent_inode =
                                inode::read_inode(&be.storage, local_parent).await?;
                            parent_inode.mtime = now;
                            parent_inode.ctime = now;
                            if is_dir && parent_inode.nlink > 2 {
                                parent_inode.nlink -= 1;
                            }
                            inode::write_inode(&be.storage, local_parent, &parent_inode).await?;
                        }
                    }
                    VolumeBackend::V3(be) => {
                        let update = if parent_shared {
                            kv::backend::RoutedParentUpdate::SharedTimes
                        } else if is_dir {
                            kv::backend::RoutedParentUpdate::ExclusiveTimesBump
                        } else {
                            kv::backend::RoutedParentUpdate::ExclusiveTimes
                        };
                        let out = be.routed_remove_dentry(local_parent, name, update).await;
                        if out.is_err() {
                            self.mirror_v3_failure(parent_v_idx);
                        }
                        out?;
                    }
                }

                // Child side: nlink discipline (dir ⇒ 0) + ctime.
                match &self.volumes[child_v_idx] {
                    VolumeBackend::V2(be) => {
                        let mut disk_inode = inode::read_inode(&be.storage, local_child).await?;
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
                        inode::write_inode(&be.storage, local_child, &disk_inode).await?;
                    }
                    VolumeBackend::V3(be) => {
                        let out = be.routed_nlink_adjust(local_child, -1, is_dir).await;
                        if out.is_err() {
                            self.mirror_v3_failure(child_v_idx);
                        }
                        out?;
                    }
                }
                Ok(global_child_ino)
            }
        }
    }

    async fn link(&self, ino: Ino, new_parent: Ino, new_name: &str) -> Result<Inode> {
        let (parent_v_idx, local_parent) = self.route_ino(new_parent);
        let (child_v_idx, local_child) = self.route_ino(ino);
        self.check_volume_enabled(parent_v_idx)?;
        self.check_volume_enabled(child_v_idx)?;

        // Both inodes are parameters: acquire the full set upfront —
        // per-volume sets in ascending volume order, each internally
        // canonical (the old sequence took I{child} after the D-guard, an
        // ABBA inversion under stripe collisions).
        let mut _guards = Vec::new();
        if child_v_idx == parent_v_idx {
            _guards.extend(
                self.volumes[parent_v_idx]
                    .dlm()
                    .lock_many(
                        &[
                            (local_parent, dlm::LockMode::Exclusive),
                            (local_child, dlm::LockMode::Exclusive),
                        ],
                        &[(local_parent, new_name, dlm::LockMode::Exclusive)],
                    )
                    .await,
            );
        } else if child_v_idx < parent_v_idx {
            _guards.extend(
                self.volumes[child_v_idx]
                    .dlm()
                    .lock_many(&[(local_child, dlm::LockMode::Exclusive)], &[])
                    .await,
            );
            _guards.extend(
                self.volumes[parent_v_idx]
                    .dlm()
                    .lock_many(
                        &[(local_parent, dlm::LockMode::Exclusive)],
                        &[(local_parent, new_name, dlm::LockMode::Exclusive)],
                    )
                    .await,
            );
        } else {
            _guards.extend(
                self.volumes[parent_v_idx]
                    .dlm()
                    .lock_many(
                        &[(local_parent, dlm::LockMode::Exclusive)],
                        &[(local_parent, new_name, dlm::LockMode::Exclusive)],
                    )
                    .await,
            );
            _guards.extend(
                self.volumes[child_v_idx]
                    .dlm()
                    .lock_many(&[(local_child, dlm::LockMode::Exclusive)], &[])
                    .await,
            );
        }

        if self
            .find_dentry_routed(parent_v_idx, local_parent, new_name)
            .await?
            .is_some()
        {
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "File already exists".to_string(),
            ));
        }

        if parent_v_idx == child_v_idx {
            // v3 arm: ONE whole-tx entry (nlink+1 + dentry + parent times).
            if let VolumeBackend::V3(be) = &self.volumes[parent_v_idx] {
                let out = be
                    .routed_link_local(local_parent, new_name, local_child, ino)
                    .await;
                if out.is_err() {
                    self.mirror_v3_failure(parent_v_idx);
                }
                return out;
            }
            let backend = self.v2_volume(parent_v_idx);
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
            // Child side: nlink+1 + ctime (per format).
            let child_inode = match &self.volumes[child_v_idx] {
                VolumeBackend::V2(be) => {
                    let mut disk_inode = inode::read_inode(&be.storage, local_child).await?;
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
                    inode::write_inode(&be.storage, local_child, &disk_inode).await?;
                    Inode {
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
                    }
                }
                VolumeBackend::V3(be) => {
                    let out = be.routed_nlink_adjust(local_child, 1, false).await;
                    if out.is_err() {
                        self.mirror_v3_failure(child_v_idx);
                    }
                    let v = out?;
                    Inode {
                        ino,
                        mode: v.mode,
                        uid: v.uid,
                        gid: v.gid,
                        size: v.size,
                        nlink: v.nlink,
                        atime: v.atime,
                        mtime: v.mtime,
                        ctime: v.ctime,
                        flags: v.flags,
                    }
                }
            };

            // Parent side: dentry + best-effort parent times.
            match &self.volumes[parent_v_idx] {
                VolumeBackend::V2(be) => {
                    dentry::insert_dentry(
                        &be.storage,
                        local_parent,
                        ino,
                        new_name,
                        child_inode.mode & libc::S_IFMT,
                    )
                    .await?;
                    if let Ok(mut parent_inode) = inode::read_inode(&be.storage, local_parent).await
                    {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos() as u64;
                        parent_inode.mtime = now;
                        parent_inode.ctime = now;
                        let _ = inode::write_inode(&be.storage, local_parent, &parent_inode).await;
                    }
                }
                VolumeBackend::V3(be) => {
                    let out = be
                        .routed_add_dentry(
                            local_parent,
                            new_name,
                            ino,
                            child_inode.mode & libc::S_IFMT,
                            kv::backend::RoutedParentUpdate::ExclusiveTimes,
                        )
                        .await;
                    if out.is_err() {
                        self.mirror_v3_failure(parent_v_idx);
                    }
                    out?;
                }
            }

            Ok(child_inode)
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

        // Per-volume lock sets in ascending volume order, each internally
        // canonical (I before D, stripe-deduped by lock_many). Interleaving
        // classes across volumes (old code: both parents, then both
        // dentries) descends the (volume, class) order and can ABBA against
        // cross-volume unlink/link.
        let mut _guards = Vec::new();
        if old_parent_v_idx == new_parent_v_idx {
            _guards.extend(
                self.volumes[old_parent_v_idx]
                    .dlm()
                    .lock_many(
                        &[
                            (local_old_parent, dlm::LockMode::Exclusive),
                            (local_new_parent, dlm::LockMode::Exclusive),
                        ],
                        &[
                            (local_old_parent, old_name, dlm::LockMode::Exclusive),
                            (local_new_parent, new_name, dlm::LockMode::Exclusive),
                        ],
                    )
                    .await,
            );
        } else {
            let mut sets = [
                (old_parent_v_idx, local_old_parent, old_name),
                (new_parent_v_idx, local_new_parent, new_name),
            ];
            sets.sort_unstable_by_key(|&(v, _, _)| v);
            for (v_idx, local_p, name) in sets {
                _guards.extend(
                    self.volumes[v_idx]
                        .dlm()
                        .lock_many(
                            &[(local_p, dlm::LockMode::Exclusive)],
                            &[(local_p, name, dlm::LockMode::Exclusive)],
                        )
                        .await,
                );
            }
        }

        let old_dentry_opt = self
            .find_dentry_routed(old_parent_v_idx, local_old_parent, old_name)
            .await?;
        let new_dentry_opt = self
            .find_dentry_routed(new_parent_v_idx, local_new_parent, new_name)
            .await?;

        if old_parent_v_idx == new_parent_v_idx {
            // v3 arm: dentry surgery + dir-move nlink shifts + local-dest
            // accounting as ONE whole-tx entry; a remote destination
            // inode is settled first (ENOTEMPTY aborts before any
            // surgery), matching the v2 arm's check-then-mutate order.
            if let VolumeBackend::V3(be) = &self.volumes[old_parent_v_idx] {
                let mut dest_local = None;
                if flags & libc::RENAME_EXCHANGE == 0 {
                    if let Some((dest_global, _ft)) = new_dentry_opt {
                        if flags & libc::RENAME_NOREPLACE != 0 {
                            return Err(crate::error::SqueezefsError::Io(
                                std::io::Error::from_raw_os_error(libc::EEXIST),
                            ));
                        }
                        let (dest_v_idx, local_dest) = self.route_ino(dest_global);
                        if dest_v_idx == old_parent_v_idx {
                            dest_local = Some(local_dest);
                        } else {
                            self.dest_replace_routed(dest_v_idx, local_dest).await?;
                        }
                    }
                }
                let out = be
                    .routed_rename_local(
                        local_old_parent,
                        old_name,
                        local_new_parent,
                        new_name,
                        flags,
                        dest_local,
                    )
                    .await;
                if out.is_err() {
                    self.mirror_v3_failure(old_parent_v_idx);
                }
                return out;
            }

            let backend = self.v2_volume(old_parent_v_idx);
            backend
                .run_transaction(|| async {
                    // Pre-acquire both dentry buckets in ascending order so a
                    // concurrent multi-bucket rename cannot invert the order (§3.6).
                    dentry::tx_prelock_buckets(
                        &backend.storage,
                        &[(local_old_parent, old_name), (local_new_parent, new_name)],
                    )
                    .await?;
                    if flags & libc::RENAME_EXCHANGE != 0 {
                        let (old_child, old_ft) = old_dentry_opt.ok_or_else(|| {
                            crate::error::SqueezefsError::Io(std::io::Error::from_raw_os_error(
                                libc::ENOENT,
                            ))
                        })?;
                        let (new_child, new_ft) = new_dentry_opt.ok_or_else(|| {
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
                            old_child,
                            new_name,
                            old_ft,
                        )
                        .await?;

                        dentry::insert_dentry(
                            &backend.storage,
                            local_old_parent,
                            new_child,
                            old_name,
                            new_ft,
                        )
                        .await?;

                        Ok(())
                    } else {
                        if flags & libc::RENAME_NOREPLACE != 0 && new_dentry_opt.is_some() {
                            return Err(crate::error::SqueezefsError::Io(
                                std::io::Error::from_raw_os_error(libc::EEXIST),
                            ));
                        }

                        if let Some((old_child, old_ft)) = old_dentry_opt {
                            let is_dir = old_ft == libc::S_IFDIR;
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
                            if let Some((dest_ino, _dest_ft)) = new_dentry_opt {
                                let (dest_v_idx, local_dest) = self.route_ino(dest_ino);
                                self.dest_replace_routed(dest_v_idx, local_dest).await?;
                                // Remove the destination dentry so it gets replaced cleanly
                                dentry::remove_dentry(&backend.storage, local_new_parent, new_name)
                                    .await?;
                            }

                            dentry::remove_dentry(&backend.storage, local_old_parent, old_name)
                                .await?;
                            dentry::insert_dentry(
                                &backend.storage,
                                local_new_parent,
                                old_child,
                                new_name,
                                old_ft,
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
                let (old_child, old_ft) = old_dentry_opt.ok_or_else(|| {
                    crate::error::SqueezefsError::Io(std::io::Error::from_raw_os_error(
                        libc::ENOENT,
                    ))
                })?;
                let (new_child, new_ft) = new_dentry_opt.ok_or_else(|| {
                    crate::error::SqueezefsError::Io(std::io::Error::from_raw_os_error(
                        libc::ENOENT,
                    ))
                })?;

                // Remove both, insert swapped — per-volume fragments, each
                // dispatched by format (the v2 cross arms were already
                // non-transactional across volumes).
                self.remove_dentry_routed(old_parent_v_idx, local_old_parent, old_name)
                    .await?;
                self.remove_dentry_routed(new_parent_v_idx, local_new_parent, new_name)
                    .await?;
                self.insert_dentry_routed(
                    new_parent_v_idx,
                    local_new_parent,
                    old_child,
                    new_name,
                    old_ft,
                )
                .await?;
                self.insert_dentry_routed(
                    old_parent_v_idx,
                    local_old_parent,
                    new_child,
                    old_name,
                    new_ft,
                )
                .await?;

                Ok(())
            } else {
                if flags & libc::RENAME_NOREPLACE != 0 && new_dentry_opt.is_some() {
                    return Err(crate::error::SqueezefsError::Io(
                        std::io::Error::from_raw_os_error(libc::EEXIST),
                    ));
                }

                if let Some((old_child, old_ft)) = old_dentry_opt {
                    let is_dir = old_ft == libc::S_IFDIR;

                    if is_dir {
                        // Directory move across parents: nlink shift on
                        // each side (best-effort, the v2 shape).
                        self.parent_nlink_delta_routed(old_parent_v_idx, local_old_parent, -1)
                            .await?;
                        self.parent_nlink_delta_routed(new_parent_v_idx, local_new_parent, 1)
                            .await?;
                    }

                    // Destination replacement: settle its inode, then
                    // remove its dentry.
                    if let Some((dest_ino, _dest_ft)) = new_dentry_opt {
                        let (dest_v_idx, local_dest) = self.route_ino(dest_ino);
                        self.dest_replace_routed(dest_v_idx, local_dest).await?;
                        self.remove_dentry_routed(new_parent_v_idx, local_new_parent, new_name)
                            .await?;
                    }

                    self.remove_dentry_routed(old_parent_v_idx, local_old_parent, old_name)
                        .await?;
                    self.insert_dentry_routed(
                        new_parent_v_idx,
                        local_new_parent,
                        old_child,
                        new_name,
                        old_ft,
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

    async fn readdir(&self, dir: Ino, offset: u64, max: usize) -> Result<Vec<DirEntry>> {
        let (v_idx, local_dir) = self.route_ino(dir);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx].dlm().lock_inode_shared(local_dir).await;
        match &self.volumes[v_idx] {
            // v2 ignores `offset`/`max` — its documented behavior,
            // byte-identical; the FUSE layer owns offset slicing there.
            VolumeBackend::V2(be) => {
                let dentries = dentry::list_dentries(&be.storage, local_dir).await?;
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
            // PR K7 (§5.1): v3 honors `offset`/`max` — offset is a
            // readdir cookie, pages resume strictly after its key suffix.
            VolumeBackend::V3(be) => be.readdir(local_dir, offset, max).await,
        }
    }

    async fn getattr(&self, ino: Ino) -> Result<Inode> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx].dlm().lock_inode_shared(local_ino).await;
        let mut inode = self.read_inode_routed(v_idx, local_ino).await?;
        inode.ino = ino;
        Ok(inode)
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
            .dlm()
            .lock_inode_exclusive(local_ino)
            .await;
        if let VolumeBackend::V3(be) = &self.volumes[v_idx] {
            let out = be
                .setattr_locked(local_ino, mode, uid, gid, size, atime, mtime, ctime)
                .await;
            if out.is_err() {
                self.mirror_v3_failure(v_idx);
            }
            return out.map(|mut i| {
                i.ino = ino;
                i
            });
        }
        let be = self.v2_volume(v_idx);
        let mut disk_inode = inode::read_inode(&be.storage, local_ino).await?;
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
        inode::write_inode(&be.storage, local_ino, &disk_inode).await?;
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
        let _guard = self.volumes[v_idx].dlm().lock_inode_shared(local_ino).await;
        match &self.volumes[v_idx] {
            VolumeBackend::V2(be) => xattr::get_xattr(&be.storage, local_ino, name).await,
            VolumeBackend::V3(be) => be.getxattr(local_ino, name).await,
        }
    }

    async fn setxattr(&self, ino: Ino, name: &str, value: &[u8]) -> Result<()> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx]
            .dlm()
            .lock_inode_exclusive(local_ino)
            .await;
        match &self.volumes[v_idx] {
            VolumeBackend::V2(be) => xattr::set_xattr(&be.storage, local_ino, name, value).await,
            VolumeBackend::V3(be) => {
                let out = be.setxattr_locked(local_ino, name, value).await;
                if out.is_err() {
                    self.mirror_v3_failure(v_idx);
                }
                out
            }
        }
    }

    async fn removexattr(&self, ino: Ino, name: &str) -> Result<()> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx]
            .dlm()
            .lock_inode_exclusive(local_ino)
            .await;
        match &self.volumes[v_idx] {
            VolumeBackend::V2(be) => xattr::remove_xattr(&be.storage, local_ino, name).await,
            VolumeBackend::V3(be) => {
                let out = be.removexattr_locked(local_ino, name).await;
                if out.is_err() {
                    self.mirror_v3_failure(v_idx);
                }
                out
            }
        }
    }

    async fn listxattr(&self, ino: Ino) -> Result<Vec<String>> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx].dlm().lock_inode_shared(local_ino).await;
        match &self.volumes[v_idx] {
            VolumeBackend::V2(be) => xattr::list_xattrs(&be.storage, local_ino).await,
            VolumeBackend::V3(be) => be.listxattr(local_ino).await,
        }
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
            match vol {
                VolumeBackend::V2(be) => {
                    crate::uring_fs::fdatasync(be.storage.device_path()).await?
                }
                // v3: the coalesced barrier ALSO drains the §4.6 pt 3
                // pending-reclaim bookkeeping.
                VolumeBackend::V3(be) => be.sync_device().await?,
            }
        }
        Ok(())
    }

    /// fdatasync only the MetaLV volume that owns `ino` (avoid multi-volume fsync tax).
    pub async fn sync_device_for_ino(&self, ino: Ino) -> Result<()> {
        let (v_idx, _) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        crate::fuse_client::METRICS
            .meta_sync_requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.volumes[v_idx].sync_device().await
    }

    /// Persist layout xattr + size with fine locks (no journal transaction_lock).
    /// Used on fsync/release writeback; avoids serializing all layout commits.
    pub async fn set_layout_and_size(&self, ino: Ino, layout: &[u8], size: u64) -> Result<()> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        match &self.volumes[v_idx] {
            VolumeBackend::V2(be) => {
                let _guard = be.dlm.lock_inode_exclusive(local_ino).await;
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
            // §5.3: ONE two-record transaction on v3 (its own I-guard).
            VolumeBackend::V3(be) => {
                let out = be.set_layout_and_size(local_ino, layout, size).await;
                if out.is_err() {
                    self.mirror_v3_failure(v_idx);
                }
                out
            }
        }
    }
}

/// The mounted volume set's **filesystem generation identity** — what a
/// `squeezefs format` invocation changes and nothing else does. Local NVMe
/// staging is bound to this identity (`cache::nvme` stamps it into every
/// staging dir and discards staging content stamped by a DEAD generation
/// before any recovery/seeding runs — the reformat-over-stale-staging
/// poisoning fix).
///
/// Per-volume identity, strongest existing stamp per format:
/// - **v3**: the superblock `uuid` (`kv::superblock::SuperblockV3::uuid`) —
///   random at every format ([`kv::builder::BuilderConfig::new`]), read
///   straight off sector 0 without mounting the volume.
/// - **v2**: the v2 superblock is deterministic (no uuid, no timestamp), so
///   the identity comes from the `user.squeezefs.format_config` root xattr
///   the CLI records on the FIRST volume: its `fs_uuid` field (random per
///   format invocation since this fix; a config-JSON addition, NOT a v2
///   on-disk format change) when present, else a content hash of the raw
///   config bytes (pre-fix volumes), else the `unstamped` sentinel
///   (non-first v2 volumes carry no config). Pre-fix staging dirs carry no
///   marker at all, so they are discarded once regardless — the weak v2
///   fallbacks only ever have to distinguish generations for volumes
///   formatted before `fs_uuid` existed.
///
/// The set identity is the ORDERED join of per-volume identities:
/// `route_ino` stripes by volume order, so a reordered volume set is a
/// different metadata view and must not adopt the old set's staging.
pub async fn volume_set_generation(meta_lvs: &[String]) -> Result<String> {
    use std::fmt::Write as _;
    let mut parts = Vec::with_capacity(meta_lvs.len());
    for path in meta_lvs {
        let part = match kv::superblock::classify_volume(std::path::Path::new(path)).await? {
            kv::superblock::VolumeFormat::Blank => {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "Metadata volume {path} is not formatted (zeroed superblock) — cannot \
                     derive a filesystem generation; run `squeezefs format` first"
                )));
            }
            kv::superblock::VolumeFormat::V3(sb) => {
                let mut s = String::with_capacity(3 + 32);
                s.push_str("v3:");
                for b in sb.uuid {
                    let _ = write!(s, "{b:02x}");
                }
                s
            }
            kv::superblock::VolumeFormat::V2(_) => {
                // The unchanged v2 open path (same 128 MiB file floor as
                // `VolumeBackend::open_for_mount`).
                let storage = storage::MetaLvStorage::open(path, 128 * 1024 * 1024)?;
                match xattr::get_xattr(&storage, 1, kv::builder::FORMAT_CONFIG_XATTR).await? {
                    Some(bytes) => {
                        let fs_uuid = serde_json::from_slice::<crate::FormatConfig>(&bytes)
                            .ok()
                            .and_then(|cfg| cfg.fs_uuid);
                        match fs_uuid {
                            Some(uuid) => format!("v2:{uuid}"),
                            None => {
                                format!("v2:cfg-{:016x}", xxhash_rust::xxh3::xxh3_64(&bytes))
                            }
                        }
                    }
                    None => "v2:unstamped".to_string(),
                }
            }
        };
        parts.push(part);
    }
    Ok(parts.join("|"))
}

#[cfg(test)]
mod flusher_tests {
    use super::*;

    /// PR 4 flusher lifecycle (§4.2, R6): the deferred-flush task is tied to
    /// the backend via the `needs_flush` Arc sentinel — dropping the backend
    /// releases the task's clone at its next tick, proven here by watching
    /// the `Weak` die. Guards dismount against leaked per-volume timers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_flusher_task_exits_when_backend_drops() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let storage = storage::MetaLvStorage::open(tmp.path(), 64 * 1024 * 1024).unwrap();
        let mut backend = MetaLvBackend::new(storage);
        backend.flush_interval_ms = 1; // fast ticks; no env games

        let weak = std::sync::Arc::downgrade(&backend.needs_flush);
        backend.ensure_flusher().await;
        // The task holds the only other strong clone now.
        backend
            .needs_flush
            .store(true, std::sync::atomic::Ordering::SeqCst);
        drop(backend);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while weak.upgrade().is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "flusher task leaked past backend drop (sentinel still alive)"
            );
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }

    /// Knob resolution (§4.2): canonical `SQUEEZEFS_META_FLUSH_INTERVAL_MS`
    /// wins over the legacy `SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS` alias;
    /// the alias alone still works; default is 50 ms. (Serial gate: env is
    /// process-global.)
    #[test]
    fn test_flush_interval_env_alias_precedence() {
        const NEW: &str = "SQUEEZEFS_META_FLUSH_INTERVAL_MS";
        const OLD: &str = "SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS";
        let saved = (std::env::var(NEW).ok(), std::env::var(OLD).ok());

        std::env::remove_var(NEW);
        std::env::remove_var(OLD);
        assert_eq!(resolve_flush_interval_ms(), 50, "default is 50 ms");

        std::env::set_var(OLD, "7");
        assert_eq!(resolve_flush_interval_ms(), 7, "legacy alias honored");

        std::env::set_var(NEW, "13");
        assert_eq!(
            resolve_flush_interval_ms(),
            13,
            "canonical name wins over alias"
        );

        match saved.0 {
            Some(v) => std::env::set_var(NEW, v),
            None => std::env::remove_var(NEW),
        }
        match saved.1 {
            Some(v) => std::env::set_var(OLD, v),
            None => std::env::remove_var(OLD),
        }
    }
}
