//! **Symmetric PR 6 — the backend's cross-owner arms**
//! (`docs/design-symmetric-metadata.md` §5.6, §5.6.4; KD-SYM-7/14):
//!
//! * the intent's SLOT homing — an intent rides step 0's region when
//!   step 0 is local (its own entry in the mount's rotor slot otherwise), so it keys on local 0 of that slot's namespace
//!   (`crossvol_tx::intent_ino_for_slot`) and the recovery scan reads
//!   every intent home a forest volume has;
//! * the intent-only `tx0` (every step of a plan foreign) and the
//!   idempotent retirement (a retired intent is retired again by nobody:
//!   the cadence's late pass meets the live path's own retirement);
//! * the live-refusal compensation of a minted child nothing names;
//! * the set-wide directory-rename lock — a tree-0 record on volume 0,
//!   taken/released by the manager's verbs, released for a dead holder by
//!   the recovery driver (PR 10's death ledger; the entry point exists
//!   here, the driver arrives with the ledger).
//!
//! A child module of `backend.rs` so the arms reach the same private
//! primitives the ONE applier uses without another line in the hot zone.

use super::*;
use crate::meta_backend::kv::slot_state::{DirRenameRecord, DIR_RENAME_KEY};

/// Wakes the in-process manager's parked lock takers when the record is
/// released (an unlock, a dead holder's release).
static DIR_RENAME_RELEASED: once_cell::sync::Lazy<squeezefs_ipc::sqz_notify::Notify> =
    once_cell::sync::Lazy::new(squeezefs_ipc::sqz_notify::Notify::new);

/// The in-process SERIALIZATION of one identity's directory renames, per
/// `(volume uuid, lock identity)`: a one-permit semaphore each op holds
/// with its lease (review round 2, Issues 23/24 — the lease serializes
/// OPS: the second directory rename of one identity WAITS for the first's
/// release rather than joining its record, so the law never leans on the
/// kernel's `s_vfs_rename_mutex`, which an S8-served `Rename` or an
/// offline `RoutedMetaBackend` caller never passes through). Keyed by the
/// volume's durable uuid, never an address; the permit is RAII on the
/// lease and drops in the same scope as the record's unlock.
static DIR_RENAME_LOCAL_PERMITS: once_cell::sync::Lazy<
    parking_lot::Mutex<
        std::collections::HashMap<(u128, u32), Arc<squeezefs_ipc::sqz_semaphore::Semaphore>>,
    >,
> = once_cell::sync::Lazy::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// The one-permit semaphore of `(volume, identity)` (created on first use).
fn dir_rename_permit_of(
    volume: u128,
    identity: u32,
) -> Arc<squeezefs_ipc::sqz_semaphore::Semaphore> {
    Arc::clone(
        DIR_RENAME_LOCAL_PERMITS
            .lock()
            .entry((volume, identity))
            .or_insert_with(|| Arc::new(squeezefs_ipc::sqz_semaphore::Semaphore::new(1))),
    )
}

/// The verdict of a `DirRenameLock`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirRenameOutcome {
    /// Held by the caller (`already` = it was — KD-SYM-7).
    Locked { already: bool },
    /// Another appender holds it: the caller waits, never a refusal.
    Busy { holder: u32 },
}

/// The pure screen of the two lock verbs' wire words (review round 1,
/// Issue 3 — PR 4 round 6's law for every slot verb; fuzzed by
/// `manager_call_frame`): the frame must address VOLUME 0 (the set-wide
/// lease has one home), the id must name a `Live` directory page and
/// never one of this mount's own regions (a wire peer is not this
/// process), and an unlock must name the HOLDER while the lock is held
/// (`holder` = the record's, `None` = free) — a non-holder's release of
/// another's lease is the foreign release KD-SYM-14 exists to prevent.
/// `Err` names the refusal; the caller counts it on `manager_verb_rejected`.
pub fn screen_dir_rename_words(
    volume_ordinal: Option<u16>,
    appender_id: u32,
    is_own_region: bool,
    has_live_page: bool,
    unlock_of: Option<Option<u32>>,
) -> std::result::Result<(), String> {
    if volume_ordinal != Some(0) {
        return Err(format!(
            "the set-wide directory-rename lock is volume 0's manager's; this frame names {}",
            volume_ordinal.map_or("no volume".to_string(), |v| format!("volume {v}"))
        ));
    }
    if is_own_region {
        return Err(format!(
            "appender {appender_id} is one of this mount's own regions — a wire peer never is"
        ));
    }
    if !has_live_page {
        return Err(format!(
            "appender {appender_id} holds no Live directory page"
        ));
    }
    if let Some(Some(holder)) = unlock_of {
        if holder != appender_id {
            return Err(format!(
                "appender {appender_id} is releasing a lease appender {holder} holds — a foreign \
                 release"
            ));
        }
    }
    Ok(())
}

/// The in-process manager's held lock: the record's lease AND this
/// identity's one permit (the op-serialization above). Released by
/// [`Self::release`] — the awaited fast path — or, for a lease whose
/// future is dropped (an unwound handler task, a cancellation), by
/// `Drop`, which spawns the same release so a live process never leaks
/// the set's lock (review round 1, Issue 6; the `DlmGuard::external`
/// pattern); the permit travels into that release and drops with it. A
/// holder that DIES is the recovery driver's.
pub struct DirRenameLease {
    volume: Arc<KvMetaBackend>,
    appender_id: u32,
    permit: Option<squeezefs_ipc::sqz_semaphore::OwnedSemaphorePermit>,
    released: bool,
}

impl DirRenameLease {
    /// Release the record, then the permit (idempotent against the
    /// record).
    pub async fn release(mut self) -> std::result::Result<(), KvError> {
        self.released = true;
        let permit = self.permit.take();
        let out = self.volume.dir_rename_local_release(self.appender_id).await;
        drop(permit);
        out
    }
}

impl Drop for DirRenameLease {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let volume = Arc::clone(&self.volume);
        let appender_id = self.appender_id;
        let permit = self.permit.take();
        crate::meta_exec::spawn_meta("dir_rename_lease_drop", async move {
            if let Err(e) = volume.dir_rename_local_release(appender_id).await {
                log::error!(
                    "meta volume {}: releasing a dropped directory-rename lease failed ({e}) — \
                     the record stays until the recovery driver releases it",
                    volume.path.display()
                );
            }
            drop(permit);
        });
    }
}

impl KvMetaBackend {
    /// The appender id this mount's OWN identity holds on this volume —
    /// region 0's (the manager's, KD-SYM-3; a joined non-manager
    /// appender's own region is PR 12's). A declared region's slot is
    /// another appender's for every cross-owner decision, which is what
    /// makes the seam PR 2–4 built a two-holder model in one process.
    pub fn own_appender_id(&self) -> u32 {
        self.appenders
            .as_ref()
            .and_then(|s| s.regions.first())
            .map_or(0, |r| r.id)
    }

    /// Is appender `id` one of THIS mount's regions — i.e. does its 4a
    /// lock table live in this process (the one-canonical-`lock_many`
    /// table of `crossvol_tx::acquire_guards_leased`)? Region 0 and the
    /// declared regions; a wire joiner's is another process's.
    pub fn is_own_region(&self, id: u32) -> bool {
        self.appenders
            .as_ref()
            .is_some_and(|s| s.regions.iter().any(|r| r.id == id))
    }

    /// The intent-only `tx0`: a plan whose first step is another
    /// appender's writes its intent alone into this initiator's ring
    /// before anything ships.
    pub async fn xv_write_intent(&self, put: &XvRider, guards: Arc<[DlmGuard]>) -> Result<()> {
        self.write_gate()?;
        let mut tx = KvTx::new();
        Self::stage_intent_rider(&mut tx, Some(put))?;
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// Retire the intent homed on `intent_ino`: a `Delete` of an EXACT
    /// key. Idempotent — an intent already gone (the live path's own
    /// retirement, met by the cadence's later pass) writes nothing.
    pub async fn xv_retire_intent_at(
        &self,
        intent_ino: Ino,
        tx_id: u64,
        guards: Arc<[DlmGuard]>,
    ) -> Result<()> {
        self.write_gate()?;
        let key = crossvol_tx::intent_key_at(intent_ino, tx_id);
        if self.lookup_kind(TREE_XATTRS, &key).await?.is_none() {
            return Ok(());
        }
        let mut tx = KvTx::new();
        Self::stage_intent_rider(&mut tx, Some(&XvRider::Delete { intent_ino, tx_id }))?;
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// Re-read one intent's record image (`None` = retired) — the
    /// roll-forward's read UNDER the guards it acquired (review round 1,
    /// Issue 4): a plan is applied from the record as it stands then,
    /// never from a scan an intervening retirement may have outdated.
    pub async fn xv_read_intent(&self, intent_ino: Ino, tx_id: u64) -> Result<Option<Vec<u8>>> {
        let key = crossvol_tx::intent_key_at(intent_ino, tx_id);
        match self.lookup_kind(TREE_XATTRS, &key).await? {
            Some(v) => Ok(Some(XattrValue::decode(&v)?.value)),
            None => Ok(None),
        }
    }

    /// Destroy a freshly minted inode record nothing names (the live-
    /// refusal compensation of a `Create` whose insert the holder
    /// refused): the record alone — a fresh mint has no xattr, no layout,
    /// no block.
    /// `rider` = the intent's own retirement riding the SAME entry (the
    /// compensation's last act — review round 2, Issue 26).
    pub async fn xv_destroy_unnamed(
        &self,
        local_ino: Ino,
        rider: Option<&XvRider>,
        guards: Arc<[DlmGuard]>,
    ) -> Result<()> {
        self.write_gate()?;
        let mut tx = KvTx::new();
        Self::stage_intent_rider(&mut tx, rider)?;
        if self.read_inode_value(local_ino).await?.is_some() {
            tx.stage_delete(TREE_INODES, inode_key(local_ino));
        } else if rider.is_none() {
            return Ok(());
        }
        tx.hold_guards(guards);
        self.commit_tx(tx).await?;
        Ok(())
    }

    /// [`Self::find_parent_of_child`] with the dentry's NAME — the
    /// directory-rename ancestor check confirms each link `(parent, name)
    /// → child` at the parent's slot holder (design §5.6.4). The same
    /// full dentries walk, the same instrument (`meta_parent_scans`).
    pub async fn find_parent_link_of_child(
        &self,
        child_global: Ino,
    ) -> Result<Option<(Ino, String)>> {
        crate::fuse_client::METRICS
            .meta_parent_scans
            .fetch_add(1, Ordering::Relaxed);
        let end = super::super::tree::KEY_SPACE_MAX;
        let mut cursor: Vec<u8> = vec![0u8];
        loop {
            let page = self
                .range_kind(TREE_DENTRIES, &cursor, &end, SCAN_PAGE)
                .await?;
            let Some((last_key, _)) = page.last() else {
                return Ok(None);
            };
            cursor = key_successor(last_key);
            for (k, v) in &page {
                let d = DentryValue::decode(v)?;
                if d.child_ino == child_global {
                    let (parent, _hash54, _coll) = decode_dentry_key(k)?;
                    return Ok(Some((
                        parent,
                        String::from_utf8_lossy(&d.name).into_owned(),
                    )));
                }
            }
        }
    }

    /// Every intent home this volume has: ino 0 (the flat and native
    /// home), plus local 0 of each slot tree's namespace on a forest.
    fn xv_intent_homes(&self) -> Vec<Ino> {
        let mut homes = vec![crossvol_tx::XV_INTENT_INO];
        if let Some(forest) = self.forest() {
            for (slot, _) in forest.slot_trees() {
                if slot != super::super::record::NATIVE_FOREST_SLOT {
                    homes.push(crossvol_tx::intent_ino_for_slot(slot));
                }
            }
        }
        homes
    }

    /// This volume's OPEN cross-volume intents as `(intent home ino,
    /// tx_id, image)` over every home — one bounded range scan per home,
    /// empty on a healthy volume. The recovery driver's input.
    pub async fn xv_scan_intents_homed(&self) -> Result<Vec<(Ino, u64, Vec<u8>)>> {
        let mut out = Vec::new();
        for home in self.xv_intent_homes() {
            let end = xattr_key(home, HASH56_MAX, u8::MAX);
            let mut cursor: Vec<u8> = xattr_key(home, 0, 0).to_vec();
            loop {
                let page = self
                    .range_kind(TREE_XATTRS, &cursor, &end, SCAN_PAGE)
                    .await?;
                let Some((last_key, _)) = page.last() else {
                    break;
                };
                cursor = key_successor(last_key);
                for (k, v) in &page {
                    let (_ino, hash, coll) = decode_xattr_key(k)?;
                    let tx_id = (u64::from(coll) << 56) | hash;
                    out.push((home, tx_id, XattrValue::decode(v)?.value));
                }
            }
        }
        Ok(out)
    }

    // -----------------------------------------------------------------
    // The set-wide directory-rename lock (§5.6.4, KD-SYM-14) — tree 0 of
    // volume 0; the manager's verbs.
    // -----------------------------------------------------------------

    /// The lock record as tree 0 holds it (`None` = free).
    pub async fn dir_rename_record(&self) -> std::result::Result<Option<DirRenameRecord>, KvError> {
        let Some(control) = self.forest_control_tree() else {
            return Ok(None);
        };
        match control.lookup(DIR_RENAME_KEY).await? {
            Some(v) => Ok(Some(DirRenameRecord::decode(&v)?)),
            None => Ok(None),
        }
    }

    /// `DirRenameLock` for `appender_id` at the serving manager's era
    /// `term` (the record's provenance word): ONE control entry writing
    /// the record when free; `already` when the caller holds it; `Busy`
    /// naming the holder otherwise (the caller waits — a set renames one
    /// directory at a time by design). Caller holds `manager_verbs`.
    async fn dir_rename_lock_locked(
        &self,
        set: &super::super::appender::AppenderSet,
        appender_id: u32,
        term: u64,
    ) -> std::result::Result<DirRenameOutcome, KvError> {
        if let Some(rec) = self.dir_rename_record().await? {
            if rec.holder == appender_id {
                set.verbs.replays.fetch_add(1, Ordering::Relaxed);
                return Ok(DirRenameOutcome::Locked { already: true });
            }
            return Ok(DirRenameOutcome::Busy { holder: rec.holder });
        }
        let since_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        let rec = DirRenameRecord {
            holder: appender_id,
            term,
            since_ns,
        };
        let tag = super::super::journal::tag_for(super::super::record::TREE_CONTROL, 0);
        self.write_control_entry(
            vec![(tag, Record::put(DIR_RENAME_KEY.to_vec(), 0, rec.encode()))],
            EntryAdmission::Try,
        )
        .await?;
        Ok(DirRenameOutcome::Locked { already: false })
    }

    /// `DirRenameUnlock` for `appender_id`: deletes the record it holds
    /// (`Ok(true)` = it was already free — a replay). Caller holds
    /// `manager_verbs`; the wire arm screens a non-holder first.
    async fn dir_rename_unlock_locked(
        &self,
        set: &super::super::appender::AppenderSet,
        appender_id: u32,
    ) -> std::result::Result<bool, KvError> {
        match self.dir_rename_record().await? {
            Some(rec) if rec.holder == appender_id => {
                let tag = super::super::journal::tag_for(super::super::record::TREE_CONTROL, 0);
                self.write_control_entry(
                    vec![(tag, Record::delete(DIR_RENAME_KEY.to_vec(), 0))],
                    EntryAdmission::Try,
                )
                .await?;
                DIR_RENAME_RELEASED.notify_waiters();
                Ok(false)
            }
            _ => {
                set.verbs.replays.fetch_add(1, Ordering::Relaxed);
                Ok(true)
            }
        }
    }

    /// The WIRE `DirRenameLock` (review round 1, Issue 3): the words
    /// screened before any effect — [`screen_dir_rename_words`] against
    /// the frame's volume ordinal, this mount's regions and the appender
    /// directory (`Rejected` + `manager_verb_rejected` on failure, nothing
    /// written) — then the take at the serving manager's era.
    pub async fn manager_dir_rename_lock_wire(
        &self,
        volume_ordinal: Option<u16>,
        appender_id: u32,
    ) -> std::result::Result<DirRenameOutcome, KvError> {
        let set = self.manager_gate(false)?;
        self.screen_dir_rename_verb(set, volume_ordinal, appender_id, "DirRenameLock", None)
            .await?;
        let _g = self.manager_verbs.lock().await;
        self.dir_rename_lock_locked(set, appender_id, self.writer_term())
            .await
    }

    /// The WIRE `DirRenameUnlock`: screened like the lock, plus the
    /// holder check — an unlock naming an id that is not the holder while
    /// the lock is held is a FOREIGN release, rejected; naming an id while
    /// the lock is free is the idempotent replay (`already`).
    pub async fn manager_dir_rename_unlock_wire(
        &self,
        volume_ordinal: Option<u16>,
        appender_id: u32,
    ) -> std::result::Result<bool, KvError> {
        let set = self.manager_gate(true)?;
        let _g = self.manager_verbs.lock().await;
        let holder = self.dir_rename_record().await?.map(|r| r.holder);
        self.screen_dir_rename_verb(
            set,
            volume_ordinal,
            appender_id,
            "DirRenameUnlock",
            Some(holder),
        )
        .await?;
        self.dir_rename_unlock_locked(set, appender_id).await
    }

    /// Run [`screen_dir_rename_words`] with this volume's facts, counting
    /// a refusal on `manager_verb_rejected`.
    async fn screen_dir_rename_verb(
        &self,
        set: &super::super::appender::AppenderSet,
        volume_ordinal: Option<u16>,
        appender_id: u32,
        verb: &str,
        unlock_of: Option<Option<u32>>,
    ) -> std::result::Result<(), KvError> {
        let is_own_region = set.region(appender_id).is_some();
        let has_live_page = if volume_ordinal == Some(0) && !is_own_region {
            let entries = super::super::appender::read_directory(&self.path, &self.sb).await?;
            entries.into_iter().any(|e| {
                e.page.is_some_and(|p| {
                    p.appender_id == appender_id
                        && p.state == super::super::appender::AppenderState::Live
                })
            })
        } else {
            false
        };
        screen_dir_rename_words(
            volume_ordinal,
            appender_id,
            is_own_region,
            has_live_page,
            unlock_of,
        )
        .map_err(|reason| {
            set.verbs.rejected.fetch_add(1, Ordering::Relaxed);
            KvError::Rejected(format!(
                "{}: {verb} from the wire — {reason} (manager_verb_rejected)",
                self.path.display()
            ))
        })
    }

    /// The RECOVERY DRIVER's release of a DEAD holder's lock (design
    /// §5.6.4 / §5.9 — the expiry law: the lock dies with the initiator's
    /// membership lease, and PR 10's death ledger is what proves the
    /// death; this is the entry it calls). `Ok(true)` = released; `false`
    /// = `appender_id` held nothing.
    pub async fn manager_dir_rename_release_dead(
        &self,
        appender_id: u32,
    ) -> std::result::Result<bool, KvError> {
        let set = self.manager_gate(true)?;
        let _g = self.manager_verbs.lock().await;
        let held = self.dir_rename_record().await?;
        let released = !self.dir_rename_unlock_locked(set, appender_id).await?;
        if released {
            log::warn!(
                "meta volume {}: the set-wide directory-rename lock held by dead appender \
                 {appender_id} (granted at manager era {}) was released by recovery",
                self.path.display(),
                held.map_or(0, |r| r.term)
            );
        }
        Ok(released)
    }

    /// Release this identity's record (the permit drops in the caller's
    /// scope, after).
    async fn dir_rename_local_release(
        self: &Arc<Self>,
        appender_id: u32,
    ) -> std::result::Result<(), KvError> {
        let set = self.manager_gate(true)?;
        let _g = self.manager_verbs.lock().await;
        self.dir_rename_unlock_locked(set, appender_id).await?;
        Ok(())
    }

    /// The in-process manager's take of the lock for its own directory
    /// rename: this identity's one PERMIT first (a second directory rename
    /// of the same identity waits here — ops serialize, never join), then
    /// the record, looping on `dir_rename_lock_locked` and parking on the
    /// release wake while another identity holds it. Volume 0's manager
    /// is this process on every set this codebase mounts (the wire
    /// initiator's take is PR 12's join ladder over `ManagerClient::
    /// dir_rename_lock`).
    pub async fn dir_rename_lock_held(
        self: &Arc<Self>,
        appender_id: u32,
    ) -> std::result::Result<DirRenameLease, KvError> {
        let permit = dir_rename_permit_of(self.volume_uuid(), appender_id)
            .acquire_owned()
            .await
            .map_err(|e| {
                KvError::Corrupt(format!(
                    "{}: the directory-rename permit of appender {appender_id} is closed ({e:?})",
                    self.path.display()
                ))
            })?;
        loop {
            let released = DIR_RENAME_RELEASED.notified();
            let set = self.manager_gate(false)?;
            let outcome = {
                let _g = self.manager_verbs.lock().await;
                self.dir_rename_lock_locked(set, appender_id, self.writer_term())
                    .await?
            };
            match outcome {
                DirRenameOutcome::Locked { .. } => {
                    return Ok(DirRenameLease {
                        volume: Arc::clone(self),
                        appender_id,
                        permit: Some(permit),
                        released: false,
                    })
                }
                DirRenameOutcome::Busy { .. } => {
                    // A wire holder's release is a control entry this
                    // process writes too, so the wake is exact; the bound
                    // is the belt for a holder released by recovery on
                    // another manager (PR 10).
                    let _ = squeezefs_ipc::sqz_time::timeout(
                        std::time::Duration::from_millis(50),
                        released,
                    )
                    .await;
                }
            }
        }
    }
}
