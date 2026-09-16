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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

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
/// The PR-8 image: version ‖ epoch ‖ ts.
const DEAD_MEMBER_LEN_V1: usize = 1 + 8 + 8;
/// PR 10's image appends the victim's registrant key (the preempt's
/// input); a v1 image decodes with key 0 — the record is dark, but the
/// decoder is total over both shapes.
const DEAD_MEMBER_LEN: usize = DEAD_MEMBER_LEN_V1 + 8;
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

/// Decode a `dead_member:` key to its identity (`None` for any other key
/// the prefix range may return — the range is inclusive of the prefix).
pub fn decode_dead_member_key(key: &[u8]) -> Option<AppenderIdentity> {
    let want = DEAD_MEMBER_KEY_PREFIX.len() + 12;
    if key.len() != want || !key.starts_with(DEAD_MEMBER_KEY_PREFIX) {
        return None;
    }
    let p = DEAD_MEMBER_KEY_PREFIX.len();
    let mut nt = [0u8; 8];
    nt.copy_from_slice(&key[p..p + 8]);
    let mut ms = [0u8; 4];
    ms.copy_from_slice(&key[p + 8..p + 12]);
    Some(AppenderIdentity {
        node_token: u64::from_be_bytes(nt),
        mount_slot: u32::from_be_bytes(ms),
        writer_id: 0,
    })
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
    /// The dead member's NVMe registrant key (`0` = none / unknown): what
    /// the recovering manager PREEMPTS on the volume's namespace before it
    /// reads the ring (design-symmetric-metadata §5.9; PR 10).
    pub pr_key: u64,
}

impl DeadMemberRecord {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(DEAD_MEMBER_LEN);
        out.push(ALLOC_LEASE_VERSION);
        out.extend_from_slice(&self.epoch.to_le_bytes());
        out.extend_from_slice(&self.ts_ms.to_le_bytes());
        out.extend_from_slice(&self.pr_key.to_le_bytes());
        out
    }

    pub fn decode(value: &[u8]) -> Result<Self, KvError> {
        let keyed = match value.len() {
            DEAD_MEMBER_LEN_V1 => false,
            DEAD_MEMBER_LEN => true,
            _ => {
                return Err(KvError::Corrupt(format!(
                    "dead_member record must be {DEAD_MEMBER_LEN_V1} or {DEAD_MEMBER_LEN} bytes \
                     at version {ALLOC_LEASE_VERSION}, got {} bytes",
                    value.len()
                )))
            }
        };
        if value[0] != ALLOC_LEASE_VERSION {
            return Err(KvError::Corrupt(format!(
                "dead_member record carries version {}, expected {ALLOC_LEASE_VERSION}",
                value[0]
            )));
        }
        Ok(Self {
            epoch: le64(value, 1),
            ts_ms: le64(value, 9),
            pr_key: if keyed { le64(value, 17) } else { 0 },
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

/// Unix ms now (the ledger records' timestamps).
pub fn unix_now_ms() -> u64 {
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

/// One queued bitmap mutation awaiting its ring write, in RAM order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueuedDelta {
    block: u64,
    set: bool,
}

/// One data volume's allocation lease as this mount HOLDS it: the bitmap,
/// the open grants, the pages' home — and **the ordering law's one lock**
/// (review round 1, Issue 2): every bit mutation (a carve's SET, a
/// return's CLEAR, `finish_free`'s CLEAR) moves the bit AND queues its
/// delta under `mutations`, in one critical section, so the queue's order
/// IS the RAM order; one drainer at a time (`drain`) journals the queue
/// head-first into the HOME volume's ring, so record-seq order is queue
/// order — and the replay's per-key LWW fold by seq reads the RAM order.
/// A CLEAR is journaled at the instant of its decision: `finish_free` is
/// synchronous (the reclaim worker's thread, the grace harvest), so it
/// queues under the lock and kicks the single-flight journaler
/// (`kick_journaler`); a carve drains the queue itself and answers only
/// once its SET (and everything queued before it) is durable.
pub struct AllocHolding {
    pub vol_tag: u64,
    pub term: u64,
    /// The holder's home volume ordinal (the record's `home_vol`; the
    /// shared-block index's home follows it — `shared_refs::index_home_
    /// volume_for`).
    home_vol: std::sync::atomic::AtomicU16,
    /// The HOME backend — the one whose ring the deltas journal into and
    /// whose device carries the pages; `write_data_alloc_pages` acts only
    /// on the backend this points at (review round 1, Issue 3).
    home: Weak<KvMetaBackend>,
    pub bitmap: DataAllocBitmap,
    pub ledger: BlockGrantLedger,
    /// The pages' extents on the home volume (device offsets).
    pub pages: Vec<ExtentRef>,
    /// The ordered mutation stream: pushed under this lock together with
    /// the bit it records.
    mutations: parking_lot::Mutex<std::collections::VecDeque<QueuedDelta>>,
    /// One drainer at a time journals the stream head-first.
    drain: crate::sqz_sync::SqzMutex<()>,
    /// The single-flight latch of the `finish_free` journaler task.
    journaler_running: AtomicBool,
    /// Deltas journaled (`data_alloc_bitmap_{set,clear}_bits` count the
    /// bits; these count the entries).
    pub deltas_journaled: AtomicU64,
    /// Drains the ring's USER window refused (`JournalReserveExhausted`)
    /// — the deltas stayed queued for the next drainer, the checkpoint
    /// cycle untouched (review round 2, Issue 22;
    /// `data_alloc_bitmap_deltas_deferred`).
    pub deltas_deferred: AtomicU64,
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

    /// `true` ⇔ `be` is this holding's home backend.
    pub fn is_homed_on(&self, be: &KvMetaBackend) -> bool {
        std::ptr::eq(self.home.as_ptr(), be)
    }

    /// The holder's home volume ordinal.
    pub fn home_vol(&self) -> u16 {
        self.home_vol.load(Ordering::Acquire)
    }

    /// TEST seam (review round 4, Issue 34): a holder homed on another
    /// volume — what PR 12's join ladder mints — so the index-home
    /// resolver's re-point is distinguishable from PR 7's default.
    pub fn test_set_home_vol(&self, home_vol: u16) {
        self.home_vol.store(home_vol, Ordering::Release);
    }

    /// **Carve for `writer`** (the holder's `BlockGrant`): bits SET and
    /// their deltas queued in one critical section. The caller drains
    /// (`KvMetaBackend::drain_deltas`) before it answers.
    pub fn carve(&self, writer: &str, want: u64, floor: u64, held_unconsumed: u64) -> CarveOutcome {
        let mut q = self.mutations.lock();
        let outcome = self
            .ledger
            .carve(&self.bitmap, writer, want, floor, held_unconsumed);
        if let CarveOutcome::Granted(g) = &outcome {
            q.extend((g.start..g.end()).map(|block| QueuedDelta { block, set: true }));
        }
        outcome
    }

    /// **Return `range` of `writer`'s grant**: bits CLEAR and their deltas
    /// queued in one critical section (`None` = not the writer's).
    pub fn return_blocks(&self, writer: &str, range: BlockGrant) -> Option<u64> {
        let mut q = self.mutations.lock();
        let cleared = self.ledger.return_blocks(&self.bitmap, writer, range)?;
        q.extend((range.start..range.end()).map(|block| QueuedDelta { block, set: false }));
        Some(cleared)
    }

    /// `finish_free` cleared `block`: the bit clears and its CLEAR is
    /// queued in one critical section, then the journaler is kicked — the
    /// delta lands NOW, in order, with no checkpoint in its path. `false`
    /// ⇔ the bit was already clear (a double free's shape).
    pub fn note_finish_free(self: &Arc<Self>, block: u64) -> bool {
        let was_set = {
            let mut q = self.mutations.lock();
            let was = self.bitmap.clear(block);
            if was {
                q.push_back(QueuedDelta { block, set: false });
            }
            was
        };
        if was_set {
            self.kick_journaler();
        }
        was_set
    }

    /// Revoke a dead writer's grants (the death record's arm — the bits
    /// stay SET for the quarantine).
    pub fn revoke_dead(&self, writer: &str) -> Vec<BlockGrant> {
        self.ledger.revoke_dead(writer)
    }

    /// Deltas queued and not yet journaled (the tests' witness that a
    /// CLEAR waits for no checkpoint).
    pub fn queued_deltas(&self) -> usize {
        self.mutations.lock().len()
    }

    /// Take the queue's head — up to `max` deltas, in order.
    fn take_queued(&self, max: usize) -> Vec<QueuedDelta> {
        let mut q = self.mutations.lock();
        let n = q.len().min(max);
        q.drain(..n).collect()
    }

    /// Put an undrained chunk BACK at the head (a failed ring write —
    /// order preserved: nothing after it was journaled meanwhile, since
    /// the drainer holds `drain`).
    fn requeue_front(&self, chunk: Vec<QueuedDelta>) {
        let mut q = self.mutations.lock();
        for d in chunk.into_iter().rev() {
            q.push_front(d);
        }
    }

    /// The single-flight journaler: one detached task drains the stream
    /// into the home ring; a kick while one runs is absorbed — the running
    /// task releases its latch and re-checks the queue before it exits,
    /// so a delta queued after its last drain is journaled by it or by
    /// the kick that took the latch, never left waiting.
    fn kick_journaler(self: &Arc<Self>) {
        if self
            .journaler_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let me = Arc::clone(self);
        crate::meta_exec::spawn_meta("data_alloc_delta_journaler", async move {
            // Releases the latch on every exit, a contained panic included.
            struct Latch<'a>(&'a AtomicBool, bool);
            impl Drop for Latch<'_> {
                fn drop(&mut self) {
                    if self.1 {
                        self.0.store(false, Ordering::Release);
                    }
                }
            }
            let mut latch = Latch(&me.journaler_running, true);
            let Some(home) = me.home.upgrade() else {
                return;
            };
            loop {
                if let Err(e) = home.drain_deltas(&me).await {
                    me.deltas_deferred.fetch_add(1, Ordering::Relaxed);
                    match e {
                        // The user window is full (the checkpoint cycle is
                        // what frees it): the deltas stay queued — the
                        // page write covers the bits, the next kick or the
                        // next carve journals them (Issue 22).
                        KvError::JournalReserveExhausted { .. } => log::debug!(
                            "data allocation bitmap of volume {:#018x}: the ring's user window \
                             is full; {} queued delta(s) deferred to the next drainer \
                             (data_alloc_bitmap_deltas_deferred)",
                            me.vol_tag,
                            me.queued_deltas()
                        ),
                        other => log::error!(
                            "data allocation bitmap of volume {:#018x}: journaling the queued \
                             deltas failed ({other}); {} delta(s) stay queued for the next \
                             drainer",
                            me.vol_tag,
                            me.queued_deltas()
                        ),
                    }
                    return;
                }
                latch.1 = false;
                me.journaler_running.store(false, Ordering::Release);
                if me.queued_deltas() == 0
                    || me
                        .journaler_running
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                {
                    return;
                }
                latch.1 = true;
            }
        });
    }
}

static HOLDINGS: once_cell::sync::Lazy<scc::HashMap<u64, Arc<AllocHolding>>> =
    once_cell::sync::Lazy::new(scc::HashMap::new);
/// Holdings registered (the terminal free's one acquire-load probe).
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
/// bit on the holder of `vol_tag` if this process holds its lease and
/// journal the CLEAR at once, in order. One probe of an empty map on
/// every unarmed mount (behind `holds_any`'s one acquire load).
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
    pub deltas_deferred: u64,
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
            deltas_deferred: self.deltas_deferred.load(Ordering::Relaxed),
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

/// A 1-bit-per-block set over one data volume (review round 4, Issue 31):
/// the durable reference population the arm reads — the mount's scan
/// handed over, or the arm's own — never a `BTreeSet`/`Vec` of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockBits {
    words: Vec<u64>,
    blocks: u64,
    population: u64,
}

impl BlockBits {
    /// An empty set over `blocks` blocks.
    pub fn new(blocks: u64) -> Self {
        Self {
            words: vec![0; blocks.div_ceil(64) as usize],
            blocks,
            population: 0,
        }
    }

    /// Set `block` (out of range ignored — a reference to a block past the
    /// derived capacity is the census's finding, never the seed's).
    pub fn set(&mut self, block: u64) {
        if block >= self.blocks {
            return;
        }
        let w = &mut self.words[(block / 64) as usize];
        let bit = 1u64 << (block % 64);
        if *w & bit == 0 {
            *w |= bit;
            self.population += 1;
        }
    }

    /// `true` ⇔ set.
    pub fn is_set(&self, block: u64) -> bool {
        block < self.blocks && self.words[(block / 64) as usize] & (1u64 << (block % 64)) != 0
    }

    /// Bits set.
    pub fn population(&self) -> u64 {
        self.population
    }

    /// `true` ⇔ nothing set.
    pub fn is_empty(&self) -> bool {
        self.population == 0
    }
}

/// The mount's own by-block scan, handed to the arm per data volume
/// (`recover_durable_block_refs` → [`note_mount_seed`]) so the arm reads
/// the ledger ONCE per mount (Issue 31). Taken by the arm; a stale entry
/// (a mount that never armed) is overwritten by the next mount's seed.
static MOUNT_SEEDS: once_cell::sync::Lazy<
    parking_lot::Mutex<std::collections::BTreeMap<u64, BlockBits>>,
> = once_cell::sync::Lazy::new(|| parking_lot::Mutex::new(std::collections::BTreeMap::new()));

/// The mount path's durable-refs seed for data volume `vol_tag`: the
/// referenced block indices, folded into one bitset over `blocks`.
pub fn note_mount_seed(vol_tag: u64, blocks: u64, refs: impl IntoIterator<Item = u64>) {
    let mut bits = BlockBits::new(blocks);
    for b in refs {
        bits.set(b);
    }
    MOUNT_SEEDS.lock().insert(vol_tag, bits);
}

/// Take the mount's seed for `vol_tag` (the arm's one consumer).
pub fn take_mount_seed(vol_tag: u64) -> Option<BlockBits> {
    MOUNT_SEEDS.lock().remove(&vol_tag)
}

/// One metadata volume's heap geometry as the manager validates a wire
/// bitmap ref against it (review round 1, Issue 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeHeap {
    pub heap_start: u64,
    pub heap_len: u64,
    pub node_size: u64,
}

/// The routed set's per-volume heap geometry, by ordinal — installed at
/// the arm (the manager's derived-state witness for `home_vol` and every
/// bitmap ref a wire frame carries).
static SET_HEAPS: once_cell::sync::Lazy<arc_swap::ArcSwap<Vec<VolumeHeap>>> =
    once_cell::sync::Lazy::new(|| arc_swap::ArcSwap::from_pointee(Vec::new()));

/// The set's volume heaps in ordinal order (empty on an unarmed mount —
/// the manager's own volume is then the one witness).
pub fn set_heaps() -> Arc<Vec<VolumeHeap>> {
    SET_HEAPS.load_full()
}

/// Install the set's heap geometry (the arm; `Vec::new()` withdraws it).
pub fn install_set_heaps(heaps: Vec<VolumeHeap>) {
    SET_HEAPS.store(Arc::new(heaps));
}

/// Every data volume's block count this node DERIVES from its allocators
/// (capacity ÷ block size), by `vol_tag` — the witness a wire `blocks`
/// is validated against (review round 1, Issue 6). Registered by the
/// allocation arm; the contracts register their data volume as the seam.
static DATA_VOLUME_BLOCKS: once_cell::sync::Lazy<scc::HashMap<u64, u64>> =
    once_cell::sync::Lazy::new(scc::HashMap::new);

/// Register data volume `vol_tag`'s derived block count.
pub fn register_data_volume_blocks(vol_tag: u64, blocks: u64) {
    let _ = DATA_VOLUME_BLOCKS.upsert_sync(vol_tag, blocks);
}

/// The derived block count of data volume `vol_tag` (`None` = not a data
/// volume of this set as this node knows it).
pub fn data_volume_blocks(vol_tag: u64) -> Option<u64> {
    DATA_VOLUME_BLOCKS.read_sync(&vol_tag, |_, v| *v)
}

/// The allocators the arm registered, by `vol_tag` — the quarantine sink
/// a death record's revoke reaches (`record_death` → `revoke_dead` →
/// `data_custody::quarantine_offsets`).
static ALLOCATORS: once_cell::sync::Lazy<
    scc::HashMap<u64, std::sync::Weak<crate::block_allocator::BlockAllocator>>,
> = once_cell::sync::Lazy::new(scc::HashMap::new);

/// The registered allocator of data volume `vol_tag`, if live.
pub fn allocator_for(vol_tag: u64) -> Option<Arc<crate::block_allocator::BlockAllocator>> {
    ALLOCATORS.read_sync(&vol_tag, |_, w| w.upgrade()).flatten()
}

/// `T_park_max` for a failover bound and the S6 clocks in force: the
/// GRACE term is `LeaseClocks::grace` (the owner-failover window) read
/// off the derivation (review round 1, Issue 16).
pub fn t_park_max_for(failover_bound_ms: u64, clocks: &crate::membership::LeaseClocks) -> u64 {
    crate::park_gate::t_park_max_for(failover_bound_ms, clocks)
}

/// **Arm the symmetric ROLES of a routed set** (the routed open's last
/// step): when the slot-0 volume armed the plane, it is the coordinator's
/// home (KD-SYM-2) and this mount is a symmetric APPENDER homed there
/// (`home_volume` = the slot-0 volume's ordinal until PR 12's join ladder
/// chooses a home) whose `T_self` action is the PARK, bounded by
/// `T_park_max = manager_failover_bound_ms + grace` (the S6 owner-failover
/// window, `LeaseClocks::grace`). The set's heap geometry is installed for
/// the manager's wire validation. Inert on an unarmed set.
pub fn arm_symmetric_roles(routed: &crate::meta_backend::RoutedMetaBackend) {
    let slot0 = routed.route_ino(1).0;
    let Some(vol0) = routed.volumes.get(slot0) else {
        return;
    };
    if !vol0.slot_lease_armed() {
        return;
    }
    install_coordinator_volume(Some(vol0));
    install_set_heaps(
        routed
            .volumes
            .iter()
            .map(|v| VolumeHeap {
                heap_start: v.superblock().heap.start,
                heap_len: v.superblock().heap.len,
                node_size: v.node_cache().config().layout.node_size() as u64,
            })
            .collect(),
    );
    let failover = vol0.appender_stats().map_or(0, |s| s.failover_bound_ms);
    let t_park_max = match crate::membership::LeaseClocks::derive(std::time::Duration::ZERO) {
        Ok(clocks) => t_park_max_for(failover, &clocks),
        Err(_) => failover,
    };
    crate::park_gate::arm_symmetric_appender(slot0 as u16, t_park_max);
}

/// Disarm the roles (the leave / the plane's drop): the holdings this
/// process kept are forgotten (their pages were written by the final
/// checkpoint — the lease record itself SURVIVES a clean leave, the next
/// open of the same identity re-holds it), the kept replay window of the
/// home volume is dropped, the allocators unregistered.
pub fn disarm_symmetric_roles() {
    if let Some(vol0) = COORDINATOR.load().as_ref().and_then(|w| w.upgrade()) {
        crate::data_alloc_bitmap::drop_replayed_deltas_for(vol0.device_path());
    }
    for h in holdings() {
        drop_holding(h.vol_tag);
        crate::block_grant::uninstall_free_target(h.vol_tag);
    }
    ALLOCATORS.clear_sync();
    install_set_heaps(Vec::new());
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

    /// Every death record in this volume's tree 0 (the ledger a manager
    /// reads as a projection — PR 10's recovery driver; empty off volume
    /// 0 and on a flat volume). Paged like every tree-0 range.
    pub async fn dead_member_records(
        &self,
    ) -> Result<Vec<(AppenderIdentity, DeadMemberRecord)>, KvError> {
        let Some(control) = self.forest_control_tree() else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        let mut cursor = DEAD_MEMBER_KEY_PREFIX.to_vec();
        let mut end = DEAD_MEMBER_KEY_PREFIX.to_vec();
        end.extend_from_slice(&[0xFF; 12]);
        loop {
            let page = control.range(&cursor, &end, 512).await?;
            let Some((last, _)) = page.last() else {
                break;
            };
            cursor = super::node::key_successor(last);
            for (k, v) in &page {
                let Some(member) = decode_dead_member_key(k) else {
                    continue;
                };
                out.push((member, DeadMemberRecord::decode(v)?));
            }
            if page.len() < 512 {
                break;
            }
        }
        Ok(out)
    }

    /// Every allocation-lease record in this volume's tree 0 (volume 0's
    /// set-wide records; empty elsewhere).
    pub async fn alloc_lease_records(&self) -> Result<Vec<(u64, AllocLeaseRecord)>, KvError> {
        let Some(control) = self.forest_control_tree() else {
            return Ok(Vec::new());
        };
        let (mut cursor, end) = alloc_lease_key_range();
        let mut out = Vec::new();
        loop {
            let page = control.range(&cursor, &end, 512).await?;
            let Some((last, _)) = page.last() else {
                break;
            };
            cursor = super::node::key_successor(last);
            for (k, v) in &page {
                let Ok(vol_tag) = decode_alloc_lease_key(k) else {
                    continue;
                };
                out.push((vol_tag, AllocLeaseRecord::decode(v)?));
            }
            if page.len() < 512 {
                break;
            }
        }
        Ok(out)
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

    /// **The service edge's screen for `AllocLeaseAcquire`** (review round
    /// 1, Issue 6 — PR 3/4's bounded-execution law): `blocks` must be
    /// non-zero and AT MOST the derived block count of a data volume this
    /// node knows (a peer's integer is never the successor's allocation
    /// authority; a smaller ask is a smaller bitmap over the same volume),
    /// `home_vol` must name a volume of the set. A failed screen is `Rejected`
    /// (`STATUS_REJECTED`, `manager_verb_rejected`), nothing written.
    pub fn screen_alloc_lease_acquire(
        &self,
        vol_tag: u64,
        home_vol: u16,
        blocks: u64,
    ) -> Result<(), KvError> {
        let Some(known) = data_volume_blocks(vol_tag) else {
            return Err(KvError::Rejected(format!(
                "{}: AllocLeaseAcquire names data volume {vol_tag:#018x}, which this set's \
                 allocation arm does not know — REJECTED (manager_verb_rejected)",
                self.device_path().display()
            )));
        };
        if blocks == 0 || blocks > known {
            return Err(KvError::Rejected(format!(
                "{}: AllocLeaseAcquire for data volume {vol_tag:#018x} asks for {blocks} \
                 blocks; the volume has {known} — a wire integer is never an allocation \
                 authority — REJECTED (manager_verb_rejected)",
                self.device_path().display()
            )));
        }
        let heaps = set_heaps();
        let width = if heaps.is_empty() { 1 } else { heaps.len() };
        if usize::from(home_vol) >= width {
            return Err(KvError::Rejected(format!(
                "{}: AllocLeaseAcquire names home volume {home_vol} of a {width}-volume set — \
                 REJECTED (manager_verb_rejected)",
                self.device_path().display()
            )));
        }
        Ok(())
    }

    /// **The service edge's screen for `AllocLeaseBitmap`** (review round
    /// 1, Issue 6 — the PR 4 root-witness discipline): every ref names a
    /// volume of the set, lies inside that volume's heap, is node-aligned
    /// (a whole number of nodes from the heap's start, a whole number of
    /// nodes long, non-empty, no overflow), and the refs together hold at
    /// least the region the record's `blocks` need and no more than the
    /// claim that region rounds up to. A failed screen is `Rejected`,
    /// nothing written.
    pub fn screen_alloc_lease_bitmap(
        &self,
        blocks: u64,
        bitmap: &[(u16, ExtentRef)],
    ) -> Result<(), KvError> {
        let heaps = set_heaps();
        let own = VolumeHeap {
            heap_start: self.superblock().heap.start,
            heap_len: self.superblock().heap.len,
            node_size: self.node_cache().config().layout.node_size() as u64,
        };
        let heap_of = |vol: u16| -> Option<VolumeHeap> {
            if heaps.is_empty() {
                (vol == 0).then_some(own)
            } else {
                heaps.get(usize::from(vol)).copied()
            }
        };
        screen_bitmap_refs(blocks, bitmap, heap_of).map_err(|why| {
            KvError::Rejected(format!(
                "{}: AllocLeaseBitmap {why} — REJECTED (manager_verb_rejected)",
                self.device_path().display()
            ))
        })
    }

    /// **`AllocLeaseAcquire`** on volume 0's manager (§5.5.1's ordering
    /// law): a first-come grant at term 1; the holder's own ask answers
    /// `already` WITH the record's pages and block count (crash row 8 — a
    /// successor that died before its copy retries and still learns what
    /// to copy; review round 1, Issue 14); a LIVE holder refuses everyone
    /// else (`Busy` — the requester allocates through grants from that
    /// holder); a DEAD holder (`dead_member:` present) is succeeded ONLY
    /// once `recovered:{holder, home_vol}` exists — before it the ask is
    /// DEFERRED (`LeaseDeferred`, the requester retries); the successor's
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
        if let Err(e) = self.screen_alloc_lease_acquire(vol_tag, home_vol, blocks) {
            set.verbs.rejected.fetch_add(1, Ordering::Relaxed);
            return Err(e);
        }
        let prior = self.alloc_lease_record(vol_tag).await?;
        let (term, predecessor_bitmap, predecessor_blocks) = match &prior {
            None => (1, Vec::new(), 0),
            Some(rec) if rec.holder == identity => {
                set.verbs.replays.fetch_add(1, Ordering::Relaxed);
                return Ok(AllocLeaseGrant {
                    term: rec.term,
                    already: true,
                    predecessor_bitmap: rec.bitmap.clone(),
                    predecessor_blocks: rec.blocks,
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
    /// that is not the holder at that term refuses; refs the screen
    /// rejects are never persisted.
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
        if let Err(e) = self.screen_alloc_lease_bitmap(rec.blocks, &bitmap) {
            set.verbs.rejected.fetch_add(1, Ordering::Relaxed);
            return Err(e);
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
    /// refuses naming it); the allocation arm's same-node takeover and the
    /// contracts drive it. Idempotent. **The record's arm on the holder**
    /// (review round 1, Issue 7): every allocation lease this process
    /// holds REVOKES the dead writer's open grants and QUARANTINES their
    /// blocks on the volume's allocator under a dead epoch (S7 — the bits
    /// stay SET until a drain proof releases them); `dead_members_acted`
    /// counts the act.
    pub async fn record_death(
        &self,
        member: AppenderIdentity,
        epoch: u64,
    ) -> Result<bool, KvError> {
        self.record_death_with_key(member, epoch, 0).await
    }

    /// [`Self::record_death`] carrying the dead member's registrant key
    /// — the PRODUCTION writer's form (PR 10): the S6 owner's eviction
    /// knows the key the member's join presented, and the recovering
    /// manager preempts it on every volume the member appended to.
    pub async fn record_death_with_key(
        &self,
        member: AppenderIdentity,
        epoch: u64,
        pr_key: u64,
    ) -> Result<bool, KvError> {
        let set = self.manager_gate(false)?;
        if self.dead_member_record(&member).await?.is_some() {
            set.verbs.replays.fetch_add(1, Ordering::Relaxed);
            return Ok(true);
        }
        let rec = DeadMemberRecord {
            epoch,
            ts_ms: unix_now_ms(),
            pr_key,
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
        let name = crate::meta_ship::manager::wire_writer_name(&member.into());
        for holding in holdings() {
            let revoked = holding.revoke_dead(&name);
            if revoked.is_empty() {
                continue;
            }
            let Some(alloc) = allocator_for(holding.vol_tag) else {
                log::warn!(
                    "death record for node {:#018x} slot {}: {} grant(s) on data volume \
                     {:#018x} revoked, but no allocator is registered for the quarantine — the \
                     bits stay SET (the census releases them as a leak)",
                    member.node_token,
                    member.mount_slot,
                    revoked.len(),
                    holding.vol_tag
                );
                continue;
            };
            let dead = crate::data_custody::declare_dead_epoch(&format!(
                "allocation holder of data volume {:#018x}: writer node {:#018x} slot {} named \
                 by the death ledger (epoch {epoch})",
                holding.vol_tag, member.node_token, member.mount_slot
            ));
            let chunk = alloc.chunk_size();
            let offsets = revoked
                .iter()
                .flat_map(|g| (g.start..g.end()).map(move |b| b * chunk));
            let quarantined = crate::data_custody::quarantine_offsets(&alloc, offsets, dead);
            log::warn!(
                "death record for node {:#018x} slot {}: {} grant(s) on data volume {:#018x} \
                 revoked, {quarantined} block(s) quarantined under {dead} (dead_members_acted)",
                member.node_token,
                member.mount_slot,
                revoked.len(),
                holding.vol_tag
            );
            note_dead_member_acted(rec.ts_ms);
        }
        Ok(false)
    }

    // -----------------------------------------------------------------
    // The holder half — this backend is the holder's HOME volume
    // -----------------------------------------------------------------

    /// Register `holding` (the process-wide map, one per data volume).
    fn register_holding(holding: &Arc<AllocHolding>) {
        if HOLDINGS
            .upsert_sync(holding.vol_tag, Arc::clone(holding))
            .is_none()
        {
            HOLDINGS_COUNT.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn new_holding(
        self: &Arc<Self>,
        vol_tag: u64,
        term: u64,
        bitmap: DataAllocBitmap,
        pages: Vec<ExtentRef>,
    ) -> Arc<AllocHolding> {
        let holding = Arc::new(AllocHolding {
            vol_tag,
            term,
            home_vol: std::sync::atomic::AtomicU16::new(crate::park_gate::home_volume()),
            home: Arc::downgrade(self),
            bitmap,
            ledger: BlockGrantLedger::new(),
            pages,
            mutations: parking_lot::Mutex::new(std::collections::VecDeque::new()),
            drain: crate::sqz_sync::SqzMutex::new(()),
            journaler_running: AtomicBool::new(false),
            deltas_journaled: AtomicU64::new(0),
            deltas_deferred: AtomicU64::new(0),
            timeout_deferrals: AtomicU64::new(0),
        });
        Self::register_holding(&holding);
        holding
    }

    /// **Hold `vol_tag`'s allocation lease at `term`** on this home
    /// volume: claim heap extents for the pages from the manager's own
    /// bitmap (appender 0 claims directly — its alloc deltas journaled in
    /// ring 0 like every claim of its own), write the pages — a COPY of
    /// `predecessor` (a recovered region image) or fresh all-clear pages
    /// — barrier, register the holding. Returns it with the extents the
    /// caller publishes through [`Self::manager_alloc_lease_bitmap`]. A
    /// failed page write releases the claims (review round 1, Issue 15).
    pub async fn hold_alloc_lease(
        self: &Arc<Self>,
        vol_tag: u64,
        blocks: u64,
        term: u64,
        predecessor: Option<&[u8]>,
    ) -> Result<Arc<AllocHolding>, KvError> {
        // The image is decoded BEFORE any claim: a corrupt predecessor
        // region refuses here with nothing to undo.
        let bitmap = match predecessor {
            Some(image) => DataAllocBitmap::from_region_image(vol_tag, blocks, image)?,
            None => DataAllocBitmap::new(vol_tag, blocks),
        };
        self.hold_bitmap(vol_tag, bitmap, term).await
    }

    /// **The FIRST hold of a POPULATED volume** (review round 2, Issue 23):
    /// a fresh bitmap SEEDED from the allocator's derived truth at this
    /// instant — every block in `set` (the refcount map's population plus
    /// every offset that is neither on the free list nor above the cursor:
    /// in-limbo between `begin_free` and `finish_free`, quarantined,
    /// grace-held, inside a trim window) reads SET; the free list's blocks
    /// read CLEAR. The seeded pages are written and barriered before the
    /// lease record names them (the caller publishes). A `None` seed is
    /// the fresh volume.
    pub async fn hold_alloc_lease_seeded(
        self: &Arc<Self>,
        vol_tag: u64,
        blocks: u64,
        term: u64,
        set: impl IntoIterator<Item = u64>,
    ) -> Result<Arc<AllocHolding>, KvError> {
        let bitmap = DataAllocBitmap::new(vol_tag, blocks);
        for b in set {
            bitmap.set(b);
        }
        self.hold_bitmap(vol_tag, bitmap, term).await
    }

    /// Claim the page extents, write `bitmap`'s image into them, journal
    /// the claims, barrier, register the holding (the shared body of the
    /// two holds above).
    async fn hold_bitmap(
        self: &Arc<Self>,
        vol_tag: u64,
        bitmap: DataAllocBitmap,
        term: u64,
    ) -> Result<Arc<AllocHolding>, KvError> {
        let set = self.manager_gate(false)?;
        let blocks = bitmap.blocks();
        let image = bitmap.region_image(term)?;
        let node_size = self.node_cache().config().layout.node_size() as u64;
        let region = crate::data_alloc_bitmap::region_len(blocks);
        let extents_needed = region.div_ceil(node_size).max(1);
        let mut claimed: Vec<u64> = Vec::with_capacity(extents_needed as usize);
        let release_claims = |claimed: &[u64]| {
            for c in claimed {
                self.allocator().release_unpublished(*c);
            }
        };
        let mut recs: Vec<(u8, Record)> = Vec::new();
        for _ in 0..extents_needed {
            match self.allocator().claim_internal() {
                Ok(e) => {
                    claimed.push(e);
                    recs.push(super::alloc_ext::alloc_record(e, 0));
                }
                Err(e) => {
                    release_claims(&claimed);
                    return Err(e);
                }
            }
        }
        // The pages are written into the claimed extents in claim order;
        // the record names them as runs.
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
        if let Err(e) = crate::uring_fs::write_at_batch(self.device_path(), ops).await {
            release_claims(&claimed);
            return Err(KvError::Io(e));
        }
        if let Err(e) = self
            .write_control_entry(recs, super::backend::EntryAdmission::Try)
            .await
        {
            release_claims(&claimed);
            return Err(e);
        }
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
        Ok(self.new_holding(vol_tag, term, bitmap, pages))
    }

    /// **Re-hold a lease this identity already holds** (a clean remount,
    /// or the own-residue recovery after a crash — the record still names
    /// OUR pages on this volume): read them, fold the kept replay window
    /// and the ring's window of THIS term over them (the recovery arm),
    /// write the changed pages + barrier, register the holding on the SAME
    /// extents. No claim, no copy.
    pub async fn rehold_alloc_lease(
        self: &Arc<Self>,
        vol_tag: u64,
        rec: &AllocLeaseRecord,
    ) -> Result<Arc<AllocHolding>, KvError> {
        let refs: Vec<ExtentRef> = rec.bitmap.iter().map(|(_, e)| *e).collect();
        self.screen_alloc_lease_bitmap(rec.blocks, &rec.bitmap)?;
        let image = self.read_alloc_bitmap_image(&refs, rec.blocks).await?;
        let bitmap = DataAllocBitmap::from_region_image(vol_tag, rec.blocks, &image)?;
        let changed = self.replay_data_alloc_deltas(&bitmap, rec.term).await?;
        if changed > 0 {
            let base = refs.first().map_or(0, |e| e.start);
            bitmap
                .write_dirty_pages(
                    self.device_path(),
                    base,
                    self.checkpoint_seq.load(Ordering::Acquire) + 1,
                )
                .await?;
            self.sync_device().await.map_err(KvError::Io)?;
            log::warn!(
                "data volume {vol_tag:#018x}: re-held its allocation lease at term {} — the \
                 window's {changed} delta(s) folded onto the pages (own-residue recovery)",
                rec.term
            );
        }
        Ok(self.new_holding(vol_tag, rec.term, bitmap, refs))
    }

    /// Read a bitmap region image off `refs` on THIS volume (the
    /// predecessor's pages, when its home volume is this one — the
    /// in-process shape; a foreign home volume's reader is PR 10's driver).
    /// `blocks` and every ref are screened against this volume's heap
    /// before anything proportional to them is allocated (Issue 6).
    pub async fn read_alloc_bitmap_image(
        &self,
        refs: &[ExtentRef],
        blocks: u64,
    ) -> Result<Vec<u8>, KvError> {
        let heap = self.superblock().heap;
        let node = self.node_cache().config().layout.node_size() as u64;
        let refs_here: Vec<(u16, ExtentRef)> = refs.iter().map(|e| (0u16, *e)).collect();
        screen_bitmap_refs(blocks, &refs_here, |v| {
            (v == 0).then_some(VolumeHeap {
                heap_start: heap.start,
                heap_len: heap.len,
                node_size: node,
            })
        })
        .map_err(|why| {
            KvError::Rejected(format!(
                "{}: allocation bitmap refs {why}",
                self.device_path().display()
            ))
        })?;
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

    /// **Drain `holding`'s queued deltas into this ring**, head-first, one
    /// drainer at a time — the ordering law's journal half. A failed
    /// entry puts its chunk back at the head (nothing behind it was
    /// journaled) and returns the error.
    pub async fn drain_deltas(&self, holding: &AllocHolding) -> Result<(), KvError> {
        let _one = holding.drain.lock().await;
        loop {
            let chunk = holding.take_queued(DELTAS_PER_ENTRY);
            if chunk.is_empty() {
                return Ok(());
            }
            let recs: Vec<(u8, Record)> = chunk
                .iter()
                .map(|d| {
                    if d.set {
                        crate::data_alloc_bitmap::set_record(
                            holding.vol_tag,
                            d.block,
                            holding.term,
                            0,
                        )
                    } else {
                        crate::data_alloc_bitmap::clear_record(
                            holding.vol_tag,
                            d.block,
                            holding.term,
                            0,
                        )
                    }
                })
                .collect();
            if let Err(e) = self
                .write_control_entry(recs, super::backend::EntryAdmission::Try)
                .await
            {
                holding.requeue_front(chunk);
                return Err(e);
            }
            holding.deltas_journaled.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// **`BlockGrant { vol_tag, writer, want }`** on the holder: carve
    /// (bits SET and deltas queued in one critical section), DRAIN the
    /// stream into this ring — the SET and everything queued before it
    /// journaled and BARRIERED — then answer: a grant a writer holds is
    /// always journaled (the `LaneReservation` law). `want == 0` = the
    /// derivation over the holder's known writers. A journal failure
    /// gives the bits back and refuses.
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
        if !holding.is_homed_on(self) {
            return Err(KvError::Busy(format!(
                "{}: data volume {vol_tag:#018x}'s allocation lease is homed on another \
                 metadata volume of this set",
                self.device_path().display()
            )));
        }
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
        let outcome = holding.carve(writer, want, floor, held_unconsumed);
        if let CarveOutcome::Granted(g) = &outcome {
            if let Err(e) = self.drain_deltas(&holding).await {
                let _ = holding.return_blocks(writer, *g);
                return Err(e);
            }
        }
        Ok(outcome)
    }

    /// **`ReturnBlocks`** on the holder: the writer gives back an
    /// unconsumed range — bits CLEAR, CLEAR deltas journaled in order.
    /// `None` ⇔ the range is not the writer's (refused, nothing cleared).
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
        let Some(cleared) = holding.return_blocks(writer, range) else {
            return Ok(None);
        };
        self.drain_deltas(&holding).await?;
        Ok(Some(cleared))
    }

    /// **The holder's checkpoint step** — run BEFORE barrier #1 and the
    /// ledger record, beside the meta bitmap's own page write (review
    /// round 1, Issue 1: the record's tail passes every delta journaled
    /// below the cycle's head, so the pages must already carry them; a
    /// delta journaled after the head is inside the window the tail
    /// keeps): write every dirty page into its alternate slot — for the
    /// holdings HOMED ON THIS backend only (Issue 3: another volume's
    /// checkpoint touches neither the pages nor the dirty bits). The
    /// queue is NOT drained here (review round 2, Issue 22 — §4.4 pt 5:
    /// no USER-class admission ever decides the cycle's outcome): the
    /// page snapshot already carries every RAM mutation, a queued SET only
    /// ever costs a leak and a queued CLEAR reads a genuinely free block,
    /// and the carve/return drains are synchronous — a non-empty queue is
    /// handed to the single-flight journaler OFF the cycle (drain-and-
    /// retry), counted on `deltas_deferred` when it was a refused window.
    /// A no-op on a mount holding no lease.
    pub async fn write_data_alloc_pages(&self, ckpt_seq: u64) -> Result<(), KvError> {
        for holding in holdings() {
            if !holding.is_homed_on(self) {
                continue;
            }
            if holding.queued_deltas() > 0 {
                holding.kick_journaler();
            }
            // The maintained population against the popcount, once per
            // cycle (Issue 33) — the one place the scan runs.
            holding.bitmap.population_drift_check();
            holding
                .bitmap
                .write_dirty_pages(self.device_path(), holding.page_base(), ckpt_seq)
                .await?;
        }
        Ok(())
    }

    /// **`replay_data_alloc_deltas`** (§5.5.1's recovery arm): scan THIS
    /// volume's fixed ring from the ledger's tail (the window a dead
    /// holder's crash left) and apply its kind-4 data deltas of holder
    /// `term` to `bitmap` — the pages loaded from the dead holder's
    /// extents — together with the window's deltas the mount's own replay
    /// KEPT for exactly this (volume, term, home). Returns the bits
    /// changed. PR 10's recovery driver runs it before `recovered:`; the
    /// re-hold and the contracts call it.
    pub async fn replay_data_alloc_deltas(
        &self,
        bitmap: &DataAllocBitmap,
        term: u64,
    ) -> Result<u64, KvError> {
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
        let kept = crate::data_alloc_bitmap::take_replayed_deltas(
            self.device_path(),
            bitmap.vol_tag(),
            term,
        );
        let changed = bitmap.replay(
            kept.iter()
                .map(|r| (super::record::TREE_ALLOC_RESERVED, r))
                .chain(recovery.entries.iter().flat_map(|e| {
                    e.records
                        .iter()
                        .map(|(t, r)| (super::journal::untag(*t).0, r))
                })),
            term,
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

// ---------------------------------------------------------------------------
// The service edge's pure screens (fuzzed by `manager_call_frame`'s PR-8
// arm, mirrored in `tests/decoder_property_tests.rs`)
// ---------------------------------------------------------------------------

/// **Screen a set of bitmap refs against the volumes' heaps** (review
/// round 1, Issue 6): every ref names a volume `heap_of` knows, lies inside
/// its heap, starts a whole number of nodes from the heap's start, is a
/// whole number of nodes long, non-empty, does not overflow; together the
/// refs cover at least `region_len(blocks)` and at most the nodes that
/// region rounds up to. Pure — the manager's and the reader's one screen.
pub fn screen_bitmap_refs(
    blocks: u64,
    bitmap: &[(u16, ExtentRef)],
    heap_of: impl Fn(u16) -> Option<VolumeHeap>,
) -> Result<(), String> {
    if blocks == 0 {
        return Err("names a zero-block volume".to_string());
    }
    if bitmap.is_empty() {
        return Err("names no page extent".to_string());
    }
    let region = crate::data_alloc_bitmap::region_len(blocks);
    let mut total: u64 = 0;
    let mut node_size_max = 0u64;
    for (vol, e) in bitmap {
        let Some(heap) = heap_of(*vol) else {
            return Err(format!("ref {e:?} names volume {vol}, which the set lacks"));
        };
        node_size_max = node_size_max.max(heap.node_size);
        let Some(end) = e.start.checked_add(e.len) else {
            return Err(format!("ref {e:?} overflows"));
        };
        let Some(heap_end) = heap.heap_start.checked_add(heap.heap_len) else {
            return Err(format!("volume {vol}'s heap geometry overflows"));
        };
        if e.len == 0 {
            return Err(format!("ref {e:?} is empty"));
        }
        if e.start < heap.heap_start || end > heap_end {
            return Err(format!(
                "ref {e:?} lies outside volume {vol}'s heap [{:#x}, {heap_end:#x})",
                heap.heap_start
            ));
        }
        if heap.node_size == 0
            || (e.start - heap.heap_start) % heap.node_size != 0
            || e.len % heap.node_size != 0
        {
            return Err(format!(
                "ref {e:?} is not node-aligned on volume {vol} (node {})",
                heap.node_size
            ));
        }
        total = total.saturating_add(e.len);
    }
    if total < region {
        return Err(format!(
            "refs hold {total} B, the {blocks}-block bitmap needs {region} B"
        ));
    }
    let needed_nodes = region.div_ceil(node_size_max.max(1)).max(1);
    let cap = needed_nodes.saturating_mul(node_size_max);
    if total > cap {
        return Err(format!(
            "refs hold {total} B, the {blocks}-block bitmap claims at most {cap} B"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The PRODUCTION allocation arm (review round 1, Issue 7)
// ---------------------------------------------------------------------------

/// The in-process holder's grant sink for a writer named `writer`:
/// `holder_block_grant` on the home volume.
pub fn holder_block_grant_sink(
    home: &Arc<KvMetaBackend>,
    vol_tag: u64,
    writer: String,
) -> crate::block_grant::BlockGrantSink {
    let home = Arc::downgrade(home);
    Arc::new(move |want, held| {
        let home = home.clone();
        let writer = writer.clone();
        Box::pin(async move {
            let home = home.upgrade()?;
            match home.holder_block_grant(vol_tag, &writer, want, held).await {
                Ok(CarveOutcome::Granted(g)) => Some(vec![g]),
                Ok(CarveOutcome::Already(gs)) => Some(gs),
                Ok(CarveOutcome::Full) => None,
                Err(e) => {
                    log::warn!(
                        "block grant on data volume {vol_tag:#018x} refused by the holder: {e}"
                    );
                    None
                }
            }
        })
    })
}

/// A WIRE writer's grant sink: `ManagerCall::BlockGrant` to the manager
/// venue at `endpoint` (one storage-trust session, reconnected on
/// failure). PR 12's second daemon is the production caller; the
/// contracts drive it here.
pub fn wire_block_grant_sink(
    endpoint: String,
    secret: Vec<u8>,
    writer: crate::meta_ship::manager::WireIdentity,
    volume: u16,
    vol_tag: u64,
) -> crate::block_grant::BlockGrantSink {
    let client: Arc<crate::sqz_sync::SqzMutex<Option<crate::meta_ship::manager::ManagerClient>>> =
        Arc::new(crate::sqz_sync::SqzMutex::new(None));
    Arc::new(move |want, held| {
        let client = Arc::clone(&client);
        let endpoint = endpoint.clone();
        let secret = secret.clone();
        Box::pin(async move {
            let mut slot = client.lock().await;
            if slot.is_none() {
                let peer = format!("appender-{:#x}-{}", writer.node_token, writer.mount_slot);
                match crate::meta_ship::manager::ManagerClient::connect(
                    &endpoint, &secret, &peer, volume,
                )
                .await
                {
                    Ok(c) => *slot = Some(c),
                    Err(e) => {
                        log::warn!("block grant venue {endpoint} unreachable: {e}");
                        return None;
                    }
                }
            }
            let c = slot.as_mut()?;
            // The holder's derivation answers `want == 0`; the wire carries
            // a u32 ask.
            let ask = u32::try_from(want).unwrap_or(u32::MAX);
            match c.block_grant(vol_tag, writer, ask, held).await {
                Ok(g) => g,
                Err(e) => {
                    log::warn!("block grant over the wire failed: {e}; reconnecting next ask");
                    *slot = None;
                    None
                }
            }
        })
    })
}

/// Install the manager venue at `endpoint` as the FREE TARGET of data
/// volume `vol_tag` — where a wire writer's terminal frees ship
/// (`cowriter::ship_displaced_frees` routes to it). `true` ⇔ newly
/// installed or moved.
pub fn install_wire_free_target(vol_tag: u64, endpoint: &str) -> bool {
    let moved = crate::block_grant::free_target_for(vol_tag).as_deref() != Some(endpoint);
    crate::block_grant::install_free_target(vol_tag, endpoint.to_string());
    moved
}

/// **Arm the ARMED plane's data allocation** (the mount path, after the
/// multi-writer arm; review round 1, Issue 7 — the delivered arm): for
/// every data volume this mount writes (`allocators`), register its
/// derived block count, acquire its allocation lease FIRST-COME through
/// volume 0's manager (this mount — the D0 winner), hold it (fresh pages
/// at term 1; a re-hold of our own record after a clean remount or a
/// crash — the own-residue recovery folds the window's deltas onto the
/// pages; a same-node predecessor of another mount slot is DEAD by the
/// D0 flock's proof — its death and its home's recovery are recorded here
/// and its lease succeeded), publish the refs, and install the grant arm
/// on the allocator with the in-process holder as its sink — from here
/// the allocator's fresh mint comes from the granted window and its
/// terminal frees clear the holder's bits. Inert on an unarmed mount
/// (`Ok(0)`, one load). A live FOREIGN holder of a data volume this
/// manager writes is a shape PR 8 cannot reach (a wire joiner's venue is
/// PR 12's) and refuses loud rather than allocate beside it unarmed.
pub async fn arm_symmetric_allocation(
    routed: &Arc<crate::meta_backend::RoutedMetaBackend>,
    allocators: &[Arc<crate::block_allocator::BlockAllocator>],
) -> Result<usize, crate::error::SqueezefsError> {
    if !crate::park_gate::symmetric_appender_armed() {
        return Ok(0);
    }
    let slot0 = routed.route_ino(1).0;
    let Some(vol0) = routed.volumes.get(slot0) else {
        return Ok(0);
    };
    let Some(set) = vol0.appenders_public() else {
        return Ok(0);
    };
    let me = set.identity;
    let home_vol = crate::park_gate::home_volume();
    let mut held = 0usize;
    for alloc in allocators {
        let vol_tag = crate::meta_backend::kv::block_refs::volume_tag(alloc.volume_id());
        let chunk = alloc.chunk_size().max(1);
        let blocks = alloc.capacity_bytes() / chunk;
        if blocks == 0 {
            log::warn!(
                "symmetric allocation arm: data volume '{}' reports no capacity — not leased",
                alloc.volume_id()
            );
            continue;
        }
        register_data_volume_blocks(vol_tag, blocks);
        let _ = ALLOCATORS.upsert_sync(vol_tag, Arc::downgrade(alloc));
        // The allocator's DERIVED truth at this instant (review round 2,
        // Issue 23): the FIRST hold is seeded from it, and every hold is
        // checked against it in the LOSS direction — a block the derived
        // allocator holds live that the bitmap reads CLEAR would be re-
        // granted, so the arm refuses loud rather than allocate beside it.
        // The snapshot is 1 bit per block below the cursor (Issue 29a). The
        // arm runs before FUSE serves (the mount path), so it is quiescent
        // — and that is CHECKED, not assumed (Issue 29b): a second snapshot
        // after the hold must equal the first.
        let derived = alloc.derived_allocation_snapshot();
        // The seed's structural guard (review round 3, Issue 27): the
        // derived state is the mount-time by-block seed's — on a forest
        // PR 7 keeps that ONE scan as the free list's derivation — so an
        // allocator whose cursor reads 0 while the set's durable reference
        // ledger names blocks of this volume is UN-SEEDED (a rung that
        // skipped the scan), and seeding from it would re-grant every live
        // block. The ledger's union over the set's volumes (every slot tree
        // on a forest — `block_ref_scan`'s law) is read here, O(refs) once
        // per data volume at the arm, and is also the loss check's second
        // witness below.
        // ONE pass, ONE bitset (review round 4, Issue 31): the mount's own
        // scan (`recover_durable_block_refs` hands its per-volume index
        // list here through `note_mount_seed`) is reused when present;
        // otherwise the ledger is walked once, straight into the bit words
        // — never a `BTreeSet`/`Vec` of the population (8 GiB at a full
        // PiB); O(refs) time, 1 bit per block of RAM.
        let durable = match take_mount_seed(vol_tag) {
            Some(bits) => bits,
            None => {
                let mut bits = BlockBits::new(blocks);
                for kv in &routed.volumes {
                    for r in kv.block_ref_scan(vol_tag).await.map_err(|e| {
                        crate::error::SqueezefsError::InvalidOperation(format!(
                            "symmetric allocation arm: data volume '{}' ({vol_tag:#018x}): the \
                             durable reference scan on {} failed: {e}",
                            alloc.volume_id(),
                            kv.device_path().display()
                        ))
                    })? {
                        bits.set(r.block_idx);
                    }
                }
                bits
            }
        };
        if derived.highest == 0 && !durable.is_empty() {
            return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                "symmetric allocation arm: data volume '{}' ({vol_tag:#018x}): the allocator \
                 reports an UN-SEEDED derived state (cursor 0) while the set's durable \
                 reference ledger names {} block(s) of it — the mount-time by-block seed did \
                 not run; refusing to seed the allocation bitmap from an empty truth (every live \
                 block would be re-granted)",
                alloc.volume_id(),
                durable.population()
            )));
        }
        let seed = || (0..blocks).filter(|b| derived.is_set(*b) || durable.is_set(*b));
        let holding = acquire_and_hold(vol0, me, home_vol, vol_tag, blocks, &seed)
            .await
            .map_err(|e| {
                crate::error::SqueezefsError::InvalidOperation(format!(
                    "symmetric allocation arm: data volume '{}' ({vol_tag:#018x}): {e}",
                    alloc.volume_id()
                ))
            })?;
        let after = alloc.derived_allocation_snapshot();
        if after != derived {
            return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                "symmetric allocation arm: data volume '{}' ({vol_tag:#018x}): the allocator \
                 moved during the arm (cursor {} → {}, {} → {} set) — the seed is a snapshot \
                 taken before FUSE serves and nothing may allocate or free beside it; refusing \
                 to arm on a torn snapshot",
                alloc.volume_id(),
                derived.highest,
                after.highest,
                derived.population(),
                after.population()
            )));
        }
        let loss: Vec<u64> = seed().filter(|b| !holding.bitmap.is_set(*b)).collect();
        if !loss.is_empty() {
            let report = crate::data_alloc_bitmap::DriftReport {
                loss: loss.clone(),
                leak: Vec::new(),
            };
            crate::data_alloc_bitmap::note_drift(&report);
            return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                "symmetric allocation arm: data volume '{}' ({vol_tag:#018x}): {} block(s) the \
                 derived allocator holds LIVE read CLEAR in the recovered allocation bitmap \
                 (first: {:?}) — the loss direction; refusing to arm rather than re-grant them \
                 (data_alloc_bitmap_drift; fsck's C6/C8 census is the oracle)",
                alloc.volume_id(),
                loss.len(),
                loss.first()
            )));
        }
        // The bitmap IS the free list from here: the local list's blocks
        // read CLEAR in the bitmap and return through carves; the flat
        // free-list-first pass is gated on the armed allocator.
        let drained = alloc.drain_free_list_into_grants();
        let writer = crate::meta_ship::manager::wire_writer_name(&me.into());
        alloc.install_block_grant_arm(vol_tag, holder_block_grant_sink(vol0, vol_tag, writer));
        log::info!(
            "symmetric allocation arm: data volume '{}' — {} derived-live block(s) ({} durably \
             referenced) seeded/verified SET, {drained} free-listed block(s) drained into the \
             bitmap",
            alloc.volume_id(),
            derived.population(),
            durable.population()
        );
        log::info!(
            "symmetric allocation arm: data volume '{}' leased at term {} ({} block(s), pages \
             at {:?}); this mount mints from ranged block grants of its own holding",
            alloc.volume_id(),
            holding.term,
            blocks,
            holding.pages
        );
        held += 1;
    }
    Ok(held)
}

/// One data volume's acquire-and-hold ladder on the in-process manager.
async fn acquire_and_hold<I: Iterator<Item = u64>>(
    vol0: &Arc<KvMetaBackend>,
    me: AppenderIdentity,
    home_vol: u16,
    vol_tag: u64,
    blocks: u64,
    seed: &impl Fn() -> I,
) -> Result<Arc<AllocHolding>, KvError> {
    // Bounded: one same-node takeover + one deferred retry at most.
    for _attempt in 0..4 {
        match vol0
            .manager_alloc_lease_acquire(vol_tag, me, 0, home_vol, super::builder::ROOT_INO, blocks)
            .await
        {
            Ok(g) if g.already => {
                let rec = vol0.alloc_lease_record(vol_tag).await?.ok_or_else(|| {
                    KvError::Corrupt(format!(
                        "allocation lease of {vol_tag:#018x} answered `already` with no record"
                    ))
                })?;
                if rec.bitmap.is_empty() {
                    // Our own prior incarnation died between the acquire
                    // and the publish: seeded pages at the same term.
                    return hold_seeded_and_publish(
                        vol0,
                        me,
                        vol_tag,
                        rec.blocks,
                        rec.term,
                        seed(),
                    )
                    .await;
                }
                return vol0.rehold_alloc_lease(vol_tag, &rec).await;
            }
            Ok(g) if g.term == 1 => {
                return hold_seeded_and_publish(vol0, me, vol_tag, blocks, 1, seed()).await;
            }
            Ok(g) => {
                // A successor: copy the RECOVERED predecessor pages.
                let refs: Vec<ExtentRef> = g.predecessor_bitmap.iter().map(|(_, e)| *e).collect();
                let image = vol0
                    .read_alloc_bitmap_image(&refs, g.predecessor_blocks)
                    .await?;
                return hold_fresh_and_publish(
                    vol0,
                    me,
                    vol_tag,
                    g.predecessor_blocks,
                    g.term,
                    Some(&image),
                )
                .await;
            }
            Err(KvError::Busy(why)) => {
                let Some(rec) = vol0.alloc_lease_record(vol_tag).await? else {
                    return Err(KvError::Busy(why));
                };
                if !rec.holder.owned_by_node(me.node_token) {
                    return Err(KvError::Busy(format!(
                        "{why}; a live FOREIGN holder of a data volume this manager writes is \
                         PR 12's shape (a wire joiner's venue) — refusing to arm beside it"
                    )));
                }
                // A same-node predecessor of another mount slot: the D0
                // flock this open holds is the kernel's proof it is dead.
                // Record the death, recover its home (the window's deltas
                // onto its pages), record the recovery, then succeed it.
                let refs: Vec<ExtentRef> = rec.bitmap.iter().map(|(_, e)| *e).collect();
                vol0.record_death(rec.holder, crate::dlm::durable_term())
                    .await?;
                if !refs.is_empty() {
                    let image = vol0.read_alloc_bitmap_image(&refs, rec.blocks).await?;
                    let pages = DataAllocBitmap::from_region_image(vol_tag, rec.blocks, &image)?;
                    let changed = vol0.replay_data_alloc_deltas(&pages, rec.term).await?;
                    if changed > 0 {
                        let base = refs.first().map_or(0, |e| e.start);
                        pages
                            .write_dirty_pages(
                                vol0.device_path(),
                                base,
                                vol0.checkpoint_seq.load(Ordering::Acquire) + 1,
                            )
                            .await?;
                        vol0.sync_device().await.map_err(KvError::Io)?;
                    }
                }
                vol0.manager_record_recovered(rec.holder, rec.home_vol)
                    .await?;
                note_dead_member_acted(unix_now_ms());
            }
            Err(KvError::LeaseDeferred(why)) => {
                // A dead FOREIGN holder whose home is not yet recovered:
                // the recovery is PR 10's driver's, and the mount path is
                // not a retry loop — the deferral is the caller's (loud).
                return Err(KvError::LeaseDeferred(format!(
                    "{why}; the arm does not wait for a foreign recovery (PR 10's driver)"
                )));
            }
            Err(e) => return Err(e),
        }
    }
    Err(KvError::Busy(format!(
        "data volume {vol_tag:#018x}'s allocation lease could not be acquired after the \
         same-node takeover"
    )))
}

async fn hold_fresh_and_publish(
    vol0: &Arc<KvMetaBackend>,
    me: AppenderIdentity,
    vol_tag: u64,
    blocks: u64,
    term: u64,
    predecessor: Option<&[u8]>,
) -> Result<Arc<AllocHolding>, KvError> {
    let holding = vol0
        .hold_alloc_lease(vol_tag, blocks, term, predecessor)
        .await?;
    publish_holding(vol0, me, vol_tag, term, &holding).await?;
    Ok(holding)
}

/// The FIRST hold of a volume this identity has never held: the pages
/// seeded from the derived truth, then published.
async fn hold_seeded_and_publish(
    vol0: &Arc<KvMetaBackend>,
    me: AppenderIdentity,
    vol_tag: u64,
    blocks: u64,
    term: u64,
    seed: impl IntoIterator<Item = u64>,
) -> Result<Arc<AllocHolding>, KvError> {
    let holding = vol0
        .hold_alloc_lease_seeded(vol_tag, blocks, term, seed)
        .await?;
    publish_holding(vol0, me, vol_tag, term, &holding).await?;
    Ok(holding)
}

async fn publish_holding(
    vol0: &Arc<KvMetaBackend>,
    me: AppenderIdentity,
    vol_tag: u64,
    term: u64,
    holding: &AllocHolding,
) -> Result<(), KvError> {
    let refs: Vec<(u16, ExtentRef)> = holding
        .pages
        .iter()
        .map(|e| (crate::park_gate::home_volume(), *e))
        .collect();
    vol0.manager_alloc_lease_bitmap(vol_tag, me, term, refs)
        .await?;
    Ok(())
}
