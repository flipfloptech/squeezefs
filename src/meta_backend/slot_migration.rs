//! The slot migration engine (PR VL5b — design-volume-lifecycle §5.5.2,
//! §5.5.2a, §5.5.2b, KD-7).
//!
//! One slot's whole lifecycle move, online, over a live
//! [`RoutedMetaBackend`]:
//!
//! 1. **Bulk copy** — paged latch-free range scans of the slot's
//!    keyspace on the source volume, translated into the target's guest
//!    ino-namespace partition and committed as ordinary conveyor
//!    transactions (no user-visible pause).
//! 2. **Delta capture** — the conveyor pass-task key tee
//!    ([`super::kv::backend::MigrationTee`]): keys only, values re-read
//!    at apply; bounded side log; overflow ⇒ fresh full snapshot pass;
//!    three consecutive overflows ⇒ loud abort.
//! 3. **Cutover** (§5.5.2a) — the per-slot gate closes ADMISSION (ops
//!    park holding nothing), in-flight guard-holders drain through the
//!    conveyor, the final delta rides the TARGET volume's conveyor as
//!    ordinary commit traffic (this task takes NO leaf locks — the 4b
//!    taker population stays closed), then the §5.5.2b flip. A window
//!    that cannot complete inside its deadline aborts-and-retries
//!    (gate reopened, delta rounds resume) — NEVER `disabled_volumes`.
//! 4. **The flip** (§5.5.2b) — target-first ordered durable writes:
//!    (0) final delta durable on the target, (1) target's stamp claims
//!    the slot @epoch E (+ the slot's travelling ino cursor), (2) the
//!    source's stamp releases @E, (3) the FormatConfig mirror. Crash
//!    windows resolve at discovery by per-slot highest-epoch-wins;
//!    re-running the migration converges (idempotent).
//! 5. **Teardown** — the source keyspace's records are deleted strictly
//!    after the flip (ordinary conveyor deletes; node frees ride the
//!    existing SMO pending-free protocol).
//!
//! The offline membership verbs (`volume add-meta` / `remove-meta`,
//! `src/config_ops.rs`) reuse the copy/scan/translate helpers here
//! against directly-opened backends.

use super::kv::backend::KvMetaBackend;
use super::kv::record::{xattr_name_hash56, XattrValue, HASH54_MAX, HASH56_MAX};
use super::{guest_local_ino, RoutedMetaBackend, GUEST_NS_BASE};
use crate::error::{Result, SqueezefsError};
use crate::fuse_client::METRICS;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Records per migration-apply transaction (bulk copy / delta / teardown
/// batches — well under the conveyor's byte caps for 64 KiB values).
const COPY_BATCH_RECORDS: usize = 128;

/// Engine phases the deterministic test hooks can hold (no sleeps — the
/// hold IS the scenario, the house test discipline).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationPhase {
    /// After a full snapshot (bulk-copy) pass, before its delta round —
    /// the tee window concurrent-mutation tests park here.
    AfterBulkCopy,
    /// Inside the cutover window, gate CLOSED, before the drain wait —
    /// the G-VL-4 cross-slot-rename hook parks here.
    GateClosed,
}

/// A deterministic engine hold: the engine parks at `phase` (every time,
/// or once) until the test releases it. Semaphore-paired so repeated
/// rounds hand-shake without sleeps.
pub struct PhaseHold {
    phase: MigrationPhase,
    once: bool,
    fired: AtomicBool,
    entered: squeezefs_ipc::sqz_semaphore::Semaphore,
    release: squeezefs_ipc::sqz_semaphore::Semaphore,
}

impl PhaseHold {
    /// Hold EVERY time the engine reaches `phase`.
    pub fn new(phase: MigrationPhase) -> Self {
        Self {
            phase,
            once: false,
            fired: AtomicBool::new(false),
            entered: squeezefs_ipc::sqz_semaphore::Semaphore::new(0),
            release: squeezefs_ipc::sqz_semaphore::Semaphore::new(0),
        }
    }

    /// Hold only the FIRST time.
    pub fn once(phase: MigrationPhase) -> Self {
        Self {
            once: true,
            ..Self::new(phase)
        }
    }

    /// Test side: await the engine parked at the phase.
    pub async fn entered(&self) {
        self.entered
            .acquire()
            .await
            .expect("hold semaphore never closes")
            .forget();
    }

    /// Test side: release one park.
    pub fn release(&self) {
        self.release.add_permits(1);
    }

    async fn engine_pause(&self, at: MigrationPhase) {
        if at != self.phase {
            return;
        }
        if self.once && self.fired.swap(true, Ordering::AcqRel) {
            return;
        }
        self.entered.add_permits(1);
        self.release
            .acquire()
            .await
            .expect("hold semaphore never closes")
            .forget();
    }
}

/// Test hooks (empty in production — [`Default`]).
#[derive(Default)]
pub struct MigrationTestHooks {
    pub hold: Option<Arc<PhaseHold>>,
}

impl MigrationTestHooks {
    async fn pause(&self, at: MigrationPhase) {
        if let Some(h) = &self.hold {
            h.engine_pause(at).await;
        }
    }
}

/// Engine knobs (§5.5.2 defaults).
#[derive(Clone, Debug)]
pub struct MigrationOptions {
    /// Side-log key cap (≈ 40 B/key; design default 1 M ≈ 40 MiB).
    pub delta_log_cap: usize,
    /// Delta size below which the cutover window opens (default 1000).
    pub cutover_threshold: usize,
    /// The §5.5.2a window deadline: a window that cannot drain + apply
    /// inside it aborts-and-retries (default 1 s).
    pub cutover_deadline: Duration,
    /// Consecutive window aborts before the migration fails loud.
    pub max_cutover_retries: u32,
    /// Test seam (§5.5.2b crash windows): abort the flip immediately
    /// AFTER durable write 0/1/2/3.
    pub crash_after_write: Option<u8>,
}

impl Default for MigrationOptions {
    fn default() -> Self {
        Self {
            delta_log_cap: 1_000_000,
            cutover_threshold: 1000,
            cutover_deadline: Duration::from_secs(1),
            max_cutover_retries: 16,
            crash_after_write: None,
        }
    }
}

/// What one migration did (report + engagement instrument).
#[derive(Debug, Clone, Default)]
pub struct MigrationReport {
    pub records_copied: u64,
    pub delta_rounds: u32,
    pub delta_keys: u64,
    pub overflows: u32,
    pub cutover_retries: u32,
    pub cutover_ms: u64,
}

/// A slot's keyspace on one volume: the effective-local ino range
/// `[lo, hi)` its records carry, plus the root ino control records live
/// at (`lo + 1` conceptually — raw local 1).
#[derive(Debug, Clone, Copy)]
pub struct SlotKeyspace {
    pub lo: u64,
    pub hi: u64,
    /// Whether this is the volume's LEGACY (un-namespaced) keyspace.
    pub legacy: bool,
}

impl SlotKeyspace {
    /// The keyspace of `slot` when hosted by a volume whose legacy slot
    /// is `legacy_slot`.
    pub fn of(slot: u16, legacy_slot: Option<u16>) -> Self {
        if legacy_slot == Some(slot) {
            Self {
                lo: 0,
                hi: GUEST_NS_BASE,
                legacy: true,
            }
        } else {
            let lo = guest_local_ino(slot, 0);
            Self {
                lo,
                hi: lo + GUEST_NS_BASE,
                legacy: false,
            }
        }
    }

    /// The keyspace's root ino (raw local 1) — where the excluded
    /// per-volume control records would sit.
    fn root_ino(&self) -> u64 {
        if self.legacy {
            1
        } else {
            self.lo | 1
        }
    }

    /// Translate an effective local ino out of this keyspace into `dst`.
    fn translate_ino(&self, ino: u64, dst: &SlotKeyspace) -> u64 {
        let raw = if self.legacy {
            ino
        } else {
            ino & (GUEST_NS_BASE - 1)
        };
        if dst.legacy {
            raw
        } else {
            dst.lo | raw
        }
    }

    /// Translate a record key (first 8 bytes = owning ino, all three
    /// trees) into `dst`.
    fn translate_key(&self, key: &[u8], dst: &SlotKeyspace) -> Vec<u8> {
        let mut out = key.to_vec();
        if let Some(b) = key.get(..8) {
            let ino = u64::from_be_bytes(b.try_into().expect("8 bytes"));
            out[..8].copy_from_slice(&self.translate_ino(ino, dst).to_be_bytes());
        }
        out
    }
}

/// The per-tree scan bounds of a keyspace (inclusive ends — the
/// `KvTree::range` contract).
fn tree_bounds(ks: &SlotKeyspace, tree_idx: usize) -> (Vec<u8>, Vec<u8>) {
    use super::kv::record::{dentry_key, inode_key, xattr_key};
    let lo = ks.lo.max(1);
    let hi = ks.hi - 1;
    match tree_idx {
        0 => (inode_key(lo).to_vec(), inode_key(hi).to_vec()),
        1 => (
            dentry_key(lo, 0, 0).to_vec(),
            dentry_key(hi, HASH54_MAX, u8::MAX).to_vec(),
        ),
        _ => (
            xattr_key(lo, 0, 0).to_vec(),
            xattr_key(hi, HASH56_MAX, u8::MAX).to_vec(),
        ),
    }
}

/// Whether `(tree_idx, key, value)` is a PER-VOLUME control record that
/// must never travel with a slot: the volume's own `writer_claim`, its
/// DLM-S2 `writer_term` era ladder, and — since DLM **S6** — its
/// `membership_owner` rendezvous record and the §6.2 item-7 `claim_set`,
/// all at its keyspace root. The S6 pair belongs to the VOLUME, not to
/// the routed set: the rendezvous record names the process serving THIS
/// volume's membership authority and both are read at LOCAL ino 1, so a
/// copy in another volume's guest keyspace would be unreadable residue
/// while the source's own record would have been carried away from the
/// only place anything looks for it.
/// `client:` heartbeat records DO travel — they are routed-set records
/// on global ino 1 and the registration scanners look for them in the
/// slot-0 keyspace.
fn is_pinned_control_record(ks: &SlotKeyspace, tree_idx: usize, key: &[u8], value: &[u8]) -> bool {
    if tree_idx != 2 {
        return false;
    }
    let Some(b) = key.get(..8) else {
        return false;
    };
    if u64::from_be_bytes(b.try_into().expect("8 bytes")) != ks.root_ino() {
        return false;
    }
    match XattrValue::decode(value) {
        Ok(x) => {
            x.name == crate::meta_backend::kv::backend::WRITER_CLAIM_XATTR.as_bytes()
                || x.name == crate::meta_backend::kv::backend::WRITER_TERM_XATTR.as_bytes()
                || x.name == crate::membership::MEMBERSHIP_OWNER_XATTR.as_bytes()
                || x.name == crate::membership::CLAIM_SET_XATTR.as_bytes()
        }
        Err(_) => false,
    }
}

/// Paged scan of one slot keyspace on `be`, invoking `sink(tree_idx,
/// key, value)` for every live record (pinned control records excluded).
pub async fn scan_slot_keyspace(
    be: &Arc<KvMetaBackend>,
    ks: &SlotKeyspace,
    mut sink: impl FnMut(usize, &[u8], &[u8]) -> Result<()>,
) -> Result<()> {
    for (tree_idx, tree) in be.trees().into_iter().enumerate() {
        let (start, end) = tree_bounds(ks, tree_idx);
        let mut cursor = start;
        loop {
            let page = tree.range(&cursor, &end, 512).await.map_err(|e| {
                SqueezefsError::InvalidOperation(format!("slot keyspace scan failed: {e}"))
            })?;
            let Some((last_key, _)) = page.last() else {
                break;
            };
            cursor = super::kv::node::key_successor(last_key);
            for (k, v) in &page {
                if is_pinned_control_record(ks, tree_idx, k, v) {
                    continue;
                }
                sink(tree_idx, k, v)?;
            }
        }
    }
    Ok(())
}

/// One full snapshot (bulk-copy) pass: source keyspace → target guest
/// keyspace, batched ordinary conveyor commits. Returns records copied.
pub async fn bulk_copy_slot(
    src: &Arc<KvMetaBackend>,
    src_ks: &SlotKeyspace,
    dst: &Arc<KvMetaBackend>,
    dst_ks: &SlotKeyspace,
) -> Result<u64> {
    let tree_ids: [u8; 3] = {
        let trees = src.trees();
        [trees[0].tree_id(), trees[1].tree_id(), trees[2].tree_id()]
    };
    let mut batch: Vec<(u8, Vec<u8>, Vec<u8>)> = Vec::with_capacity(COPY_BATCH_RECORDS);
    let mut copied = 0u64;
    // Collect pages first (scan sink is sync), flush batches after.
    let mut pending: Vec<(u8, Vec<u8>, Vec<u8>)> = Vec::new();
    scan_slot_keyspace(src, src_ks, |tree_idx, k, v| {
        pending.push((
            tree_ids[tree_idx],
            src_ks.translate_key(k, dst_ks),
            v.to_vec(),
        ));
        Ok(())
    })
    .await?;
    for rec in pending {
        batch.push(rec);
        copied += 1;
        if batch.len() >= COPY_BATCH_RECORDS {
            dst.migration_apply(std::mem::take(&mut batch), Vec::new())
                .await?;
        }
    }
    if !batch.is_empty() {
        dst.migration_apply(batch, Vec::new()).await?;
    }
    METRICS
        .meta_slot_records_copied
        .fetch_add(copied, Ordering::Relaxed);
    Ok(copied)
}

/// Delete every record of `ks` on `be` (batched) — the post-flip source
/// teardown and the pre-copy target wipe ("torn-down by re-run",
/// §5.5.2b write-0 note). Returns records deleted.
pub async fn teardown_slot_keyspace(be: &Arc<KvMetaBackend>, ks: &SlotKeyspace) -> Result<u64> {
    let tree_ids: [u8; 3] = {
        let trees = be.trees();
        [trees[0].tree_id(), trees[1].tree_id(), trees[2].tree_id()]
    };
    let mut keys: Vec<(u8, Vec<u8>)> = Vec::new();
    scan_slot_keyspace(be, ks, |tree_idx, k, _v| {
        keys.push((tree_ids[tree_idx], k.to_vec()));
        Ok(())
    })
    .await?;
    let deleted = keys.len() as u64;
    for chunk in keys.chunks(COPY_BATCH_RECORDS) {
        be.migration_apply(Vec::new(), chunk.to_vec()).await?;
    }
    Ok(deleted)
}

/// Apply one delta round: re-read every teed key's CURRENT value from
/// the source (per-key LWW — one re-read covers any number of commits)
/// and commit puts/deletes on the target as ordinary conveyor traffic.
async fn apply_delta(
    src: &Arc<KvMetaBackend>,
    src_ks: &SlotKeyspace,
    dst: &Arc<KvMetaBackend>,
    dst_ks: &SlotKeyspace,
    keys: &[(u8, Vec<u8>)],
) -> Result<()> {
    let mut puts: Vec<(u8, Vec<u8>, Vec<u8>)> = Vec::new();
    let mut dels: Vec<(u8, Vec<u8>)> = Vec::new();
    for (tree_id, key) in keys {
        let dst_key = src_ks.translate_key(key, dst_ks);
        match src.migration_read_record(*tree_id, key).await? {
            Some(v) => puts.push((*tree_id, dst_key, v)),
            None => dels.push((*tree_id, dst_key)),
        }
        if puts.len() + dels.len() >= COPY_BATCH_RECORDS {
            dst.migration_apply(std::mem::take(&mut puts), std::mem::take(&mut dels))
                .await?;
        }
    }
    if !puts.is_empty() || !dels.is_empty() {
        dst.migration_apply(puts, dels).await?;
    }
    Ok(())
}

/// Stable logical digest of one slot's records across the whole set:
/// key-normalized (raw-local form) so the digest is host-independent —
/// the G-VL-4 tree-diff-equivalence instrument.
pub async fn slot_logical_digest(routed: &Arc<RoutedMetaBackend>, slot: u16) -> Result<u64> {
    let map = routed.slot_map_snapshot();
    let v_idx = *map.get(usize::from(slot)).ok_or_else(|| {
        SqueezefsError::InvalidOperation(format!("slot {slot} outside the routing width"))
    })?;
    let ks = SlotKeyspace::of(slot, routed.legacy_slot_of(v_idx));
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    let norm = SlotKeyspace {
        lo: 0,
        hi: GUEST_NS_BASE,
        legacy: true,
    };
    scan_slot_keyspace(&routed.volumes[v_idx], &ks, |tree_idx, k, v| {
        h.update(&[tree_idx as u8]);
        let nk = ks.translate_key(k, &norm);
        h.update(&(nk.len() as u32).to_le_bytes());
        h.update(&nk);
        h.update(&(v.len() as u32).to_le_bytes());
        h.update(v);
        Ok(())
    })
    .await?;
    Ok(h.digest())
}

/// Whole-set logical digest: the ordered fold of every slot's digest.
pub async fn set_logical_digest(routed: &Arc<RoutedMetaBackend>) -> Result<u64> {
    let width = routed.routing_width().max(1);
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    for slot in 0..width {
        let d = slot_logical_digest(routed, slot as u16).await?;
        h.update(&d.to_le_bytes());
    }
    Ok(h.digest())
}

/// RAII cleanup for the armed gate + tee: every early-error path (and
/// the crash seams) disarms both — parked ops resume, the pass task
/// stops teeing. Success paths disarm explicitly and defuse this.
struct ArmedMigration<'a> {
    routed: &'a RoutedMetaBackend,
    src: &'a Arc<KvMetaBackend>,
    slot: u64,
    defused: bool,
}

impl Drop for ArmedMigration<'_> {
    fn drop(&mut self) {
        if self.defused {
            return;
        }
        self.src.disarm_migration_tee();
        self.routed.disarm_slot_gate(self.slot);
    }
}

/// Migrate `slot` to `target_idx` on a LIVE routed set (design §5.5.2).
/// Idempotent: re-running after any crash window converges (the §5.5.2b
/// table); a slot already hosted by the target completes the residual
/// protocol steps (source release, teardown, mirror).
pub async fn migrate_slot(
    routed: &Arc<RoutedMetaBackend>,
    slot: u16,
    target_idx: usize,
    opts: &MigrationOptions,
    hooks: &MigrationTestHooks,
) -> Result<MigrationReport> {
    let _one_at_a_time = routed.migration_lock.lock().await;
    let width = routed.routing_width();
    if u64::from(slot) >= width {
        return Err(SqueezefsError::InvalidOperation(format!(
            "slot {slot} outside the frozen routing width {width}"
        )));
    }
    if target_idx >= routed.volumes.len() {
        return Err(SqueezefsError::InvalidOperation(format!(
            "target volume index {target_idx} out of range"
        )));
    }
    if width <= 1 {
        return Err(SqueezefsError::InvalidOperation(
            "a W=1 filesystem has one slot — it can only be replaced wholesale \
             (volume add-meta/remove-meta), not spread (design-volume-lifecycle §5.5.1)"
                .to_string(),
        ));
    }
    if routed.legacy_slot_of(target_idx) == Some(slot) {
        return Err(SqueezefsError::InvalidOperation(format!(
            "migrating slot {slot} back to its origin volume is not supported in v1 \
             (its legacy keyspace may carry residue) — pick another target"
        )));
    }
    let map = routed.slot_map_snapshot();
    let src_idx = map[usize::from(slot)];
    // **Per-volume claim admission, sweep row 13** (D19 + KD-PV-6). Two
    // refusals, both load-bearing for §5.9.2's frozen-cross-owner-
    // reference law: a migration whose endpoints have different owners is
    // a CROSS-OWNER slot migration, which D19 defers to the named
    // follow-on (and which would move a child's inode record without its
    // parent's dentry — creating cross-owner names rather than removing
    // them); and slot 0 cannot move at all while the plane is armed,
    // because ino 1 pins to it and the owner of its volume IS the set
    // authority.
    if crate::meta_ship::owners::ownership_armed() {
        if slot == 0 {
            return Err(SqueezefsError::InvalidOperation(
                "refusing to migrate slot 0 while a multi-owner plane is armed: ino 1 pins to \
                 slot 0 (route_ino_width), and the owner of slot 0's volume IS the SET \
                 AUTHORITY — it assigns allocation lanes, serves the S9 custody endpoint, \
                 owns the only freed-offset grace ring and coordinates maintenance. Moving \
                 the slot would silently relocate all of that (KD-PV-6)"
                    .to_string(),
            ));
        }
        let src_owner = crate::meta_ship::owners::owner_of_volume(src_idx);
        let dst_owner = crate::meta_ship::owners::owner_of_volume(target_idx);
        let named = |o: &Option<std::sync::Arc<crate::meta_ship::PeerOwner>>| {
            o.as_ref()
                .map(|p| p.peer_id.clone())
                .unwrap_or_else(|| "this node".to_string())
        };
        if src_owner.as_ref().map(|p| p.peer_id.clone())
            != dst_owner.as_ref().map(|p| p.peer_id.clone())
        {
            return Err(SqueezefsError::InvalidOperation(format!(
                "refusing a cross-owner slot migration: slot {slot} homes on volume {src_idx} \
                 (owner {}) and the target volume {target_idx} is owned by {}. Moving a slot \
                 between OWNERS needs the two-party hand-off D19 defers to the named \
                 follow-on — and it would move inode records away from the dentries that name \
                 them, creating cross-owner names rather than removing them \
                 (design-per-volume-claim-admission §5.4a). Intra-owner migrations are \
                 unaffected",
                named(&src_owner),
                named(&dst_owner)
            )));
        }
    }
    let mut report = MigrationReport::default();
    if src_idx == target_idx {
        // Already flipped (a window-1/2/3 crash re-run): complete the
        // residual steps.
        finish_flip(routed, slot, target_idx).await?;
        METRICS.meta_slot_migrations.fetch_add(1, Ordering::Relaxed);
        return Ok(report);
    }
    // The source must keep a mint slot: a flip never leaves a member
    // hostless (that is remove-meta's job).
    if !map
        .iter()
        .enumerate()
        .any(|(s, &v)| v == src_idx && s != usize::from(slot))
    {
        return Err(SqueezefsError::InvalidOperation(format!(
            "migrating slot {slot} would leave its host volume {src_idx} hosting \
             nothing — use `squeezefs volume remove-meta` to retire a whole member"
        )));
    }

    let src = &routed.volumes[src_idx];
    let dst = &routed.volumes[target_idx];
    check_hash_seed_uniform(src, dst)?;
    let src_ks = SlotKeyspace::of(slot, routed.legacy_slot_of(src_idx));
    let dst_ks = SlotKeyspace::of(slot, routed.legacy_slot_of(target_idx));

    // Bit 4 durably on BOTH participants BEFORE any guest record /
    // extended stamp (the bit-before-first-extended-record invariant).
    for be in [src, dst] {
        super::kv::superblock::set_slot_migration_bit(be.device_path()).await?;
        be.sync_device().await?;
    }

    // Wipe any prior aborted attempt's target residue ("torn-down by
    // re-run" — §5.5.2b write-0 note), then arm gate + tee.
    teardown_slot_keyspace(dst, &dst_ks).await?;
    let gate = routed.arm_slot_gate(u64::from(slot));
    let tee = src.arm_migration_tee(src_ks.lo.max(1), src_ks.hi, opts.delta_log_cap);
    tee.exclude_writer_claim(
        src_ks.root_ino(),
        xattr_name_hash56(
            super::kv::backend::WRITER_CLAIM_XATTR.as_bytes(),
            src.superblock().hash_seed,
        ),
    );
    let mut armed = ArmedMigration {
        routed,
        src,
        slot: u64::from(slot),
        defused: false,
    };

    // ---- Phase A: snapshot passes + delta rounds (§5.5.2 steps 2–3).
    let mut consecutive_overflows = 0u32;
    'snapshot: loop {
        report.records_copied += bulk_copy_slot(src, &src_ks, dst, &dst_ks).await?;
        hooks.pause(MigrationPhase::AfterBulkCopy).await;
        let mut cutover_retries_this_round = 0u32;
        loop {
            let (keys, overflowed) = tee.drain_round();
            if overflowed {
                consecutive_overflows += 1;
                report.overflows += 1;
                if consecutive_overflows >= 3 {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "slot {slot} migration aborted: the delta side log overflowed \
                         {consecutive_overflows} consecutive rounds (cap {} keys) — the \
                         mutation rate outruns the copy; retry at a quieter time or \
                         raise the cap (§5.5.2 overflow rule)",
                        opts.delta_log_cap
                    )));
                }
                // Fresh full snapshot supersedes the lost keys.
                continue 'snapshot;
            }
            consecutive_overflows = 0;
            report.delta_rounds += 1;
            report.delta_keys += keys.len() as u64;
            METRICS
                .meta_slot_delta_keys
                .fetch_add(keys.len() as u64, Ordering::Relaxed);
            let below_threshold = keys.len() <= opts.cutover_threshold;
            apply_delta(src, &src_ks, dst, &dst_ks, &keys).await?;
            if !below_threshold {
                continue; // keep chasing the delta
            }

            // ---- Phase B: the cutover window (§5.5.2a).
            match cutover_window(
                routed,
                src,
                dst,
                &src_ks,
                &dst_ks,
                &gate,
                &tee,
                slot,
                target_idx,
                opts,
                hooks,
                &mut report,
            )
            .await?
            {
                CutoverOutcome::Flipped => break 'snapshot,
                CutoverOutcome::Retry => {
                    cutover_retries_this_round += 1;
                    report.cutover_retries += 1;
                    if cutover_retries_this_round >= opts.max_cutover_retries {
                        return Err(SqueezefsError::InvalidOperation(format!(
                            "slot {slot} migration aborted: {cutover_retries_this_round} \
                             consecutive cutover windows missed the {}ms deadline — \
                             abort-and-retry exhausted (§5.5.2a; never disabled_volumes)",
                            opts.cutover_deadline.as_millis()
                        )));
                    }
                    continue; // more delta rounds, then try again
                }
            }
        }
    }

    // ---- Phase C: post-flip — disarm, teardown, mirror.
    armed.defused = true;
    src.disarm_migration_tee();
    routed.disarm_slot_gate(u64::from(slot));
    teardown_slot_keyspace(src, &src_ks).await?;
    update_slot_map_mirror(routed).await;
    if let Some(3) = opts.crash_after_write {
        return Err(SqueezefsError::InvalidOperation(
            "crash injection: after flip write 3 (FormatConfig mirror)".to_string(),
        ));
    }
    METRICS.meta_slot_migrations.fetch_add(1, Ordering::Relaxed);
    Ok(report)
}

/// Record keys embed the §4.2 seeded dentry/xattr hashes: a slot can
/// only move between volumes sharing ONE hash seed (VL5b `--meta-slots`
/// formats derive it from the set uuid). Pre-VL5b stamped sets carry
/// per-volume seeds — refuse loud, never resolve-nothing silently.
pub fn check_hash_seed_uniform(src: &Arc<KvMetaBackend>, dst: &Arc<KvMetaBackend>) -> Result<()> {
    if src.superblock().hash_seed != dst.superblock().hash_seed {
        return Err(SqueezefsError::InvalidOperation(
            "slot migration refused: the source and target volumes carry DIFFERENT §4.2 \
             hash seeds (a pre-VL5b `--meta-slots` format) — migrated dentry/xattr keys \
             could never resolve on the target; reformat the set with this binary's \
             `format --meta-slots` (set-wide seed) to enable slot migration"
                .to_string(),
        ));
    }
    Ok(())
}

enum CutoverOutcome {
    Flipped,
    Retry,
}

/// One §5.5.2a cutover window: close → drain → final delta → §5.5.2b
/// flip → swap → reopen. A missed deadline reopens and reports `Retry`.
#[allow(clippy::too_many_arguments)]
async fn cutover_window(
    routed: &Arc<RoutedMetaBackend>,
    src: &Arc<KvMetaBackend>,
    dst: &Arc<KvMetaBackend>,
    src_ks: &SlotKeyspace,
    dst_ks: &SlotKeyspace,
    gate: &Arc<super::slot_gate_core::SlotGate>,
    tee: &Arc<super::kv::backend::MigrationTee>,
    slot: u16,
    target_idx: usize,
    opts: &MigrationOptions,
    hooks: &MigrationTestHooks,
    report: &mut MigrationReport,
) -> Result<CutoverOutcome> {
    let t0 = std::time::Instant::now();
    gate.close();
    let reopen_retry = |why: &str| {
        log::info!(
            "slot {slot} cutover window aborted ({why} after {} ms) — gate reopened, \
             delta rounds resume (§5.5.2a abort-and-retry; never disabled_volumes)",
            t0.elapsed().as_millis()
        );
        gate.reopen();
        routed.wake_slot_gate_waiters();
    };
    hooks.pause(MigrationPhase::GateClosed).await;

    // Drain the in-flight guard-holders (they complete through the
    // conveyor as normal — the cutover task holds nothing they need).
    loop {
        if gate.drained() {
            break;
        }
        if t0.elapsed() > opts.cutover_deadline {
            reopen_retry("in-flight ops did not drain");
            return Ok(CutoverOutcome::Retry);
        }
        squeezefs_ipc::sqz_blocking::yield_now().await;
    }
    // Absorbed times refinements ride ordinary commits — drain them so
    // the tee-empty recheck below can settle.
    src.drain_pending_times_now().await?;

    // The final delta — ordinary TARGET-conveyor commit traffic (this
    // task takes no leaf locks; the 4b taker population stays closed).
    let (keys, overflowed) = tee.drain_round();
    if overflowed {
        reopen_retry("side log overflowed inside the window");
        report.overflows += 1;
        return Ok(CutoverOutcome::Retry);
    }
    report.delta_keys += keys.len() as u64;
    METRICS
        .meta_slot_delta_keys
        .fetch_add(keys.len() as u64, Ordering::Relaxed);
    apply_delta(src, src_ks, dst, dst_ks, &keys).await?;
    // Recheck: nothing may have slipped onto the source since the drain
    // (a non-empty tee here = an ungated committer, e.g. a late times
    // refinement) — abort-and-retry, never flip over a hole.
    if tee.pending_len() != 0 || tee.has_overflowed() {
        reopen_retry("late commits landed after the final delta");
        return Ok(CutoverOutcome::Retry);
    }
    if t0.elapsed() > opts.cutover_deadline {
        reopen_retry("deadline exceeded before the flip");
        return Ok(CutoverOutcome::Retry);
    }

    // ---- §5.5.2b: the ordered flip. Past this point the window never
    // aborts — every crash prefix lands in the table and re-run
    // converges.
    // Write 0 (precondition): the final delta durable on the target.
    dst.checkpoint_now()
        .await
        .map_err(|e| SqueezefsError::InvalidOperation(format!("flip write 0 failed: {e}")))?;
    if let Some(0) = opts.crash_after_write {
        return Err(SqueezefsError::InvalidOperation(
            "crash injection: after flip write 0 (delta durable, nothing claimed)".to_string(),
        ));
    }

    // The travelling cursor: the slot's mint watermark on the source.
    let travelling_cursor = if src_ks.legacy {
        src.next_ino()
    } else {
        src.guest_cursor_snapshot(slot).unwrap_or(2)
    };

    let epoch = {
        let se = src.membership_stamp().map(|s| s.set_epoch).unwrap_or(1);
        let te = dst.membership_stamp().map(|s| s.set_epoch).unwrap_or(1);
        se.max(te) + 1
    };

    // Write 1: the TARGET's stamp claims the slot @E (+ cursor).
    {
        let mut st = dst.membership_stamp().ok_or_else(|| {
            SqueezefsError::InvalidOperation("target volume carries no membership stamp".into())
        })?;
        st.native_slot = st.resolved_native_slot();
        st.set_epoch = epoch;
        st.slots_hosted.insert(slot);
        st.slot_cursors.retain(|(s, _)| *s != slot);
        st.slot_cursors.push((slot, travelling_cursor));
        st.slot_cursors.sort_unstable_by_key(|(s, _)| *s);
        dst.install_guest_cursor(slot, travelling_cursor);
        dst.set_membership_stamp(st);
        dst.checkpoint_now()
            .await
            .map_err(|e| SqueezefsError::InvalidOperation(format!("flip write 1 failed: {e}")))?;
    }
    if let Some(1) = opts.crash_after_write {
        return Err(SqueezefsError::InvalidOperation(
            "crash injection: after flip write 1 (dual claim — highest epoch wins)".to_string(),
        ));
    }

    // Write 2: the SOURCE's stamp releases @E.
    {
        let mut st = src.membership_stamp().ok_or_else(|| {
            SqueezefsError::InvalidOperation("source volume carries no membership stamp".into())
        })?;
        st.native_slot = st.resolved_native_slot();
        st.set_epoch = epoch;
        st.slots_hosted.remove(slot);
        st.slot_cursors.retain(|(s, _)| *s != slot);
        src.remove_guest_cursor(slot);
        src.set_membership_stamp(st);
        src.checkpoint_now()
            .await
            .map_err(|e| SqueezefsError::InvalidOperation(format!("flip write 2 failed: {e}")))?;
    }
    if let Some(2) = opts.crash_after_write {
        return Err(SqueezefsError::InvalidOperation(
            "crash injection: after flip write 2 (new map fully expressed in stamps)".to_string(),
        ));
    }

    // The runtime swap: parked ops re-route through the new map when the
    // gate reopens (the slot_gate_core protocol guarantees no admitted
    // mutation straddles this store).
    let mut new_map = routed.slot_map_snapshot();
    new_map[usize::from(slot)] = target_idx;
    routed.publish_slot_map(new_map)?;
    gate.reopen();
    routed.wake_slot_gate_waiters();

    let window_ms = t0.elapsed().as_millis() as u64;
    report.cutover_ms = report.cutover_ms.max(window_ms);
    METRICS
        .meta_slot_cutover_ms_max
        .fetch_max(window_ms, Ordering::Relaxed);
    Ok(CutoverOutcome::Flipped)
}

/// Complete a flip the §5.5.2b resolution already decided (re-run after
/// a window-1/2/3 crash): release every stale claimer, tear down source
/// residue, rewrite the mirror.
async fn finish_flip(routed: &Arc<RoutedMetaBackend>, slot: u16, target_idx: usize) -> Result<()> {
    let target_epoch = routed.volumes[target_idx]
        .membership_stamp()
        .map(|s| s.set_epoch)
        .unwrap_or(1);
    for (v_idx, be) in routed.volumes.iter().enumerate() {
        if v_idx == target_idx {
            continue;
        }
        let Some(mut st) = be.membership_stamp() else {
            continue;
        };
        if st.slots_hosted.contains(slot) {
            // A stale claimer (window-1 residue): release at the
            // resolved epoch — the idempotent re-do of flip write 2.
            st.native_slot = st.resolved_native_slot();
            st.set_epoch = target_epoch;
            st.slots_hosted.remove(slot);
            st.slot_cursors.retain(|(s, _)| *s != slot);
            be.remove_guest_cursor(slot);
            be.set_membership_stamp(st);
            be.checkpoint_now().await.map_err(|e| {
                SqueezefsError::InvalidOperation(format!("finish-flip release failed: {e}"))
            })?;
        }
        // Source-residue teardown: any records still in this volume's
        // keyspace for the slot (its legacy keyspace, or a guest
        // partition from an older hosting).
        let ks = SlotKeyspace::of(slot, routed.legacy_slot_of(v_idx));
        teardown_slot_keyspace(be, &ks).await?;
    }
    update_slot_map_mirror(routed).await;
    Ok(())
}

/// Flip write 3: the informational FormatConfig mirror (best-effort —
/// the stamps are authoritative; a lost mirror is re-derived).
async fn update_slot_map_mirror(routed: &Arc<RoutedMetaBackend>) {
    use crate::meta_backend::Metadata;
    let map = routed.slot_map_snapshot();
    let Ok(Some(bytes)) = routed
        .getxattr(1, crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR)
        .await
    else {
        return;
    };
    let Ok(mut cfg) = serde_json::from_slice::<crate::FormatConfig>(&bytes) else {
        return;
    };
    // Mirror per-member hosted-slot runs (O(runs), never O(W) —
    // design-dynamic-meta-routing §5.6).
    let mut hosted: Vec<Vec<u16>> = vec![Vec::new(); routed.volumes.len()];
    for (slot, &v) in map.iter().enumerate() {
        hosted[v].push(slot as u16);
    }
    cfg.meta_slot_runs = Some(
        hosted
            .into_iter()
            .map(|slots| {
                crate::meta_backend::kv::slot_set::SlotSet::from_slots(&slots)
                    .runs()
                    .iter()
                    .map(|r| (r.start, r.stride, r.count))
                    .collect()
            })
            .collect(),
    );
    if let Ok(out) = serde_json::to_vec(&cfg) {
        if let Err(e) = routed
            .setxattr(
                1,
                crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR,
                &out,
            )
            .await
        {
            log::warn!("slot-map FormatConfig mirror update failed (informational): {e}");
        }
    }
}
