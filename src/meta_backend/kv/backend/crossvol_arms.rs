//! **Symmetric PR 6 — the backend's cross-owner arms**
//! (`docs/design-symmetric-metadata.md` §5.6, §5.6.4; KD-SYM-7/14):
//!
//! * the intent's SLOT homing — an intent rides the first local step's
//!   region, so it keys on local 0 of that slot's namespace
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

/// The verdict of a `DirRenameLock`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirRenameOutcome {
    /// Held by the caller (`already` = it was — KD-SYM-7).
    Locked { already: bool },
    /// Another appender holds it: the caller waits, never a refusal.
    Busy { holder: u32 },
}

/// The in-process manager's held lock: released by [`Self::release`] (an
/// explicit act — the release is a tree-0 write, which `Drop` cannot
/// await; a lease whose holder dies is the recovery driver's).
pub struct DirRenameLease {
    volume: Arc<KvMetaBackend>,
    appender_id: u32,
}

impl DirRenameLease {
    /// Release the lock (idempotent against the record).
    pub async fn release(self) -> std::result::Result<(), KvError> {
        self.volume
            .manager_dir_rename_unlock(self.appender_id)
            .await
            .map(|_| ())
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

    /// Destroy a freshly minted inode record nothing names (the live-
    /// refusal compensation of a `Create` whose insert the holder
    /// refused): the record alone — a fresh mint has no xattr, no layout,
    /// no block.
    pub async fn xv_destroy_unnamed(&self, local_ino: Ino, guards: Arc<[DlmGuard]>) -> Result<()> {
        self.write_gate()?;
        if self.read_inode_value(local_ino).await?.is_none() {
            return Ok(());
        }
        let mut tx = KvTx::new();
        tx.stage_delete(TREE_INODES, inode_key(local_ino));
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

    /// `DirRenameLock` for `appender_id` at writer term `term`: ONE
    /// control entry writing the record when free; `already` when the
    /// caller holds it; `Busy` naming the holder otherwise (the caller
    /// waits — a set renames one directory at a time by design).
    pub async fn manager_dir_rename_lock(
        &self,
        appender_id: u32,
        term: u64,
    ) -> std::result::Result<DirRenameOutcome, KvError> {
        let set = self.manager_gate(false)?;
        let _g = self.manager_verbs.lock().await;
        if let Some(rec) = self.dir_rename_record().await? {
            if rec.holder == appender_id {
                set.verbs.replays.fetch_add(1, Ordering::Relaxed);
                return Ok(DirRenameOutcome::Locked { already: true });
            }
            return Ok(DirRenameOutcome::Busy { holder: rec.holder });
        }
        let rec = DirRenameRecord {
            holder: appender_id,
            term,
            since_ns: crate::mono_core::monotonic_ns_u64(),
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
    /// (`Ok(true)` = it was already free, or another's — a replay).
    pub async fn manager_dir_rename_unlock(
        &self,
        appender_id: u32,
    ) -> std::result::Result<bool, KvError> {
        let set = self.manager_gate(true)?;
        let _g = self.manager_verbs.lock().await;
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

    /// The RECOVERY DRIVER's release of a DEAD holder's lock (design
    /// §5.6.4 / §5.9 — the expiry law: the lock dies with the initiator's
    /// membership lease, and PR 10's death ledger is what proves the
    /// death; this is the entry it calls). `Ok(true)` = released; `false`
    /// = `appender_id` held nothing.
    pub async fn manager_dir_rename_release_dead(
        &self,
        appender_id: u32,
    ) -> std::result::Result<bool, KvError> {
        let released = !self.manager_dir_rename_unlock(appender_id).await?;
        if released {
            log::warn!(
                "meta volume {}: the set-wide directory-rename lock held by dead appender \
                 {appender_id} was released by recovery",
                self.path.display()
            );
        }
        Ok(released)
    }

    /// The in-process manager's take of the lock for its own directory
    /// rename: loops on [`Self::manager_dir_rename_lock`], parking on the
    /// release wake while another appender holds it. Volume 0's manager
    /// is this process on every set this codebase mounts (the wire
    /// initiator's take is PR 12's join ladder over `ManagerClient::
    /// dir_rename_lock`).
    pub async fn dir_rename_lock_held(
        self: &Arc<Self>,
        appender_id: u32,
    ) -> std::result::Result<DirRenameLease, KvError> {
        loop {
            let released = DIR_RENAME_RELEASED.notified();
            match self
                .manager_dir_rename_lock(appender_id, self.writer_term())
                .await?
            {
                DirRenameOutcome::Locked { .. } => {
                    return Ok(DirRenameLease {
                        volume: Arc::clone(self),
                        appender_id,
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
