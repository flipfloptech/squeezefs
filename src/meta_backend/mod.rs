pub mod atomicity;
pub mod dlm;
pub mod kv;
pub mod reservation;
pub mod sync_coalescer;

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

/// Resolve the deferred-flush interval knob: canonical name first, legacy
/// alias second, default 50 ms (design-wal-crash-consistency §4.2 — a
/// `JOURNAL_`-named knob controlling a flusher with no journal is a
/// permanent naming wart; the alias keeps old operator scripts working).
/// The v3 backend reuses it as the journal/checkpoint cadence
/// (design-cow-kv-metadata §4.6; `0` = strict per-commit barriers).
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

/// The loud, precise refusal for legacy format-v2 volumes: v2 support was
/// removed entirely (always forward — no backwards compatibility). One
/// message shape shared by every surface that can meet a v2 superblock
/// (mount open, generation derivation) so operators always see the same
/// actionable text.
pub(crate) fn v2_unsupported_error(path: &str) -> crate::error::SqueezefsError {
    crate::error::SqueezefsError::InvalidOperation(format!(
        "Metadata volume {path} is format v2, which is no longer supported (this binary mounts \
         only format v3) — v2 volumes must be reformatted: run `squeezefs format` (destroys the \
         old contents)"
    ))
}

/// Open `path` for mounting, gated by the sector-0 classification
/// (`kv::superblock::classify_volume`): v3 superblocks mount the KV
/// backend (SB → ledger → bitmap → replay); blank volumes fail loud with
/// the actionable "run `squeezefs format`"; **format-v2 superblocks fail
/// loud as no longer supported** (reformat required); foreign magic, torn
/// superblocks, versions above 3, and unknown incompat feature bits fail
/// loud from the gate itself.
pub async fn open_volume_for_mount(
    path: &str,
) -> Result<std::sync::Arc<kv::backend::KvMetaBackend>> {
    open_volume_gated(path, false).await
}

/// [`open_volume_for_mount`]'s **read-only probe** twin (same version-gate
/// refusals): full bootstrap + RAM replay but no checkpoint task, so
/// nothing is ever written — the bootstrap config read and status paths
/// use it and drop the backend when done.
pub async fn open_volume_probe(path: &str) -> Result<std::sync::Arc<kv::backend::KvMetaBackend>> {
    open_volume_gated(path, true).await
}

async fn open_volume_gated(
    path: &str,
    probe: bool,
) -> Result<std::sync::Arc<kv::backend::KvMetaBackend>> {
    match kv::superblock::classify_volume(std::path::Path::new(path)).await? {
        kv::superblock::VolumeFormat::Blank => {
            Err(crate::error::SqueezefsError::InvalidOperation(format!(
                "Metadata volume {path} is not formatted (zeroed superblock) — run \
                 `squeezefs format` first"
            )))
        }
        kv::superblock::VolumeFormat::V2Legacy => Err(v2_unsupported_error(path)),
        kv::superblock::VolumeFormat::V3(_) => {
            let p = std::path::Path::new(path);
            Ok(if probe {
                kv::backend::KvMetaBackend::open_probe(p).await?
            } else {
                kv::backend::KvMetaBackend::open(p).await?
            })
        }
    }
}

/// Open a mount's whole metadata volume set **in set order**, each volume
/// through the version gate + the D0 single-writer guard
/// (design-metadata-throughput §5.0: "multi-volume mounts claim every meta
/// volume in the set (volume order)"). A guard refusal or open failure on
/// volume k releases the guards already taken on volumes `0..k` — flocks,
/// `writer_claim` records, and NVMe reservations — via each backend's
/// clean shutdown, then propagates the volume-k error loud.
pub async fn open_meta_volume_set(
    paths: &[String],
) -> Result<Vec<std::sync::Arc<kv::backend::KvMetaBackend>>> {
    let mut opened: Vec<std::sync::Arc<kv::backend::KvMetaBackend>> = Vec::new();
    for path in paths {
        match open_volume_for_mount(path).await {
            Ok(be) => opened.push(be),
            Err(e) => {
                for prior in &opened {
                    if let Err(te) = prior.shutdown().await {
                        log::warn!(
                            "releasing guard on {:?} after a failed set open failed too: {te}",
                            prior.device_path()
                        );
                    }
                }
                return Err(e);
            }
        }
    }
    Ok(opened)
}

/// The routed multi-volume metadata backend: `volumes` stripes inos over
/// per-volume [`kv::backend::KvMetaBackend`]s (format v3 is the only
/// metadata format). `Arc`-shared (not `Clone`): the volumes own live
/// backends (checkpoint tasks, caches) that must not fork.
pub struct RoutedMetaBackend {
    pub volumes: Vec<std::sync::Arc<kv::backend::KvMetaBackend>>,
    pub disabled_volumes: std::sync::Arc<dashmap::DashMap<usize, bool, ahash::RandomState>>,
    pub redirections: std::sync::Arc<dashmap::DashMap<usize, usize, ahash::RandomState>>,
}

impl RoutedMetaBackend {
    pub fn new(volumes: Vec<std::sync::Arc<kv::backend::KvMetaBackend>>) -> Self {
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

    /// Routed dentry read: `(stored child ino (global), S_IFMT bits)`.
    async fn find_dentry_routed(
        &self,
        idx: usize,
        local_parent: Ino,
        name: &str,
    ) -> Result<Option<(Ino, u32)>> {
        self.volumes[idx]
            .routed_find_dentry(local_parent, name)
            .await
    }

    /// §4.4 pt 4 escalation mirror: after any mutation error, latch the
    /// volume into `disabled_volumes` iff its backend has fail-stopped
    /// (repeated journal write failures) — the existing mechanism
    /// `check_volume_enabled` consults.
    fn mirror_volume_failure(&self, idx: usize) {
        if self.volumes[idx].is_failed() {
            self.disabled_volumes.insert(idx, true);
        }
    }

    /// Rename fragment: pure dentry removal on one volume (the caller
    /// holds the D-guard; `guards` is its Arc'd set — PR M7 Issue 13:
    /// multi-commit ops clone one set per sequential commit).
    async fn remove_dentry_routed(
        &self,
        idx: usize,
        local_parent: Ino,
        name: &str,
        guards: std::sync::Arc<[dlm::DlmGuard]>,
    ) -> Result<()> {
        let out = self.volumes[idx]
            // PR M6 D4.b: cross-volume rename fragments carry the POSIX
            // parent-time update (rename holds both parents EXCLUSIVE).
            .routed_remove_dentry(
                local_parent,
                name,
                kv::backend::RoutedParentUpdate::ExclusiveTimes,
                guards,
            )
            .await;
        if out.is_err() {
            self.mirror_volume_failure(idx);
        }
        out
    }

    /// Rename fragment: dentry insertion + parent times on one volume
    /// (`ft_bits` = `mode & S_IFMT`).
    async fn insert_dentry_routed(
        &self,
        idx: usize,
        local_parent: Ino,
        global_child: Ino,
        name: &str,
        ft_bits: u32,
        guards: std::sync::Arc<[dlm::DlmGuard]>,
    ) -> Result<()> {
        let out = self.volumes[idx]
            .routed_add_dentry(
                local_parent,
                name,
                global_child,
                ft_bits,
                // PR M6 D4.b: parent times ride the fragment (exclusive
                // parent I-guard held by the rename).
                kv::backend::RoutedParentUpdate::ExclusiveTimes,
                guards,
            )
            .await;
        if out.is_err() {
            self.mirror_volume_failure(idx);
        }
        out
    }

    /// Rename fragment (PR M6 D4.b): stamp the moved/exchanged inode's
    /// ctime when it lives on a DIFFERENT volume than the dentry surgery
    /// (the same-volume path stages it inside the rename tx).
    async fn touch_ctime_routed(
        &self,
        idx: usize,
        local_ino: Ino,
        guards: std::sync::Arc<[dlm::DlmGuard]>,
    ) -> Result<()> {
        self.check_volume_enabled(idx)?;
        let out = self.volumes[idx]
            .routed_touch_ctime(local_ino, guards)
            .await;
        if out.is_err() {
            self.mirror_volume_failure(idx);
        }
        out
    }

    /// Rename fragment: directory-move parent nlink shift, best-effort.
    async fn parent_nlink_delta_routed(
        &self,
        idx: usize,
        local_parent: Ino,
        delta: i64,
        guards: std::sync::Arc<[dlm::DlmGuard]>,
    ) -> Result<()> {
        let out = self.volumes[idx]
            .routed_parent_nlink_delta(local_parent, delta, guards)
            .await;
        if out.is_err() {
            self.mirror_volume_failure(idx);
        }
        out
    }

    /// Rename fragment: destination-inode replacement accounting —
    /// ENOTEMPTY probe for directories, nlink dec + ctime (best-effort on
    /// a missing inode).
    async fn dest_replace_routed(
        &self,
        idx: usize,
        local_dest: Ino,
        guards: std::sync::Arc<[dlm::DlmGuard]>,
    ) -> Result<()> {
        let out = self.volumes[idx]
            .routed_dest_replace(local_dest, guards)
            .await;
        if out.is_err() {
            self.mirror_volume_failure(idx);
        }
        out
    }

    /// LOCK-FREE inode read (no DLM acquisition — safe under held routed
    /// I-guards, where a re-entrant stripe read can deadlock against a
    /// queued writer).
    async fn read_inode_routed(&self, idx: usize, local_ino: Ino) -> Result<Inode> {
        self.volumes[idx].getattr(local_ino).await
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
        // Health only used for dir placement; a short cache avoids scanning
        // allocator occupancy on every mkdir under multi-thread load.
        static HEALTH_CACHE: once_cell::sync::Lazy<scc::HashMap<usize, (u32, std::time::Instant)>> =
            once_cell::sync::Lazy::new(scc::HashMap::new);
        if let Some(v) = HEALTH_CACHE.read_sync(&idx, |_, v| *v) {
            if v.1.elapsed() < std::time::Duration::from_millis(500) {
                return v.0;
            }
        }
        // Estimated remaining-capacity FRACTION: free extents / total
        // (resolved OQ 5, design §4.9).
        let be = &self.volumes[idx];
        let total = be.superblock().total_extents();
        let free_factor = if total > 0 {
            be.free_extents() as f64 / total as f64
        } else {
            0.0
        };

        let score = (free_factor * 1000.0) as u32;
        let score = score.min(1000);
        let _ = HEALTH_CACHE.upsert_sync(idx, (score, std::time::Instant::now()));
        score
    }

    /// Batched reclaim (design-wal-crash-consistency §4.5): group the inos
    /// by owning volume and destroy each volume's group as one
    /// `destroy_inodes` transaction. All-or-nothing per call — the first
    /// failing volume aborts and the caller bisects (volume grouping is
    /// deterministic, so halves re-route consistently and converge to
    /// singletons).
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

    /// The per-ino xattr value cap — the largest inline xattr value
    /// `ino`'s volume can hold (design-cow-kv-metadata §5.3): the
    /// record-value cap `min(65_536, node_size/4)` (§4.2). A *non-trait*
    /// capability accessor (the `Metadata` trait stays unchanged, §5.1);
    /// the layout inline-spill decision
    /// (`DataRouter::save_metadata_to_backend`) consults it, so a volume
    /// set with differing `node_size` spills per volume by serialized
    /// size.
    pub fn xattr_value_cap(&self, ino: Ino) -> usize {
        let (v_idx, _) = self.route_ino(ino);
        self.volumes[v_idx].record_value_cap()
    }

    /// One cookie-paged readdir step against `dir`'s volume (design
    /// §5.1): pages of at most `max` `(resume_cookie, entry)` pairs, each
    /// entry paired with its resume cookie
    /// (`3 + ((hash54 << 8) | coll_seq)`). Dentry child inos are stored
    /// global, so no mapping. Takes the same per-volume shared inode
    /// guard as the trait `readdir`.
    pub async fn readdir_stream(
        &self,
        dir: Ino,
        offset: u64,
        max: usize,
    ) -> Result<Vec<(u64, DirEntry)>> {
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
        // holding a D-stripe while waiting on an I-stripe inverts the class
        // order: ABBA against unlink's held child-I under stripe
        // collisions). Snapshot semantics are unchanged — lookup→getattr
        // was never atomic.
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

        // Directories need the EXCLUSIVE parent lock (parent nlink RMW).
        // Regular creates take a SHARED parent lock: they only update the
        // parent's mtime/ctime (never nlink/mode), so same-dir regular
        // creates run concurrently, while the shared lock still serializes
        // against any exclusive parent mutator (mkdir/setattr/unlink/
        // rename) — design §3.8.
        let parent_guard = if is_dir {
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
        let dentry_guard = self.volumes[parent_v_idx]
            .dlm()
            .lock_dentry_exclusive(local_parent, name)
            .await;
        // PR M7 (Issue 13): the op's guard set travels with each commit
        // (cloned per fragment for the cross-volume shape).
        let guards: std::sync::Arc<[dlm::DlmGuard]> =
            std::sync::Arc::from(vec![parent_guard, dentry_guard]);

        if parent_v_idx == target_v_idx {
            // Same-volume create: ONE whole-tx journal entry with the
            // routed semantics (design §4.4).
            let be = &self.volumes[target_v_idx];
            let out = be
                .routed_create_local(
                    local_parent,
                    name,
                    mode,
                    uid,
                    gid,
                    |local| self.make_global_ino(local, target_v_idx),
                    guards,
                )
                .await;
            if out.is_err() {
                self.mirror_volume_failure(target_v_idx);
            }
            out
        } else {
            // Cross-volume create: mutate each volume only through its own
            // whole-tx commit (mixed-volume sets stripe directories by
            // health, §4.9).
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
            let target_be = &self.volumes[target_v_idx];
            let new_local_ino = target_be.allocate_ino();
            let minted = target_be
                .routed_mint_inode(new_local_ino, final_mode, uid, final_gid, guards.clone())
                .await;
            if minted.is_err() {
                self.mirror_volume_failure(target_v_idx);
            }
            let v = minted?;
            let child_inode = Inode {
                ino: new_local_ino,
                mode: v.mode,
                uid: v.uid,
                gid: v.gid,
                size: v.size,
                nlink: v.nlink,
                atime: v.atime,
                mtime: v.mtime,
                ctime: v.ctime,
                flags: v.flags,
            };

            let global_child_ino = self.make_global_ino(new_local_ino, target_v_idx);

            // Parent side: the dentry (global child ino) + parent update.
            let update = if is_dir_flag {
                kv::backend::RoutedParentUpdate::ExclusiveTimesBump
            } else {
                kv::backend::RoutedParentUpdate::SharedTimes
            };
            let out = self.volumes[parent_v_idx]
                .routed_add_dentry(
                    local_parent,
                    name,
                    global_child_ino,
                    final_mode & libc::S_IFMT,
                    update,
                    guards.clone(),
                )
                .await;
            if out.is_err() {
                self.mirror_volume_failure(parent_v_idx);
            }
            out?;

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
        // Parent lock mode mirrors create (design §3.8): regular unlink
        // touches only the parent's mtime/ctime — SHARED parent, so
        // same-directory delete storms overlap. Directory removal mutates
        // parent nlink — EXCLUSIVE.
        let (file_type, global_child_ino, child_v_idx, local_child, parent_shared, guards) = loop {
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
                    // PR M7 (Issue 13): Arc the op's guard set — each
                    // commit below co-owns it.
                    let guards: std::sync::Arc<[dlm::DlmGuard]> = std::sync::Arc::from(guards);
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

        let is_dir = file_type == libc::S_IFDIR;
        if parent_v_idx == child_v_idx {
            // Same-volume unlink: ONE whole-tx entry with the routed
            // semantics.
            let be = &self.volumes[parent_v_idx];
            let out = be
                .routed_unlink_local(
                    local_parent,
                    name,
                    local_child,
                    is_dir,
                    parent_shared,
                    guards,
                )
                .await;
            if out.is_err() {
                self.mirror_volume_failure(parent_v_idx);
            }
            out?;
            Ok(global_child_ino)
        } else {
            // Parent side: dentry removal + parent update.
            let update = if parent_shared {
                kv::backend::RoutedParentUpdate::SharedTimes
            } else if is_dir {
                kv::backend::RoutedParentUpdate::ExclusiveTimesBump
            } else {
                kv::backend::RoutedParentUpdate::ExclusiveTimes
            };
            let out = self.volumes[parent_v_idx]
                .routed_remove_dentry(local_parent, name, update, guards.clone())
                .await;
            if out.is_err() {
                self.mirror_volume_failure(parent_v_idx);
            }
            out?;

            // Child side: nlink discipline (dir ⇒ 0) + ctime.
            let out = self.volumes[child_v_idx]
                .routed_nlink_adjust(local_child, -1, is_dir, guards.clone())
                .await;
            if out.is_err() {
                self.mirror_volume_failure(child_v_idx);
            }
            out?;
            Ok(global_child_ino)
        }
    }

    async fn link(&self, ino: Ino, new_parent: Ino, new_name: &str) -> Result<Inode> {
        let (parent_v_idx, local_parent) = self.route_ino(new_parent);
        let (child_v_idx, local_child) = self.route_ino(ino);
        self.check_volume_enabled(parent_v_idx)?;
        self.check_volume_enabled(child_v_idx)?;

        // Both inodes are parameters: acquire the full set upfront —
        // per-volume sets in ascending volume order, each internally
        // canonical (taking I{child} after the D-guard would be an ABBA
        // inversion under stripe collisions).
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

        // PR M7 (Issue 13): Arc the op's guard set for its commit(s).
        let guards: std::sync::Arc<[dlm::DlmGuard]> = std::sync::Arc::from(_guards);

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
            // Same-volume link: ONE whole-tx entry (nlink+1 + dentry +
            // parent times).
            let be = &self.volumes[parent_v_idx];
            let out = be
                .routed_link_local(local_parent, new_name, local_child, ino, guards)
                .await;
            if out.is_err() {
                self.mirror_volume_failure(parent_v_idx);
            }
            out
        } else {
            // Child side: nlink+1 + ctime.
            let out = self.volumes[child_v_idx]
                .routed_nlink_adjust(local_child, 1, false, guards.clone())
                .await;
            if out.is_err() {
                self.mirror_volume_failure(child_v_idx);
            }
            let v = out?;
            let child_inode = Inode {
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
            };

            // Parent side: dentry + best-effort parent times.
            let out = self.volumes[parent_v_idx]
                .routed_add_dentry(
                    local_parent,
                    new_name,
                    ino,
                    child_inode.mode & libc::S_IFMT,
                    kv::backend::RoutedParentUpdate::ExclusiveTimes,
                    guards.clone(),
                )
                .await;
            if out.is_err() {
                self.mirror_volume_failure(parent_v_idx);
            }
            out?;

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
        // classes across volumes descends the (volume, class) order and can
        // ABBA against cross-volume unlink/link.
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

        // PR M7 (Issue 13): Arc the op's guard set — the same-volume
        // one-tx shape takes it once; the cross-volume fragments clone it
        // per sequential commit.
        let guards: std::sync::Arc<[dlm::DlmGuard]> = std::sync::Arc::from(_guards);

        let old_dentry_opt = self
            .find_dentry_routed(old_parent_v_idx, local_old_parent, old_name)
            .await?;
        let new_dentry_opt = self
            .find_dentry_routed(new_parent_v_idx, local_new_parent, new_name)
            .await?;

        if old_parent_v_idx == new_parent_v_idx {
            // Same-volume rename: dentry surgery + dir-move nlink shifts +
            // parent Δtimes + local-dest accounting + the moved inode's
            // Δctime as ONE whole-tx entry (PR M6 D4.b); a remote
            // destination inode is settled first (ENOTEMPTY aborts before
            // any surgery) — check-then-mutate order — and a remote
            // moved/exchanged inode gets its ctime as a per-volume
            // fragment after the surgery (cross-volume renames were never
            // transactional across volumes).
            let be = &self.volumes[old_parent_v_idx];
            let (src_local, src_remote) = match old_dentry_opt {
                Some((src_global, _ft)) => {
                    let (v, l) = self.route_ino(src_global);
                    if v == old_parent_v_idx {
                        (Some(l), None)
                    } else {
                        (None, Some((v, l)))
                    }
                }
                None => (None, None),
            };
            let mut dest_local = None;
            let mut dest_remote = None;
            if flags & libc::RENAME_EXCHANGE != 0 {
                if let Some((dest_global, _ft)) = new_dentry_opt {
                    let (v, l) = self.route_ino(dest_global);
                    if v == old_parent_v_idx {
                        dest_local = Some(l);
                    } else {
                        dest_remote = Some((v, l));
                    }
                }
            } else if let Some((dest_global, _ft)) = new_dentry_opt {
                if flags & libc::RENAME_NOREPLACE != 0 {
                    return Err(crate::error::SqueezefsError::Io(
                        std::io::Error::from_raw_os_error(libc::EEXIST),
                    ));
                }
                let (dest_v_idx, local_dest) = self.route_ino(dest_global);
                if dest_v_idx == old_parent_v_idx {
                    dest_local = Some(local_dest);
                } else {
                    self.dest_replace_routed(dest_v_idx, local_dest, guards.clone())
                        .await?;
                }
            }
            let out = be
                .routed_rename_local(
                    local_old_parent,
                    old_name,
                    local_new_parent,
                    new_name,
                    flags,
                    src_local,
                    dest_local,
                    guards.clone(),
                )
                .await;
            if out.is_err() {
                self.mirror_volume_failure(old_parent_v_idx);
            }
            out?;
            // Remote moved/exchanged inode ctime fragments.
            if let Some((v, l)) = src_remote {
                self.touch_ctime_routed(v, l, guards.clone()).await?;
            }
            if let Some((v, l)) = dest_remote {
                self.touch_ctime_routed(v, l, guards.clone()).await?;
            }
            Ok(())
        } else if flags & libc::RENAME_EXCHANGE != 0 {
            let (old_child, old_ft) = old_dentry_opt.ok_or_else(|| {
                crate::error::SqueezefsError::Io(std::io::Error::from_raw_os_error(libc::ENOENT))
            })?;
            let (new_child, new_ft) = new_dentry_opt.ok_or_else(|| {
                crate::error::SqueezefsError::Io(std::io::Error::from_raw_os_error(libc::ENOENT))
            })?;

            // Remove both, insert swapped — per-volume fragments (cross-
            // volume renames were never transactional across volumes).
            // PR M6 D4.b: each fragment carries its parent's time update;
            // the swapped inodes' ctimes follow as their own fragments.
            self.remove_dentry_routed(old_parent_v_idx, local_old_parent, old_name, guards.clone())
                .await?;
            self.remove_dentry_routed(new_parent_v_idx, local_new_parent, new_name, guards.clone())
                .await?;
            self.insert_dentry_routed(
                new_parent_v_idx,
                local_new_parent,
                old_child,
                new_name,
                old_ft,
                guards.clone(),
            )
            .await?;
            self.insert_dentry_routed(
                old_parent_v_idx,
                local_old_parent,
                new_child,
                old_name,
                new_ft,
                guards.clone(),
            )
            .await?;
            for child in [old_child, new_child] {
                let (v, l) = self.route_ino(child);
                self.touch_ctime_routed(v, l, guards.clone()).await?;
            }

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
                    // Directory move across parents: nlink shift on each
                    // side (best-effort).
                    self.parent_nlink_delta_routed(
                        old_parent_v_idx,
                        local_old_parent,
                        -1,
                        guards.clone(),
                    )
                    .await?;
                    self.parent_nlink_delta_routed(
                        new_parent_v_idx,
                        local_new_parent,
                        1,
                        guards.clone(),
                    )
                    .await?;
                }

                // Destination replacement: settle its inode, then remove
                // its dentry.
                if let Some((dest_ino, _dest_ft)) = new_dentry_opt {
                    let (dest_v_idx, local_dest) = self.route_ino(dest_ino);
                    self.dest_replace_routed(dest_v_idx, local_dest, guards.clone())
                        .await?;
                    self.remove_dentry_routed(
                        new_parent_v_idx,
                        local_new_parent,
                        new_name,
                        guards.clone(),
                    )
                    .await?;
                }

                self.remove_dentry_routed(
                    old_parent_v_idx,
                    local_old_parent,
                    old_name,
                    guards.clone(),
                )
                .await?;
                self.insert_dentry_routed(
                    new_parent_v_idx,
                    local_new_parent,
                    old_child,
                    new_name,
                    old_ft,
                    guards.clone(),
                )
                .await?;
                // PR M6 D4.b: the moved inode's ctime fragment.
                let (v, l) = self.route_ino(old_child);
                self.touch_ctime_routed(v, l, guards.clone()).await?;
                Ok(())
            } else {
                Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Source dentry not found",
                )))
            }
        }
    }

    async fn readdir(&self, dir: Ino, offset: u64, max: usize) -> Result<Vec<DirEntry>> {
        let (v_idx, local_dir) = self.route_ino(dir);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx].dlm().lock_inode_shared(local_dir).await;
        // `offset` is a readdir cookie; pages resume strictly after its
        // key suffix (design §5.1).
        self.volumes[v_idx].readdir(local_dir, offset, max).await
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
        let guards: std::sync::Arc<[dlm::DlmGuard]> = std::sync::Arc::from(vec![
            self.volumes[v_idx]
                .dlm()
                .lock_inode_exclusive(local_ino)
                .await,
        ]);
        let out = self.volumes[v_idx]
            .setattr_locked(local_ino, mode, uid, gid, size, atime, mtime, ctime, guards)
            .await;
        if out.is_err() {
            self.mirror_volume_failure(v_idx);
        }
        out.map(|mut i| {
            i.ino = ino;
            i
        })
    }

    async fn getxattr(&self, ino: Ino, name: &str) -> Result<Option<Vec<u8>>> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx].dlm().lock_inode_shared(local_ino).await;
        self.volumes[v_idx].getxattr(local_ino, name).await
    }

    async fn setxattr(&self, ino: Ino, name: &str, value: &[u8]) -> Result<()> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let guards: std::sync::Arc<[dlm::DlmGuard]> = std::sync::Arc::from(vec![
            self.volumes[v_idx]
                .dlm()
                .lock_inode_exclusive(local_ino)
                .await,
        ]);
        let out = self.volumes[v_idx]
            .setxattr_locked(local_ino, name, value, guards)
            .await;
        if out.is_err() {
            self.mirror_volume_failure(v_idx);
        }
        out
    }

    async fn removexattr(&self, ino: Ino, name: &str) -> Result<()> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let guards: std::sync::Arc<[dlm::DlmGuard]> = std::sync::Arc::from(vec![
            self.volumes[v_idx]
                .dlm()
                .lock_inode_exclusive(local_ino)
                .await,
        ]);
        let out = self.volumes[v_idx]
            .removexattr_locked(local_ino, name, guards)
            .await;
        if out.is_err() {
            self.mirror_volume_failure(v_idx);
        }
        out
    }

    async fn listxattr(&self, ino: Ino) -> Result<Vec<String>> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx].dlm().lock_inode_shared(local_ino).await;
        self.volumes[v_idx].listxattr(local_ino).await
    }

    async fn destroy_inode(&self, ino: Ino) -> Result<()> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        Metadata::destroy_inode(self.volumes[v_idx].as_ref(), local_ino).await
    }
}

impl RoutedMetaBackend {
    pub async fn sync_all_devices(&self) -> Result<()> {
        for vol in &self.volumes {
            // PR M6: parked times refinements ride this durability point
            // (fsyncdir/syncfs-class callers) — journal them BEFORE the
            // barrier so the barrier covers them.
            vol.drain_pending_times_now().await?;
            // The coalesced barrier ALSO drains the §4.6 pt 3
            // pending-reclaim bookkeeping.
            vol.sync_device().await?;
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
        // PR M6: fsync(ino) durability covers the ino's absorbed times
        // refinement — drain the volume's parked set (batched, usually
        // empty) ahead of the barrier.
        self.volumes[v_idx].drain_pending_times_now().await?;
        self.volumes[v_idx].sync_device().await
    }

    /// Persist layout xattr + size with fine locks. Used on fsync/release
    /// writeback; ONE two-record transaction (design §5.3, its own
    /// I-guard).
    pub async fn set_layout_and_size(&self, ino: Ino, layout: &[u8], size: u64) -> Result<()> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let out = self.volumes[v_idx]
            .set_layout_and_size(local_ino, layout, size)
            .await;
        if out.is_err() {
            self.mirror_volume_failure(v_idx);
        }
        out
    }
}

/// The mounted volume set's **filesystem generation identity** — what a
/// `squeezefs format` invocation changes and nothing else does. Local NVMe
/// staging is bound to this identity (`cache::nvme` stamps it into every
/// staging dir and discards staging content stamped by a DEAD generation
/// before any recovery/seeding runs — the reformat-over-stale-staging
/// poisoning fix).
///
/// Per-volume identity: the v3 superblock `uuid`
/// (`kv::superblock::SuperblockV3::uuid`) — random at every format
/// ([`kv::builder::BuilderConfig::new`]), read straight off sector 0
/// without mounting the volume.
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
            kv::superblock::VolumeFormat::V2Legacy => {
                return Err(v2_unsupported_error(path));
            }
            kv::superblock::VolumeFormat::V3(sb) => {
                let mut s = String::with_capacity(3 + 32);
                s.push_str("v3:");
                for b in sb.uuid {
                    let _ = write!(s, "{b:02x}");
                }
                s
            }
        };
        parts.push(part);
    }
    Ok(parts.join("|"))
}

#[cfg(test)]
mod knob_tests {
    use super::*;

    /// Knob resolution (design-wal-crash-consistency §4.2): canonical
    /// `SQUEEZEFS_META_FLUSH_INTERVAL_MS` wins over the legacy
    /// `SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS` alias; the alias alone still
    /// works; default is 50 ms. (Serial gate: env is process-global.)
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
