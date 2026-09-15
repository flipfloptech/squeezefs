//! **Ranged block grants** — the armed symmetric plane's data allocation
//! (docs/design-symmetric-metadata.md §5.5 "Data-plane allocation",
//! KD-SYM-9; PR 8).
//!
//! The S9 residue-lane partition (`crate::alloc_lane_grant` /
//! `crate::data_alloc_lane`: writer `w` of `W` mints `b % W == w`) inherited
//! `MAX_LANES = 16` from the append-partition descriptor and stranded
//! `(W−1)/W` of every volume behind a power-of-two width. Under the
//! symmetric plane that partition RETIRES for the armed data plane: each
//! DATA volume has a floating **allocation lease** (§5.5.1 — the holder is
//! an ordinary mount) and every writer allocates from **ranged grants**
//! the holder carves — `BlockGrant { start, len }` — sized by
//!
//! `G = clamp(2 × ewma_blocks_per_s × T_renewal, 64, cap / (2 × writers))`
//!
//! where `T_renewal` is the membership beat (the grant is refreshed as
//! carriage — a top-up, never an expiry), `2 ×` is one EWMA window of
//! under-estimate, the floor 64 is one write-pipeline BDP window of 4 MiB
//! blocks, and the cap is half the volume spread over the writers (half
//! stays for reuse). **A grant's liveness is the writer's `dead_member`
//! status, never a TTL**: a parked member's (§5.5.3) already-granted
//! blocks stay its own until the death ledger says otherwise.
//!
//! Two halves live here:
//!
//! * [`BlockGrantLedger`] — the HOLDER's table for one data volume: every
//!   open grant by writer, carved from the volume's
//!   [`crate::data_alloc_bitmap::DataAllocBitmap`] (the bits are SET
//!   before the grant is answered — the `LaneReservation` law: a zombie may
//!   DMA into granted-but-unpublished offsets, so the recovery floor is the
//!   grant's END), and the `dead_member`-driven revocation whose remainder
//!   the S7 quarantine holds until a drain proof;
//! * [`GrantWindow`] — the WRITER's mint window: the grants it holds,
//!   consumed lowest-first by a CAS (a refused racer never advances it),
//!   with the 50 % top-up ask ([`GrantWindow::wants_topup`]) the refill
//!   cadence reads.
//!
//! The unarmed S9 co-writer path (`alloc_lane_grant`) is untouched and
//! byte-identical — pinned by `tests/sym_block_grant_tests.rs` and the S9
//! suites on both legs of the matrix. The re-homing of shipped frees to
//! the volume's allocation holder rides [`free_target_for`].

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// The grant floor, blocks: one write-pipeline BDP window of 4 MiB blocks
/// (`write_pipeline_depth_target`'s shipped floor posture — 256 MiB in
/// flight is what a single streaming writer keeps between two beats), so
/// a writer never asks the holder more than once per window at the norm.
pub const BLOCK_GRANT_FLOOR: u64 = 64;

/// One ranged block grant: `[start, start + len)` block indices of one
/// data volume.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct BlockGrant {
    pub start: u64,
    pub len: u32,
}

impl BlockGrant {
    /// One past the last granted block — the zombie floor.
    pub fn end(&self) -> u64 {
        self.start.saturating_add(u64::from(self.len))
    }

    /// `true` ⇔ `block` lies inside the grant.
    pub fn contains(&self, block: u64) -> bool {
        block >= self.start && block < self.end()
    }

    /// `true` ⇔ the two ranges share a block.
    pub fn overlaps(&self, other: &BlockGrant) -> bool {
        self.start < other.end() && other.start < self.end()
    }
}

/// `G` — the grant size the derivation answers (§5.5): `2 × ewma × T_renewal`
/// clamped to `[BLOCK_GRANT_FLOOR, cap / (2 × writers)]`, with the cap
/// never below the floor (a tiny volume grants the floor and lets the
/// carve truncate at what is free). `ewma_milli_blocks_per_s` is the
/// writer's measured allocation rate in milli-blocks/s (the allocator's
/// `rate_mblk_per_s` word); `writers` = the writers the holder knows
/// (≥ 1).
pub fn block_grant_derived(
    ewma_milli_blocks_per_s: u64,
    t_renewal_ms: u64,
    capacity_blocks: u64,
    writers: u64,
) -> u64 {
    // blocks/s × ms → blocks: (milli-blocks/s × ms) / 1000 / 1000.
    let want = ewma_milli_blocks_per_s
        .saturating_mul(2)
        .saturating_mul(t_renewal_ms)
        / 1_000_000;
    let cap = (capacity_blocks / (2 * writers.max(1))).max(BLOCK_GRANT_FLOOR);
    want.clamp(BLOCK_GRANT_FLOOR, cap)
}

// ---------------------------------------------------------------------------
// The holder's ledger
// ---------------------------------------------------------------------------

/// The outcome of a carve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CarveOutcome {
    /// A fresh range was carved and its bits set.
    Granted(BlockGrant),
    /// The writer's unconsumed grants already cover the ask — answered
    /// verbatim (§5.3.5, a replayed frame carves nothing).
    Already(Vec<BlockGrant>),
    /// No clear bit remains: the volume is full for this holder.
    Full,
}

/// One data volume's open grants as its allocation holder attests them.
/// Mutex-serialized (the grant cadence, never the write hot path); the
/// bits themselves are set on the lock-free bitmap.
#[derive(Debug, Default)]
pub struct BlockGrantLedger {
    grants: parking_lot::Mutex<BTreeMap<String, Vec<BlockGrant>>>,
    /// `block_grants` / `block_grant_blocks`.
    grants_issued: AtomicU64,
    blocks_granted: AtomicU64,
    /// Blocks returned (`ReturnBlocks`) — cleared in the bitmap.
    blocks_returned: AtomicU64,
    /// Grants revoked by a `dead_member` record.
    revoked: AtomicU64,
}

impl BlockGrantLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// **Carve up to `want` blocks for `writer`** from `bitmap`: the lowest
    /// run of clear bits at or above `floor` (the holder's fresh cursor —
    /// the bitmap's highest set bit + 1 for a holder that prefers fresh
    /// space, 0 to reuse the lowest hole), its bits SET here, before any
    /// reply. Idempotent against the ledger: a writer whose unconsumed
    /// grants (`held` = what the writer says it still holds) already cover
    /// `want` is answered them verbatim.
    pub fn carve(
        &self,
        bitmap: &crate::data_alloc_bitmap::DataAllocBitmap,
        writer: &str,
        want: u64,
        floor: u64,
        held_unconsumed: u64,
    ) -> CarveOutcome {
        let mut grants = self.grants.lock();
        if held_unconsumed >= want {
            let mine = grants.get(writer).cloned().unwrap_or_default();
            if !mine.is_empty() {
                return CarveOutcome::Already(mine);
            }
        }
        let Some((start, len)) = bitmap
            .first_clear_run(floor, want)
            .or_else(|| bitmap.first_clear_run(0, want))
        else {
            return CarveOutcome::Full;
        };
        let len32 = u32::try_from(len).unwrap_or(u32::MAX);
        let len = u64::from(len32);
        bitmap.set_run(start, len);
        let grant = BlockGrant { start, len: len32 };
        grants.entry(writer.to_string()).or_default().push(grant);
        self.grants_issued.fetch_add(1, Ordering::Relaxed);
        self.blocks_granted.fetch_add(len, Ordering::Relaxed);
        CarveOutcome::Granted(grant)
    }

    /// Record a grant the holder recovered from durable state (the
    /// successor's adoption of a predecessor's open grants — the bits are
    /// already set on the recovered pages).
    pub fn adopt(&self, writer: &str, grant: BlockGrant) {
        self.grants
            .lock()
            .entry(writer.to_string())
            .or_default()
            .push(grant);
    }

    /// **Return `[start, start+len)` of `writer`'s grant** (an unconsumed
    /// remainder, or a clean leave): the bits CLEAR in the bitmap, the
    /// range leaves the ledger. Returns the blocks cleared; a range the
    /// ledger does not grant to `writer` is refused with `None` (nothing
    /// cleared — a wire integer is never an allocation authority).
    pub fn return_blocks(
        &self,
        bitmap: &crate::data_alloc_bitmap::DataAllocBitmap,
        writer: &str,
        range: BlockGrant,
    ) -> Option<u64> {
        let mut grants = self.grants.lock();
        let mine = grants.get_mut(writer)?;
        let idx = mine
            .iter()
            .position(|g| range.start >= g.start && range.end() <= g.end())?;
        let g = mine[idx];
        // Split the covering grant around the returned range.
        let mut rest = Vec::new();
        if range.start > g.start {
            rest.push(BlockGrant {
                start: g.start,
                len: (range.start - g.start) as u32,
            });
        }
        if range.end() < g.end() {
            rest.push(BlockGrant {
                start: range.end(),
                len: (g.end() - range.end()) as u32,
            });
        }
        mine.remove(idx);
        mine.extend(rest);
        if mine.is_empty() {
            grants.remove(writer);
        }
        drop(grants);
        let cleared = bitmap.clear_run(range.start, u64::from(range.len));
        self.blocks_returned.fetch_add(cleared, Ordering::Relaxed);
        Some(cleared)
    }

    /// **Revoke every grant of a writer the death ledger names**: the
    /// grants leave the table and are returned for the caller to
    /// QUARANTINE (S7 — their unpublished remainder is a zombie's possible
    /// DMA target until a drain proof; their bits stay SET until the
    /// quarantine releases and clears them). A writer with no grant
    /// returns an empty list — the idempotent replay.
    pub fn revoke_dead(&self, writer: &str) -> Vec<BlockGrant> {
        let taken = self.grants.lock().remove(writer).unwrap_or_default();
        if !taken.is_empty() {
            self.revoked
                .fetch_add(taken.len() as u64, Ordering::Relaxed);
        }
        taken
    }

    /// `writer`'s open grants.
    pub fn grants_of(&self, writer: &str) -> Vec<BlockGrant> {
        self.grants.lock().get(writer).cloned().unwrap_or_default()
    }

    /// Every open grant as `(start, len)` ranges — the census's open-grant
    /// input.
    pub fn open_ranges(&self) -> Vec<(u64, u64)> {
        self.grants
            .lock()
            .values()
            .flatten()
            .map(|g| (g.start, u64::from(g.len)))
            .collect()
    }

    /// The highest granted block + 1 over every open grant — the zombie
    /// floor a successor carves above (`None` = no open grant).
    pub fn grant_frontier(&self) -> Option<u64> {
        self.grants
            .lock()
            .values()
            .flatten()
            .map(BlockGrant::end)
            .max()
    }

    /// Writers holding an open grant.
    pub fn writers(&self) -> Vec<String> {
        self.grants.lock().keys().cloned().collect()
    }

    /// `block_grants`.
    pub fn grants_issued(&self) -> u64 {
        self.grants_issued.load(Ordering::Relaxed)
    }

    /// `block_grant_blocks`.
    pub fn blocks_granted(&self) -> u64 {
        self.blocks_granted.load(Ordering::Relaxed)
    }

    /// Blocks returned through `ReturnBlocks`.
    pub fn blocks_returned(&self) -> u64 {
        self.blocks_returned.load(Ordering::Relaxed)
    }

    /// Grants revoked by the death ledger.
    pub fn revoked(&self) -> u64 {
        self.revoked.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// The writer's mint window
// ---------------------------------------------------------------------------

/// The writer's open grants on one data volume and its position inside
/// them. `next` is the CAS word every fresh mint advances; a refused mint
/// (the window exhausted) never moves it. The window is refilled by
/// APPENDING grants (a top-up), never by replacing one — a block already
/// handed out is inside a grant that stays in the list until it is fully
/// consumed.
#[derive(Debug, Default)]
pub struct GrantWindow {
    grants: parking_lot::Mutex<Vec<BlockGrant>>,
    /// Blocks consumed across every grant ever installed.
    consumed: AtomicU64,
    /// Blocks installed across every grant ever installed.
    installed: AtomicU64,
    /// The grant size the last top-up answered (the 50 % law's reference;
    /// 0 = nothing installed yet).
    reference: AtomicU64,
    /// Mints refused because the window was empty (the writer parked on a
    /// top-up).
    exhausted: AtomicU64,
}

impl GrantWindow {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a grant (the initial one or a top-up). Idempotent: a grant
    /// already in the window is not installed twice.
    pub fn install(&self, grant: BlockGrant) -> bool {
        let mut grants = self.grants.lock();
        if grants.iter().any(|g| *g == grant || g.overlaps(&grant)) {
            return false;
        }
        grants.push(grant);
        grants.sort_unstable();
        self.installed
            .fetch_add(u64::from(grant.len), Ordering::Relaxed);
        self.reference
            .fetch_max(u64::from(grant.len), Ordering::Relaxed);
        true
    }

    /// Mint the next unconsumed block, lowest grant first. `None` ⇔ the
    /// window is empty (the caller asks for a top-up and retries, or
    /// refuses `StorageFull`-class).
    pub fn mint(&self) -> Option<u64> {
        let mut grants = self.grants.lock();
        let g = grants.first_mut()?;
        let block = g.start;
        g.start += 1;
        g.len -= 1;
        if g.len == 0 {
            grants.remove(0);
        }
        self.consumed.fetch_add(1, Ordering::Relaxed);
        Some(block)
    }

    /// Blocks still unconsumed in the window.
    pub fn remaining(&self) -> u64 {
        self.grants.lock().iter().map(|g| u64::from(g.len)).sum()
    }

    /// The unconsumed ranges — what the writer returns at a clean leave
    /// and what its page/lease attests as held.
    pub fn unconsumed(&self) -> Vec<BlockGrant> {
        self.grants.lock().clone()
    }

    /// Take every unconsumed range out of the window (the leave).
    pub fn drain(&self) -> Vec<BlockGrant> {
        std::mem::take(&mut *self.grants.lock())
    }

    /// The 50 % refill law (the extent grant's own): a top-up is wanted
    /// once the remainder falls below half the last grant's size — and
    /// always when the window is empty.
    pub fn wants_topup(&self) -> bool {
        let reference = self.reference.load(Ordering::Relaxed);
        self.remaining() * 2 < reference || reference == 0
    }

    /// Count an exhausted mint.
    pub fn note_exhausted(&self) {
        self.exhausted.fetch_add(1, Ordering::Relaxed);
    }

    /// Blocks consumed / installed / mints refused empty.
    pub fn consumed(&self) -> u64 {
        self.consumed.load(Ordering::Relaxed)
    }

    /// See [`Self::consumed`].
    pub fn installed(&self) -> u64 {
        self.installed.load(Ordering::Relaxed)
    }

    /// See [`Self::consumed`].
    pub fn exhausted(&self) -> u64 {
        self.exhausted.load(Ordering::Relaxed)
    }
}

/// The writer's top-up sink: asked for `want` more blocks on this volume,
/// answers the grant the holder carved (in-process: the holder's ledger
/// directly; over the wire: `ManagerCall::BlockGrant`). `None` = the
/// holder had nothing (full, or unreachable — the caller refuses
/// `StorageFull`-class, never `ENOSPC`-poisons anything).
pub type BlockGrantSink = Arc<
    dyn Fn(u64) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<BlockGrant>> + Send>>
        + Send
        + Sync,
>;

// ---------------------------------------------------------------------------
// The free target: which holder receives a data volume's terminal frees
// ---------------------------------------------------------------------------

static FREE_TARGETS: once_cell::sync::Lazy<scc::HashMap<u64, String>> =
    once_cell::sync::Lazy::new(scc::HashMap::new);

/// Register (or move) the allocation holder of data volume `vol_tag` —
/// the endpoint a co-writer ships that volume's terminal frees to
/// (`execute_shipped_frees` runs there verbatim). Learned from
/// `alloc_lease:{vol_tag}` at every projection refresh.
pub fn install_free_target(vol_tag: u64, endpoint: String) {
    let _ = FREE_TARGETS.upsert_sync(vol_tag, endpoint);
}

/// Forget a volume's holder (its lease released / the plane dropped).
pub fn uninstall_free_target(vol_tag: u64) {
    let _ = FREE_TARGETS.remove_sync(&vol_tag);
}

/// The allocation holder's endpoint for `vol_tag`, when the plane knows
/// one — the caller falls back to the set authority (the shipped route)
/// otherwise, so an unarmed mount pays one probe of an empty map.
pub fn free_target_for(vol_tag: u64) -> Option<String> {
    FREE_TARGETS.read_sync(&vol_tag, |_, v| v.clone())
}

/// Test seam: clear every registered target.
pub fn test_clear_free_targets() {
    FREE_TARGETS.clear_sync();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_alloc_bitmap::DataAllocBitmap;

    #[test]
    fn the_derivation_clamps_between_the_floor_and_half_the_volume_per_writer() {
        assert_eq!(
            block_grant_derived(0, 10_000, 1 << 20, 4),
            BLOCK_GRANT_FLOOR
        );
        // 100 blocks/s × 10 s × 2 = 2,000 blocks.
        assert_eq!(block_grant_derived(100_000, 10_000, 1 << 20, 4), 2_000);
        // Cap: 1,024 / (2 × 4) = 128.
        assert_eq!(block_grant_derived(100_000, 10_000, 1_024, 4), 128);
        // A cap below the floor is the floor.
        assert_eq!(
            block_grant_derived(100_000, 10_000, 100, 4),
            BLOCK_GRANT_FLOOR
        );
    }

    #[test]
    fn grants_are_disjoint_and_the_window_mints_lowest_first() {
        let bm = DataAllocBitmap::new(9, 1_000);
        let ledger = BlockGrantLedger::new();
        let CarveOutcome::Granted(a) = ledger.carve(&bm, "a", 64, 0, 0) else {
            panic!()
        };
        let CarveOutcome::Granted(b) = ledger.carve(&bm, "b", 64, 0, 0) else {
            panic!()
        };
        assert!(!a.overlaps(&b));
        assert_eq!(bm.population(), 128);
        let w = GrantWindow::new();
        assert!(w.install(b));
        assert!(!w.install(b));
        assert_eq!(w.mint(), Some(b.start));
        assert_eq!(w.remaining(), 63);
        assert_eq!(ledger.return_blocks(&bm, "a", a), Some(64));
        assert_eq!(bm.population(), 64);
        assert!(ledger.return_blocks(&bm, "a", a).is_none());
        assert_eq!(ledger.revoke_dead("b"), vec![b]);
        assert!(ledger.revoke_dead("b").is_empty());
    }
}
