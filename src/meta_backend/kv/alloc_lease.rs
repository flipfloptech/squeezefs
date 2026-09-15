//! **The allocation lease, the death ledger's RECORDS and the coordinator
//! predicate** — tree 0 of VOLUME 0's set-wide records and their
//! executors (docs/design-symmetric-metadata.md §5.4.2, §5.5, §5.5.1,
//! §5.5.2, §5.9; KD-SYM-9/15; PR 8).
//!
//! Three record families live in tree 0 of the set's volume 0 (the
//! volume hosting slot 0 — its manager is the set-wide bound, KD-SYM-2):
//!
//! * `alloc_lease:{vol_tag} → { holder, holder_appender_id, home_vol,
//!   control_ino, blocks, term, bitmap: [(home_vol, ExtentRef)] }` — which
//!   mount holds DATA volume `vol_tag`'s floating allocation lease, and
//!   where its [`crate::data_alloc_bitmap`] pages live (extents of the
//!   holder's own grant on its HOME metadata volume, volume-qualified);
//! * `dead_member:{node_token, mount_slot} → { epoch, ts }` — the death
//!   ledger (§5.5.2): written by volume 0's manager on `RecordDeath`.
//!   **PR 8 builds the RECORD and its in-process writer
//!   ([`KvMetaBackend::record_death`]); PR 10's recovery driver is the
//!   production writer** — the wire verb refuses naming it;
//! * `recovered:{node_token, mount_slot, vol} → { by_term, ts }` — written
//!   by the manager that recovered the dead member's region on volume
//!   `vol` ([`KvMetaBackend::manager_record_recovered`], idempotent).
//!
//! **The ordering law** ([`KvMetaBackend::manager_alloc_lease_acquire`]):
//! volume 0's manager re-grants `alloc_lease:{vol}` to a successor ONLY
//! after `recovered:{dead holder, its home_vol}` is present — the death
//! ledger already orders the recovery (the home manager's ring replay
//! applies the kind-4 bitmap deltas to the pages, THEN writes
//! `recovered:`), so a successor never reads pages that show a journaled
//! grant as free. A live holder's lease is refused to everyone else; a
//! dead holder's without the record is DEFERRED (the requester retries).
//! The successor then COPIES the recovered pages into extents of its own
//! grant on ITS home volume and publishes the new refs
//! ([`KvMetaBackend::manager_alloc_lease_bitmap`], barriered by volume
//! 0's lane); the dead region stays `Recovered` until `alloc_lease:` no
//! longer names its extents (PR 10's release reads the record).
//!
//! The HOLDER half ([`AllocHolding`], [`KvMetaBackend::hold_alloc_lease`],
//! `holder_block_grant` / `holder_return_blocks` / the `finish_free`
//! clears / the checkpoint's page write) journals every bitmap delta in
//! the holder's OWN ring ahead of the reply and writes the pages at its
//! checkpoint — the meta-bitmap law on one device.
//!
//! The **coordinator predicate** ([`symmetric_coordinator_refusal`]) and
//! the **lessee shards** ([`KvMetaBackend::inode_plane_owns_slot`],
//! [`KvMetaBackend::inode_plane_slot_coverage`]) are the maintenance
//! plane's half: under the armed plane the coordinator is volume 0's
//! manager (KD-PV-14's owner ⇒ the manager lease), and each mount
//! evaluates the inode plane over the slots it leases plus — as the
//! volume's manager — the slots nobody leases; coverage `≡ leased ∪
//! unleased` per volume.

use super::appender::AppenderIdentity;
use super::backend::KvMetaBackend;
use super::record::Record;
use super::superblock::ExtentRef;
use super::KvError;
use crate::block_grant::{BlockGrant, BlockGrantLedger, CarveOutcome};
use crate::data_alloc_bitmap::DataAllocBitmap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Keys and records
// ---------------------------------------------------------------------------

/// `alloc_lease:` ‖ `vol_tag: u64 BE`.
pub const ALLOC_LEASE_KEY_PREFIX: &[u8] = b"alloc_lease:";
/// `dead_member:` ‖ `node_token: u64 BE` ‖ `mount_slot: u32 BE`.
pub const DEAD_MEMBER_KEY_PREFIX: &[u8] = b"dead_member:";
/// `recovered:` ‖ `node_token` ‖ `mount_slot` ‖ `vol: u16 BE`.
pub const RECOVERED_KEY_PREFIX: &[u8] = b"recovered:";
/// Record value version (byte 0 of every image here).
pub const ALLOC_LEASE_VERSION: u8 = 1;

const ALLOC_LEASE_FIXED_LEN: usize = 1 + 8 + 4 + 16 + 4 + 2 + 8 + 8 + 8 + 2;
const BITMAP_REF_LEN: usize = 2 + 8 + 8;
const DEAD_MEMBER_LEN: usize = 1 + 8 + 8;
const RECOVERED_LEN: usize = 1 + 8 + 8;

/// The tree-0 key of data volume `vol_tag`'s allocation lease.
pub fn alloc_lease_key(vol_tag: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(ALLOC_LEASE_KEY_PREFIX.len() + 8);
    k.extend_from_slice(ALLOC_LEASE_KEY_PREFIX);
    k.extend_from_slice(&vol_tag.to_be_bytes());
    k
}

/// Inclusive bounds over every `alloc_lease:` record.
pub fn alloc_lease_key_range() -> (Vec<u8>, Vec<u8>) {
    (alloc_lease_key(0), alloc_lease_key(u64::MAX))
}

/// Decode an `alloc_lease:` key to its volume tag.
pub fn decode_alloc_lease_key(key: &[u8]) -> Result<u64, KvError> {
    let want = ALLOC_LEASE_KEY_PREFIX.len() + 8;
    if key.len() != want || !key.starts_with(ALLOC_LEASE_KEY_PREFIX) {
        return Err(KvError::Corrupt(format!(
            "alloc_lease key must be {want} bytes under the {:?} prefix, got {} bytes",
            String::from_utf8_lossy(ALLOC_LEASE_KEY_PREFIX),
            key.len()
        )));
    }
    Ok(be64(key, ALLOC_LEASE_KEY_PREFIX.len()))
}

/// The tree-0 key of `member`'s death record.
pub fn dead_member_key(member: &AppenderIdentity) -> Vec<u8> {
    let mut k = Vec::with_capacity(DEAD_MEMBER_KEY_PREFIX.len() + 12);
    k.extend_from_slice(DEAD_MEMBER_KEY_PREFIX);
    k.extend_from_slice(&member.node_token.to_be_bytes());
    k.extend_from_slice(&member.mount_slot.to_be_bytes());
    k
}

/// The tree-0 key of `member`'s recovery record on volume `vol`.
pub fn recovered_key(member: &AppenderIdentity, vol: u16) -> Vec<u8> {
    let mut k = Vec::with_capacity(RECOVERED_KEY_PREFIX.len() + 14);
    k.extend_from_slice(RECOVERED_KEY_PREFIX);
    k.extend_from_slice(&member.node_token.to_be_bytes());
    k.extend_from_slice(&member.mount_slot.to_be_bytes());
    k.extend_from_slice(&vol.to_be_bytes());
    k
}

/// One data volume's allocation lease as volume 0's manager attests it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocLeaseRecord {
    pub holder: AppenderIdentity,
    /// The holder's appender id on its home volume.
    pub holder_appender_id: u32,
    /// The holder's HOME metadata volume ordinal — where its ring and its
    /// bitmap pages live.
    pub home_vol: u16,
    /// The holder's control ino (the shared index's home, PR 7).
    pub control_ino: u64,
    /// Blocks the bitmap covers (the pages' geometry).
    pub blocks: u64,
    /// The lease term: +1 per grant, never at a replay.
    pub term: u64,
    /// The bitmap pages, volume-qualified: `(home_vol, extent)`.
    pub bitmap: Vec<(u16, ExtentRef)>,
}

impl AllocLeaseRecord {
    /// LE image, versioned.
    pub fn encode(&self) -> Result<Vec<u8>, KvError> {
        let n = u16::try_from(self.bitmap.len()).map_err(|_| {
            KvError::Corrupt(format!(
                "alloc_lease record cannot carry {} bitmap refs (the count is a u16)",
                self.bitmap.len()
            ))
        })?;
        let mut out =
            Vec::with_capacity(ALLOC_LEASE_FIXED_LEN + self.bitmap.len() * BITMAP_REF_LEN);
        out.push(ALLOC_LEASE_VERSION);
        out.extend_from_slice(&self.holder.node_token.to_le_bytes());
        out.extend_from_slice(&self.holder.mount_slot.to_le_bytes());
        out.extend_from_slice(&self.holder.writer_id.to_le_bytes());
        out.extend_from_slice(&self.holder_appender_id.to_le_bytes());
        out.extend_from_slice(&self.home_vol.to_le_bytes());
        out.extend_from_slice(&self.control_ino.to_le_bytes());
        out.extend_from_slice(&self.blocks.to_le_bytes());
        out.extend_from_slice(&self.term.to_le_bytes());
        out.extend_from_slice(&n.to_le_bytes());
        for (vol, ext) in &self.bitmap {
            out.extend_from_slice(&vol.to_le_bytes());
            out.extend_from_slice(&ext.start.to_le_bytes());
            out.extend_from_slice(&ext.len.to_le_bytes());
        }
        Ok(out)
    }

    /// Decode + validate (total).
    pub fn decode(value: &[u8]) -> Result<Self, KvError> {
        let version = *value
            .first()
            .ok_or_else(|| KvError::Corrupt("alloc_lease record is empty".to_string()))?;
        if version != ALLOC_LEASE_VERSION {
            return Err(KvError::Corrupt(format!(
                "alloc_lease record version {version} — this binary writes {ALLOC_LEASE_VERSION} \
                 and the format is forward-only (upgrade squeezefs)"
            )));
        }
        if value.len() < ALLOC_LEASE_FIXED_LEN {
            return Err(KvError::Corrupt(format!(
                "alloc_lease record truncated: {} of {ALLOC_LEASE_FIXED_LEN} fixed bytes",
                value.len()
            )));
        }
        let mut off = 1;
        let node_token = le64(value, off);
        off += 8;
        let mount_slot = le32(value, off);
        off += 4;
        let mut w = [0u8; 16];
        w.copy_from_slice(&value[off..off + 16]);
        let writer_id = u128::from_le_bytes(w);
        off += 16;
        let holder_appender_id = le32(value, off);
        off += 4;
        let home_vol = le16(value, off);
        off += 2;
        let control_ino = le64(value, off);
        off += 8;
        let blocks = le64(value, off);
        off += 8;
        let term = le64(value, off);
        off += 8;
        let n = usize::from(le16(value, off));
        off += 2;
        let want = ALLOC_LEASE_FIXED_LEN + n * BITMAP_REF_LEN;
        if value.len() != want {
            return Err(KvError::Corrupt(format!(
                "alloc_lease record names {n} bitmap refs ({want} bytes) but holds {} bytes",
                value.len()
            )));
        }
        let mut bitmap = Vec::with_capacity(n);
        for _ in 0..n {
            let vol = le16(value, off);
            let start = le64(value, off + 2);
            let len = le64(value, off + 10);
            if len == 0 || start.checked_add(len).is_none() {
                return Err(KvError::Corrupt(
                    "alloc_lease record carries an empty or overflowing bitmap extent".to_string(),
                ));
            }
            bitmap.push((vol, ExtentRef { start, len }));
            off += BITMAP_REF_LEN;
        }
        Ok(Self {
            holder: AppenderIdentity {
                node_token,
                mount_slot,
                writer_id,
            },
            holder_appender_id,
            home_vol,
            control_ino,
            blocks,
            term,
            bitmap,
        })
    }
}

/// `dead_member:` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadMemberRecord {
    /// The S7 dead epoch the home shard minted.
    pub epoch: u64,
    /// Unix ms at the record.
    pub ts_ms: u64,
}

impl DeadMemberRecord {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(DEAD_MEMBER_LEN);
        out.push(ALLOC_LEASE_VERSION);
        out.extend_from_slice(&self.epoch.to_le_bytes());
        out.extend_from_slice(&self.ts_ms.to_le_bytes());
        out
    }

    pub fn decode(value: &[u8]) -> Result<Self, KvError> {
        if value.len() != DEAD_MEMBER_LEN || value[0] != ALLOC_LEASE_VERSION {
            return Err(KvError::Corrupt(format!(
                "dead_member record must be {DEAD_MEMBER_LEN} bytes at version \
                 {ALLOC_LEASE_VERSION}, got {} bytes",
                value.len()
            )));
        }
        Ok(Self {
            epoch: le64(value, 1),
            ts_ms: le64(value, 9),
        })
    }
}

/// `recovered:` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveredRecord {
    /// The recovering manager's term.
    pub by_term: u64,
    pub ts_ms: u64,
}

impl RecoveredRecord {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(RECOVERED_LEN);
        out.push(ALLOC_LEASE_VERSION);
        out.extend_from_slice(&self.by_term.to_le_bytes());
        out.extend_from_slice(&self.ts_ms.to_le_bytes());
        out
    }

    pub fn decode(value: &[u8]) -> Result<Self, KvError> {
        if value.len() != RECOVERED_LEN || value[0] != ALLOC_LEASE_VERSION {
            return Err(KvError::Corrupt(format!(
                "recovered record must be {RECOVERED_LEN} bytes at version {ALLOC_LEASE_VERSION}, \
                 got {} bytes",
                value.len()
            )));
        }
        Ok(Self {
            by_term: le64(value, 1),
            ts_ms: le64(value, 9),
        })
    }
}

#[inline]
fn le64(v: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&v[off..off + 8]);
    u64::from_le_bytes(b)
}

#[inline]
fn le32(v: &[u8], off: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&v[off..off + 4]);
    u32::from_le_bytes(b)
}

#[inline]
fn le16(v: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([v[off], v[off + 1]])
}

#[inline]
fn be64(v: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&v[off..off + 8]);
    u64::from_be_bytes(b)
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// The manager's verdicts
// ---------------------------------------------------------------------------

/// `AllocLeaseAcquire`'s answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocLeaseGrant {
    pub term: u64,
    /// The caller already held it (KD-SYM-7).
    pub already: bool,
    /// The dead predecessor's pages, volume-qualified — what the successor
    /// copies (empty on a first grant or a replay).
    pub predecessor_bitmap: Vec<(u16, ExtentRef)>,
    pub predecessor_blocks: u64,
}

// ---------------------------------------------------------------------------
// The holder's RAM: one holding per data volume this mount leases
// ---------------------------------------------------------------------------

/// One data volume's allocation lease as this mount HOLDS it: the bitmap,
/// the open grants, the pages' home.
pub struct AllocHolding {
    pub vol_tag: u64,
    pub term: u64,
    pub home_vol: u16,
    pub bitmap: DataAllocBitmap,
    pub ledger: BlockGrantLedger,
    /// The pages' extents on the home volume (device offsets).
    pub pages: Vec<ExtentRef>,
    /// Terminal frees whose bit was cleared in RAM at `finish_free` and
    /// whose CLEAR delta the next checkpoint journals.
    pending_clears: parking_lot::Mutex<Vec<u64>>,
    /// Deltas journaled (`data_alloc_bitmap_{set,clear}_bits` count the
    /// bits; these count the entries).
    pub deltas_journaled: AtomicU64,
    /// Frees held past the routine bound — the ring's timeout path
    /// (`free_grace_timeout_deferrals`).
    pub timeout_deferrals: AtomicU64,
}

impl std::fmt::Debug for AllocHolding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AllocHolding")
            .field("vol_tag", &format_args!("{:#018x}", self.vol_tag))
            .field("term", &self.term)
            .field("pages", &self.pages)
            .finish()
    }
}

impl AllocHolding {
    /// The device offset of the pages' region (the first extent's start;
    /// the extents are claimed contiguous where possible and the region is
    /// written across them in order).
    fn page_base(&self) -> u64 {
        self.pages.first().map_or(0, |e| e.start)
    }

    /// `finish_free` cleared `block` in RAM; the delta is journaled at the
    /// holder's next checkpoint.
    pub fn note_finish_free(&self, block: u64) -> bool {
        if self.bitmap.clear(block) {
            self.pending_clears.lock().push(block);
            true
        } else {
            false
        }
    }

    /// Take the pending clears.
    pub fn take_pending_clears(&self) -> Vec<u64> {
        std::mem::take(&mut *self.pending_clears.lock())
    }

    /// Pending clears.
    pub fn pending_clears(&self) -> usize {
        self.pending_clears.lock().len()
    }
}

static HOLDINGS: once_cell::sync::Lazy<scc::HashMap<u64, Arc<AllocHolding>>> =
    once_cell::sync::Lazy::new(scc::HashMap::new);
/// Holdings registered (the terminal free's one-load probe).
static HOLDINGS_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// `true` ⇔ this process holds at least one allocation lease.
pub fn holds_any() -> bool {
    HOLDINGS_COUNT.load(Ordering::Acquire) != 0
}

/// The holding this process keeps for data volume `vol_tag` (its
/// allocation lease), if any.
pub fn holding(vol_tag: u64) -> Option<Arc<AllocHolding>> {
    HOLDINGS.read_sync(&vol_tag, |_, v| Arc::clone(v))
}

/// Every holding.
pub fn holdings() -> Vec<Arc<AllocHolding>> {
    let mut out = Vec::new();
    HOLDINGS.iter_sync(|_, v| {
        out.push(Arc::clone(v));
        true
    });
    out
}

/// Forget a holding (the lease released / test teardown).
pub fn drop_holding(vol_tag: u64) -> Option<Arc<AllocHolding>> {
    let removed = HOLDINGS.remove_sync(&vol_tag).map(|(_, v)| v);
    if removed.is_some() {
        HOLDINGS_COUNT.fetch_sub(1, Ordering::AcqRel);
    }
    removed
}

/// Test seam: forget every holding.
pub fn test_clear_holdings() {
    HOLDINGS.clear_sync();
    HOLDINGS_COUNT.store(0, Ordering::Release);
}

/// `finish_free`'s hook (the block allocator's terminal free): clear the
/// bit on the holder of `vol_tag` if this process holds its lease. One
/// probe of an empty map on every unarmed mount.
pub fn note_finish_free(vol_tag: u64, block: u64) -> bool {
    holding(vol_tag).is_some_and(|h| h.note_finish_free(block))
}

/// The Allocation-lease family's per-holding face.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllocLeaseStats {
    pub vol_tag: u64,
    pub term: u64,
    pub block_grants: u64,
    pub block_grant_blocks: u64,
    pub blocks_returned: u64,
    pub grants_revoked: u64,
    pub bitmap_set_bits: u64,
    pub bitmap_clear_bits: u64,
    pub bitmap_population: u64,
    pub deltas_journaled: u64,
    pub free_grace_timeout_deferrals: u64,
}

impl AllocHolding {
    pub fn stats(&self) -> AllocLeaseStats {
        AllocLeaseStats {
            vol_tag: self.vol_tag,
            term: self.term,
            block_grants: self.ledger.grants_issued(),
            block_grant_blocks: self.ledger.blocks_granted(),
            blocks_returned: self.ledger.blocks_returned(),
            grants_revoked: self.ledger.revoked(),
            bitmap_set_bits: self.bitmap.set_count(),
            bitmap_clear_bits: self.bitmap.clear_count(),
            bitmap_population: self.bitmap.population(),
            deltas_journaled: self.deltas_journaled.load(Ordering::Relaxed),
            free_grace_timeout_deferrals: self.timeout_deferrals.load(Ordering::Relaxed),
        }
    }
}

// ---------------------------------------------------------------------------
// The death-ledger gauges (`dead_members_recorded` / `_acted`,
// `dead_member_propagation_ms`, `recovered_records`)
// ---------------------------------------------------------------------------

/// `dead_members_recorded` — death records this manager wrote.
pub static DEAD_MEMBERS_RECORDED: AtomicU64 = AtomicU64::new(0);
/// `dead_members_acted` — regions this node acted on from the ledger
/// (closure `acted ≡ recorded × regions held`; PR 10's driver counts).
pub static DEAD_MEMBERS_ACTED: AtomicU64 = AtomicU64::new(0);
/// `recovered_records` — recovery records this manager wrote.
pub static RECOVERED_RECORDS: AtomicU64 = AtomicU64::new(0);
/// `dead_member_propagation_ms` — the max wall from a death record's
/// timestamp to a reader acting on it.
pub static DEAD_MEMBER_PROPAGATION_MS: AtomicU64 = AtomicU64::new(0);

/// Count one act on a death record stamped `ts_ms` (PR 10's driver's
/// hook; the contracts' seam).
pub fn note_dead_member_acted(ts_ms: u64) {
    DEAD_MEMBERS_ACTED.fetch_add(1, Ordering::Relaxed);
    DEAD_MEMBER_PROPAGATION_MS.fetch_max(unix_now_ms().saturating_sub(ts_ms), Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// The coordinator predicate (KD-PV-14 under the symmetric plane)
// ---------------------------------------------------------------------------

static COORDINATOR: once_cell::sync::Lazy<arc_swap::ArcSwapOption<std::sync::Weak<KvMetaBackend>>> =
    once_cell::sync::Lazy::new(arc_swap::ArcSwapOption::empty);

/// Register the set's VOLUME 0 as the coordinator's home (the symmetric
/// arm); `None` withdraws it.
pub fn install_coordinator_volume(vol0: Option<&Arc<KvMetaBackend>>) {
    COORDINATOR.store(vol0.map(|v| Arc::new(Arc::downgrade(v))));
}

/// **Arm the symmetric ROLES of a routed set** (the routed open's last
/// step): when the slot-0 volume armed the plane, it is the coordinator's
/// home (KD-SYM-2) and this mount is a symmetric APPENDER homed there
/// (`home_volume` = the slot-0 volume's ordinal until PR 12's join ladder
/// chooses a home) whose `T_self` action is the PARK, bounded by
/// `T_park_max = manager_failover_bound_ms + T_owner` (the S6 grace
/// window is one owner TTL). Inert on an unarmed set.
pub fn arm_symmetric_roles(routed: &crate::meta_backend::RoutedMetaBackend) {
    let slot0 = routed.route_ino(1).0;
    let Some(vol0) = routed.volumes.get(slot0) else {
        return;
    };
    if !vol0.slot_lease_armed() {
        return;
    }
    install_coordinator_volume(Some(vol0));
    let failover = vol0.appender_stats().map_or(0, |s| s.failover_bound_ms);
    let grace_ms = crate::membership::LeaseClocks::derive(std::time::Duration::ZERO)
        .map(|c| c.t_owner.as_millis() as u64)
        .unwrap_or(0);
    crate::park_gate::arm_symmetric_appender(
        slot0 as u16,
        crate::park_gate::t_park_max_ms(failover, grace_ms),
    );
}

/// Disarm the roles (the leave / the plane's drop).
pub fn disarm_symmetric_roles() {
    install_coordinator_volume(None);
    crate::park_gate::disarm_symmetric_appender();
}

/// **The ONE coordinator predicate** under the symmetric plane: the
/// maintenance coordinator is volume 0's MANAGER. `None` = this node may
/// coordinate (an unarmed mount — one relaxed load — or the manager
/// itself); `Some(refusal)` names the class. Consulted by
/// `jobs::maintenance_coordinator_refusal` ahead of the per-volume-owner
/// map.
pub fn symmetric_coordinator_refusal() -> Option<String> {
    let guard = COORDINATOR.load();
    let vol0 = guard.as_ref()?.upgrade()?;
    let word = vol0.appender_stats()?.manager_lease.word();
    if word == "held" {
        return None;
    }
    Some(format!(
        "refusing to COORDINATE maintenance on this volume set: under the symmetric plane the \
         maintenance coordinator is VOLUME 0's MANAGER (KD-SYM-2 — the slot-0 holder), and \
         this mount's manager lease on volume 0 reads `{word}`. Submit fsck/defrag/job verbs on \
         the manager; this mount still evaluates the inode plane over the slots it LEASES \
         (the lessee shard), which is participation, not a second coordinator"
    ))
}

// ---------------------------------------------------------------------------
// The executors
// ---------------------------------------------------------------------------

/// The lessee-shard coverage of one volume's inode plane.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotCoverage {
    /// Slots this mount leases (the native slot included).
    pub leased: u64,
    /// Slots nobody leases — the manager's to walk (0 elsewhere).
    pub unleased: u64,
    /// Slots another appender leases — its shard's.
    pub foreign: u64,
    /// `leased + unleased` — what this mount's pass covers.
    pub covered: u64,
}

const DELTAS_PER_ENTRY: usize = 1024;

impl KvMetaBackend {
    /// Read `vol_tag`'s allocation lease from tree 0 (`None` = unleased).
    pub async fn alloc_lease_record(
        &self,
        vol_tag: u64,
    ) -> Result<Option<AllocLeaseRecord>, KvError> {
        let Some(control) = self.forest_control_tree() else {
            return Ok(None);
        };
        match control.lookup(&alloc_lease_key(vol_tag)).await? {
            Some(v) => AllocLeaseRecord::decode(&v).map(Some),
            None => Ok(None),
        }
    }

    /// Read `member`'s death record.
    pub async fn dead_member_record(
        &self,
        member: &AppenderIdentity,
    ) -> Result<Option<DeadMemberRecord>, KvError> {
        let Some(control) = self.forest_control_tree() else {
            return Ok(None);
        };
        match control.lookup(&dead_member_key(member)).await? {
            Some(v) => DeadMemberRecord::decode(&v).map(Some),
            None => Ok(None),
        }
    }

    /// Read `member`'s recovery record for volume `vol`.
    pub async fn recovered_record(
        &self,
        member: &AppenderIdentity,
        vol: u16,
    ) -> Result<Option<RecoveredRecord>, KvError> {
        let Some(control) = self.forest_control_tree() else {
            return Ok(None);
        };
        match control.lookup(&recovered_key(member, vol)).await? {
            Some(v) => RecoveredRecord::decode(&v).map(Some),
            None => Ok(None),
        }
    }

    /// **`AllocLeaseAcquire`** on volume 0's manager (§5.5.1's ordering
    /// law): a first-come grant at term 1; the holder's own ask answers
    /// `already`; a LIVE holder refuses everyone else (`Busy` — the
    /// requester allocates through grants from that holder); a DEAD holder
    /// (`dead_member:` present) is succeeded ONLY once
    /// `recovered:{holder, home_vol}` exists — before it the ask is
    /// DEFERRED (`GrantDeferred`, the requester retries); the successor's
    /// record names the predecessor's pages until it publishes its copy
    /// ([`Self::manager_alloc_lease_bitmap`]).
    pub async fn manager_alloc_lease_acquire(
        &self,
        vol_tag: u64,
        identity: AppenderIdentity,
        appender_id: u32,
        home_vol: u16,
        control_ino: u64,
        blocks: u64,
    ) -> Result<AllocLeaseGrant, KvError> {
        let set = self.manager_gate(false)?;
        let prior = self.alloc_lease_record(vol_tag).await?;
        let (term, predecessor_bitmap, predecessor_blocks) = match &prior {
            None => (1, Vec::new(), 0),
            Some(rec) if rec.holder == identity => {
                set.verbs.replays.fetch_add(1, Ordering::Relaxed);
                return Ok(AllocLeaseGrant {
                    term: rec.term,
                    already: true,
                    predecessor_bitmap: Vec::new(),
                    predecessor_blocks: 0,
                });
            }
            Some(rec) => {
                if self.dead_member_record(&rec.holder).await?.is_none() {
                    set.verbs.refusals.fetch_add(1, Ordering::Relaxed);
                    return Err(KvError::Busy(format!(
                        "{}: data volume {vol_tag:#018x}'s allocation lease is held by node \
                         {:#018x} slot {} (term {}) and the death ledger does not name it — a \
                         live holder's lease moves only by its release; allocate through block \
                         grants from the holder",
                        self.device_path().display(),
                        rec.holder.node_token,
                        rec.holder.mount_slot,
                        rec.term
                    )));
                }
                if self
                    .recovered_record(&rec.holder, rec.home_vol)
                    .await?
                    .is_none()
                {
                    return Err(KvError::LeaseDeferred(format!(
                        "data volume {vol_tag:#018x}'s allocation lease holder (node {:#018x} \
                         slot {}) is dead but its home region on volume {} is not yet \
                         recovered (no `recovered:` record) — the lease is re-granted only \
                         after the home manager's ring replay applied the bitmap deltas \
                         (design-symmetric-metadata §5.5.1); retry",
                        rec.holder.node_token, rec.holder.mount_slot, rec.home_vol
                    )));
                }
                (rec.term + 1, rec.bitmap.clone(), rec.blocks)
            }
        };
        let rec = AllocLeaseRecord {
            holder: identity,
            holder_appender_id: appender_id,
            home_vol,
            control_ino,
            blocks: if predecessor_blocks != 0 {
                predecessor_blocks
            } else {
                blocks
            },
            term,
            bitmap: predecessor_bitmap.clone(),
        };
        self.write_control_entry(
            vec![(
                super::record::TREE_CONTROL,
                Record::put(alloc_lease_key(vol_tag), 0, rec.encode()?),
            )],
            super::backend::EntryAdmission::Try,
        )
        .await?;
        set.verbs.verbs.fetch_add(1, Ordering::Relaxed);
        Ok(AllocLeaseGrant {
            term,
            already: false,
            predecessor_bitmap,
            predecessor_blocks,
        })
    }

    /// **`AllocLeaseBitmap`**: the holder at `term` publishes where its
    /// pages live (the successor's copy, or a first holder's fresh pages).
    /// Idempotent (`already` = the record already names them); a caller
    /// that is not the holder at that term refuses.
    pub async fn manager_alloc_lease_bitmap(
        &self,
        vol_tag: u64,
        identity: AppenderIdentity,
        term: u64,
        bitmap: Vec<(u16, ExtentRef)>,
    ) -> Result<bool, KvError> {
        let set = self.manager_gate(false)?;
        let Some(mut rec) = self.alloc_lease_record(vol_tag).await? else {
            set.verbs.refusals.fetch_add(1, Ordering::Relaxed);
            return Err(KvError::Busy(format!(
                "{}: no allocation lease exists for data volume {vol_tag:#018x}",
                self.device_path().display()
            )));
        };
        if rec.holder != identity || rec.term != term {
            set.verbs.refusals.fetch_add(1, Ordering::Relaxed);
            return Err(KvError::Busy(format!(
                "{}: data volume {vol_tag:#018x}'s allocation lease is held at term {} by node \
                 {:#018x} slot {}, not by the caller at term {term}",
                self.device_path().display(),
                rec.term,
                rec.holder.node_token,
                rec.holder.mount_slot
            )));
        }
        if rec.bitmap == bitmap {
            set.verbs.replays.fetch_add(1, Ordering::Relaxed);
            return Ok(true);
        }
        rec.bitmap = bitmap;
        self.write_control_entry(
            vec![(
                super::record::TREE_CONTROL,
                Record::put(alloc_lease_key(vol_tag), 0, rec.encode()?),
            )],
            super::backend::EntryAdmission::Try,
        )
        .await?;
        set.verbs.verbs.fetch_add(1, Ordering::Relaxed);
        Ok(false)
    }

    /// **`AllocLeaseRelease`**: the holder at `term` gives the lease up
    /// (its clean leave). `already` = no lease, or another term's.
    pub async fn manager_alloc_lease_release(
        &self,
        vol_tag: u64,
        identity: AppenderIdentity,
        term: u64,
    ) -> Result<bool, KvError> {
        let set = self.manager_gate(true)?;
        let Some(rec) = self.alloc_lease_record(vol_tag).await? else {
            set.verbs.replays.fetch_add(1, Ordering::Relaxed);
            return Ok(true);
        };
        if rec.holder != identity || rec.term != term {
            set.verbs.replays.fetch_add(1, Ordering::Relaxed);
            return Ok(true);
        }
        self.write_control_entry(
            vec![(
                super::record::TREE_CONTROL,
                Record::delete(alloc_lease_key(vol_tag), 0),
            )],
            super::backend::EntryAdmission::Try,
        )
        .await?;
        set.verbs.verbs.fetch_add(1, Ordering::Relaxed);
        Ok(false)
    }

    /// **`RecordRecovered { member, vol }`** (§5.5.2 / §5.9): the manager
    /// that recovered `member`'s region on volume `vol` says so — the
    /// record that gates the allocation-lease re-grant. Idempotent.
    pub async fn manager_record_recovered(
        &self,
        member: AppenderIdentity,
        vol: u16,
    ) -> Result<bool, KvError> {
        let set = self.manager_gate(false)?;
        if self.recovered_record(&member, vol).await?.is_some() {
            set.verbs.replays.fetch_add(1, Ordering::Relaxed);
            return Ok(true);
        }
        let rec = RecoveredRecord {
            by_term: crate::dlm::durable_term(),
            ts_ms: unix_now_ms(),
        };
        self.write_control_entry(
            vec![(
                super::record::TREE_CONTROL,
                Record::put(recovered_key(&member, vol), 0, rec.encode()),
            )],
            super::backend::EntryAdmission::Try,
        )
        .await?;
        set.verbs.verbs.fetch_add(1, Ordering::Relaxed);
        RECOVERED_RECORDS.fetch_add(1, Ordering::Relaxed);
        Ok(false)
    }

    /// **The death ledger's RECORD** (§5.5.2): `dead_member:{member} →
    /// { epoch, ts }` in tree 0 of volume 0. In-process only — PR 10's
    /// recovery driver is the production writer (the wire's `RecordDeath`
    /// refuses naming it); the contracts drive it as the seam. Idempotent.
    pub async fn record_death(
        &self,
        member: AppenderIdentity,
        epoch: u64,
    ) -> Result<bool, KvError> {
        let set = self.manager_gate(false)?;
        if self.dead_member_record(&member).await?.is_some() {
            set.verbs.replays.fetch_add(1, Ordering::Relaxed);
            return Ok(true);
        }
        let rec = DeadMemberRecord {
            epoch,
            ts_ms: unix_now_ms(),
        };
        self.write_control_entry(
            vec![(
                super::record::TREE_CONTROL,
                Record::put(dead_member_key(&member), 0, rec.encode()),
            )],
            super::backend::EntryAdmission::Try,
        )
        .await?;
        set.verbs.verbs.fetch_add(1, Ordering::Relaxed);
        DEAD_MEMBERS_RECORDED.fetch_add(1, Ordering::Relaxed);
        Ok(false)
    }

    // -----------------------------------------------------------------
    // The holder half — this backend is the holder's HOME volume
    // -----------------------------------------------------------------

    /// **Hold `vol_tag`'s allocation lease at `term`** on this home
    /// volume: claim heap extents for the pages from the manager's own
    /// bitmap (appender 0 claims directly — its alloc deltas journaled in
    /// ring 0 like every claim of its own), write the pages — a COPY of
    /// `predecessor` (a recovered region image) or fresh all-clear pages —
    /// barrier, register the holding. Returns it with the extents the
    /// caller publishes through [`Self::manager_alloc_lease_bitmap`].
    pub async fn hold_alloc_lease(
        &self,
        vol_tag: u64,
        blocks: u64,
        term: u64,
        predecessor: Option<&[u8]>,
    ) -> Result<Arc<AllocHolding>, KvError> {
        let set = self.manager_gate(false)?;
        let node_size = self.node_cache().config().layout.node_size() as u64;
        let region = crate::data_alloc_bitmap::region_len(blocks);
        let extents_needed = region.div_ceil(node_size).max(1);
        let mut claimed: Vec<u64> = Vec::with_capacity(extents_needed as usize);
        let mut recs: Vec<(u8, Record)> = Vec::new();
        for _ in 0..extents_needed {
            match self.allocator().claim_internal() {
                Ok(e) => {
                    claimed.push(e);
                    recs.push(super::alloc_ext::alloc_record(e, 0));
                }
                Err(e) => {
                    for c in claimed {
                        self.allocator().release_unpublished(c);
                    }
                    return Err(e);
                }
            }
        }
        // The pages are written into the claimed extents in claim order;
        // the record names them as runs.
        let bitmap = match predecessor {
            Some(image) => DataAllocBitmap::from_region_image(vol_tag, blocks, image),
            None => DataAllocBitmap::new(vol_tag, blocks),
        };
        let image = bitmap.region_image(term)?;
        let mut ops: Vec<(u64, bytes::Bytes)> = Vec::new();
        let mut written = 0usize;
        for e in &claimed {
            let addr = self.superblock().heap.start + e * node_size;
            let end = (written + node_size as usize).min(image.len());
            if written < end {
                ops.push((addr, bytes::Bytes::copy_from_slice(&image[written..end])));
            }
            written = end;
        }
        crate::uring_fs::write_at_batch(self.device_path(), ops)
            .await
            .map_err(KvError::Io)?;
        self.write_control_entry(recs, super::backend::EntryAdmission::Try)
            .await?;
        let pages: Vec<ExtentRef> =
            super::slot_state::ExtentGrantRecord::from_extents(claimed.iter().copied())
                .runs
                .iter()
                .map(|r| ExtentRef {
                    start: self.superblock().heap.start + r.start * node_size,
                    len: u64::from(r.len) * node_size,
                })
                .collect();
        set.verbs.verbs.fetch_add(1, Ordering::Relaxed);
        let holding = Arc::new(AllocHolding {
            vol_tag,
            term,
            home_vol: crate::park_gate::home_volume(),
            bitmap,
            ledger: BlockGrantLedger::new(),
            pages,
            pending_clears: parking_lot::Mutex::new(Vec::new()),
            deltas_journaled: AtomicU64::new(0),
            timeout_deferrals: AtomicU64::new(0),
        });
        if HOLDINGS
            .upsert_sync(vol_tag, Arc::clone(&holding))
            .is_none()
        {
            HOLDINGS_COUNT.fetch_add(1, Ordering::AcqRel);
        }
        Ok(holding)
    }

    /// Read a bitmap region image off `refs` on THIS volume (the
    /// predecessor's pages, when its home volume is this one — the
    /// in-process shape; a foreign home volume's reader is PR 10's driver).
    pub async fn read_alloc_bitmap_image(
        &self,
        refs: &[ExtentRef],
        blocks: u64,
    ) -> Result<Vec<u8>, KvError> {
        let want = crate::data_alloc_bitmap::region_len(blocks) as usize;
        let mut out = Vec::with_capacity(want);
        for r in refs {
            if out.len() >= want {
                break;
            }
            let take = (want - out.len()).min(r.len as usize);
            let got = crate::uring_fs::read_at(self.device_path(), r.start, take).await?;
            out.extend_from_slice(&got);
        }
        out.resize(want, 0);
        Ok(out)
    }

    async fn journal_data_alloc_deltas(
        &self,
        holding: &AllocHolding,
        blocks: impl Iterator<Item = u64>,
        set: bool,
    ) -> Result<(), KvError> {
        let mut chunk: Vec<(u8, Record)> = Vec::with_capacity(DELTAS_PER_ENTRY);
        for b in blocks {
            chunk.push(if set {
                crate::data_alloc_bitmap::set_record(holding.vol_tag, b, 0)
            } else {
                crate::data_alloc_bitmap::clear_record(holding.vol_tag, b, 0)
            });
            if chunk.len() == DELTAS_PER_ENTRY {
                self.write_control_entry(
                    std::mem::take(&mut chunk),
                    super::backend::EntryAdmission::Try,
                )
                .await?;
                holding.deltas_journaled.fetch_add(1, Ordering::Relaxed);
            }
        }
        if !chunk.is_empty() {
            self.write_control_entry(chunk, super::backend::EntryAdmission::Try)
                .await?;
            holding.deltas_journaled.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// **`BlockGrant { vol_tag, writer, want }`** on the holder: carve
    /// (bits SET in RAM), journal the SET deltas in this ring and BARRIER,
    /// then answer — a grant a writer holds is always journaled (the
    /// `LaneReservation` law). `want == 0` = the derivation over the
    /// holder's known writers. A journal failure gives the bits back and
    /// refuses.
    pub async fn holder_block_grant(
        &self,
        vol_tag: u64,
        writer: &str,
        want: u64,
        held_unconsumed: u64,
    ) -> Result<CarveOutcome, KvError> {
        let holding = holding(vol_tag).ok_or_else(|| {
            KvError::Busy(format!(
                "{}: this mount does not hold data volume {vol_tag:#018x}'s allocation lease",
                self.device_path().display()
            ))
        })?;
        let writers = holding.ledger.writers().len() as u64 + 1;
        let want = if want == 0 {
            crate::block_grant::block_grant_derived(
                0,
                crate::membership::renewal_beat_ms(),
                holding.bitmap.blocks(),
                writers,
            )
        } else {
            want.min(crate::block_grant::block_grant_derived(
                u64::MAX / 4,
                1,
                holding.bitmap.blocks(),
                writers,
            ))
        };
        let floor = holding.ledger.grant_frontier().unwrap_or(0);
        let outcome = holding
            .ledger
            .carve(&holding.bitmap, writer, want, floor, held_unconsumed);
        if let CarveOutcome::Granted(g) = &outcome {
            if let Err(e) = self
                .journal_data_alloc_deltas(&holding, g.start..g.end(), true)
                .await
            {
                let _ = holding.ledger.return_blocks(&holding.bitmap, writer, *g);
                return Err(e);
            }
        }
        Ok(outcome)
    }

    /// **`ReturnBlocks`** on the holder: the writer gives back an
    /// unconsumed range — bits CLEAR, CLEAR deltas journaled. `None` ⇔ the
    /// range is not the writer's (refused, nothing cleared).
    pub async fn holder_return_blocks(
        &self,
        vol_tag: u64,
        writer: &str,
        range: BlockGrant,
    ) -> Result<Option<u64>, KvError> {
        let holding = holding(vol_tag).ok_or_else(|| {
            KvError::Busy(format!(
                "{}: this mount does not hold data volume {vol_tag:#018x}'s allocation lease",
                self.device_path().display()
            ))
        })?;
        let Some(cleared) = holding.ledger.return_blocks(&holding.bitmap, writer, range) else {
            return Ok(None);
        };
        self.journal_data_alloc_deltas(&holding, range.start..range.end(), false)
            .await?;
        Ok(Some(cleared))
    }

    /// **The holder's checkpoint step**: journal the pending `finish_free`
    /// clears, write every dirty page into its alternate slot. Called by
    /// the checkpoint task after the appender pages; a no-op on a mount
    /// holding no lease (one probe of an empty map).
    pub async fn write_data_alloc_pages(&self, ckpt_seq: u64) -> Result<(), KvError> {
        for holding in holdings() {
            if holding.home_vol != crate::park_gate::home_volume() {
                continue;
            }
            let clears = holding.take_pending_clears();
            if !clears.is_empty() {
                self.journal_data_alloc_deltas(&holding, clears.into_iter(), false)
                    .await?;
            }
            holding
                .bitmap
                .write_dirty_pages(self.device_path(), holding.page_base(), ckpt_seq)
                .await?;
        }
        Ok(())
    }

    /// **`replay_data_alloc_deltas`** (§5.5.1's recovery arm): scan THIS
    /// volume's fixed ring from the ledger's tail (the window a dead
    /// holder's crash left) and apply its kind-4 data deltas to `bitmap`
    /// — the pages loaded from the dead holder's extents. Returns the bits
    /// changed. PR 10's recovery driver runs it before `recovered:`; the
    /// contracts call it as the seam.
    pub async fn replay_data_alloc_deltas(&self, bitmap: &DataAllocBitmap) -> Result<u64, KvError> {
        let ring = Self::fixed_ring_extent(self.superblock());
        let pages = ring.len / super::journal::JOURNAL_PAGE_LEN;
        let (_, recovery) = super::journal::JournalRing::recover(
            self.device_path(),
            ring.start,
            pages,
            super::journal::checkpoint_reserve_bytes(ring.len),
            self.ledger_tail(),
        )
        .await?;
        // The window's deltas the mount's own replay met (kept because the
        // bring-up checkpoint may already have advanced the tail past
        // them) plus whatever the ring still holds — one fold, LWW by seq.
        let kept = crate::data_alloc_bitmap::take_replayed_deltas(bitmap.vol_tag());
        let changed = bitmap.replay(
            kept.iter()
                .map(|r| (super::record::TREE_ALLOC_RESERVED, r))
                .chain(recovery.entries.iter().flat_map(|e| {
                    e.records
                        .iter()
                        .map(|(t, r)| (super::journal::untag(*t).0, r))
                })),
        );
        Ok(changed)
    }

    // -----------------------------------------------------------------
    // The §5.5.2 vol-0 rule — the manager role's release, wired
    // -----------------------------------------------------------------

    /// **The vol-0 rule's driver** (§5.5.2; PR 3 left the decision as
    /// `manager_should_release_role`): one probe of volume 0's ledger
    /// (tree 0 of volume 0 as a projection) at `now_ms`. Unreachable for
    /// longer than `T_owner` ⇒ this manager RELEASES its role — the lease
    /// word reads `vacant`, the coordinator predicate refuses, the D0
    /// claim itself stays (handing the claim to a successor is PR 10's
    /// ladder) — counted on `manager_vol0_unreachable`. `true` ⇔ released
    /// by this probe. The production prober is PR 10's ledger poll; the
    /// contracts drive it.
    pub fn note_vol0_ledger_probe(&self, reachable: bool, now_ms: u64, t_owner_ms: u64) -> bool {
        let Some(set) = self.appenders_public() else {
            return false;
        };
        if reachable {
            set.vol0_unreachable_since_ms.store(0, Ordering::Release);
            return false;
        }
        let since = set.vol0_unreachable_since_ms.load(Ordering::Acquire);
        if since == 0 {
            set.vol0_unreachable_since_ms
                .store(now_ms.max(1), Ordering::Release);
            return false;
        }
        if !super::appender::manager_should_release_role(now_ms.saturating_sub(since), t_owner_ms) {
            return false;
        }
        let mut lease = set.manager_lease.lock().unwrap_or_else(|e| e.into_inner());
        if *lease != super::appender::ManagerLease::Held {
            return false;
        }
        *lease = super::appender::ManagerLease::Vacant;
        drop(lease);
        set.vol0_unreachable.fetch_add(1, Ordering::Relaxed);
        log::error!(
            "meta volume {}: volume 0's ledger unreachable for {} ms (> T_owner {t_owner_ms} \
             ms) — RELEASING the manager role rather than act on a stale death ledger \
             (manager_vol0_unreachable; design-symmetric-metadata §5.5.2)",
            self.device_path().display(),
            now_ms.saturating_sub(since)
        );
        true
    }

    // -----------------------------------------------------------------
    // The lessee shards
    // -----------------------------------------------------------------

    /// **Does this mount's inode-plane pass judge local key ino `local`?**
    /// Unarmed: every slot is the mount's (`true`, one relaxed load).
    /// Armed: the slot is LEASED here, or — on the volume's manager —
    /// UNLEASED (nobody's to judge but the manager's); a slot another
    /// appender leases is its lessee's shard.
    pub fn inode_plane_owns_slot(&self, local: u64) -> bool {
        let gate = self.node_cache().lease_gate();
        if !gate.is_armed() {
            return true;
        }
        let slot = super::record::forest_slot_of_ino(local);
        matches!(
            gate.verdict_structural(slot),
            crate::slot_lease_core::CommitVerdict::Allowed
                | crate::slot_lease_core::CommitVerdict::Unarmed
        )
    }

    /// The coverage of this mount's inode-plane pass over the volume's
    /// slots: leased ∪ (unleased, on the manager); `covered` is the
    /// `fsck_inode_plane_slots_covered` gauge's per-volume term. On an
    /// unarmed volume every hosted slot is covered.
    pub async fn inode_plane_slot_coverage(&self) -> Result<SlotCoverage, KvError> {
        let mut cov = SlotCoverage::default();
        let gate = self.node_cache().lease_gate();
        let Some(control) = self.forest_control_tree() else {
            // A flat volume: one shard, every slot its own.
            cov.leased = 1;
            cov.covered = 1;
            return Ok(cov);
        };
        // The native slot is the manager's by KD-SYM-2.
        let native_ours = !gate.is_armed() || gate.is_leased(super::record::NATIVE_FOREST_SLOT);
        if native_ours {
            cov.leased += 1;
        } else {
            cov.foreign += 1;
        }
        let (mut cursor, end) = super::slot_state::slot_state_key_range();
        loop {
            let page = control.range(&cursor, &end, 512).await?;
            let Some((last, _)) = page.last() else {
                break;
            };
            cursor = super::node::key_successor(last);
            for (k, v) in &page {
                let slot = super::slot_state::decode_slot_state_key(k)?;
                if slot == super::record::NATIVE_FOREST_SLOT {
                    continue;
                }
                match super::slot_state::SlotState::decode(v)? {
                    super::slot_state::SlotState::Leased { .. } if gate.is_leased(slot) => {
                        cov.leased += 1
                    }
                    super::slot_state::SlotState::Leased { .. } if gate.is_armed() => {
                        cov.foreign += 1
                    }
                    super::slot_state::SlotState::Leased { .. } => cov.leased += 1,
                    super::slot_state::SlotState::Unleased { .. } => {
                        if gate.is_leased(slot) {
                            cov.leased += 1;
                        } else if !gate.is_armed() || gate.is_manager() {
                            cov.unleased += 1;
                        } else {
                            cov.foreign += 1;
                        }
                    }
                }
            }
            if page.len() < 512 {
                break;
            }
        }
        cov.covered = cov.leased + cov.unleased;
        Ok(cov)
    }
}
