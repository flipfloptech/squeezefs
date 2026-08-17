//! DLM **S11 rung 17** — the extent-ship client half and the authority's
//! coverage ledger (KD-MW-8 revised, `docs/design-full-multi-writer.md`
//! §9.3; PR-plan row 17).
//!
//! # The retention law (the peer's half)
//!
//! A sub-block writer of a SHARED block ships its bytes to the AUTHORITY
//! as [`crate::meta_ship::publish::PublishCall::WriteExtent`] records and
//! **retains each extent until the layout version covering it is
//! visible**. An assembler-side ACK alone never releases retention —
//! otherwise an authority death between ack and publish loses bytes the
//! application saw succeed (MW-11). Retention rides the W2 budget's
//! gauge (`parked_extent_bytes`), and release is **pull only** — the four
//! paths, each pinned red-first in `tests/mw_authority_assembler_tests.rs`:
//!
//! 1. **ack-carried `covering_version`** — `Some` iff the covering
//!    publish already ran (the extent rode or trailed a flush-forced
//!    publish): released at the round trip;
//! 2. **renewal observation** — the custody renewal reply carries the
//!    per-ino covered watermark (`extent_covered`), computed from the
//!    authority's own coverage ledger;
//! 3. **`FlushExtents`** — the synchronous fsync force: the reply carries
//!    the covering version and fsync returns only after release;
//! 4. **the at-budget W2 spill** — retention over its derived budget
//!    spills the oldest extents to the local-durable record sink (the
//!    spilled extent stays LOGICALLY retained — the record IS the
//!    retention — but leaves the RAM gauge; the writer is never blocked).
//!
//! # The owner half
//!
//! The authority's [`Self`]-side ledger tracks, per `(client, ino)`, the
//! highest merged witness id and the highest COVERED one (advanced by the
//! flush force and by the assembler's natural fold-publish completions).
//! The renewal reply reads [`owner_covered_watermarks`]; the ids are the
//! `(lease_epoch, request_id)` witness ids the wire already carries, so
//! coverage visibility is exact without guessing a future version.
//!
//! Everything here is structurally inert on a shipped mount: no client
//! is installed, no extent is ever retained, every probe is one
//! relaxed-load/empty-map check.

use crate::error::Result;
use crate::meta_backend::RoutedMetaBackend;
use once_cell::sync::Lazy;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// The client retention store
// ---------------------------------------------------------------------------

/// One retained extent — the client's proof-holdable copy of a shipped
/// sub-block write, alive until its covering layout version is visible.
#[derive(Clone, Debug)]
struct Retained {
    block_index: u64,
    offset_in_block: u32,
    /// The absolute file offset (the read-overlay key).
    abs: u64,
    data: bytes::Bytes,
    request_id: u64,
    /// Spilled to the local-durable record sink: the bytes left the RAM
    /// gauge but the extent stays logically retained.
    spilled: bool,
}

static RETAINED: Lazy<scc::HashMap<u64, Vec<Retained>>> = Lazy::new(scc::HashMap::new);
/// RAM bytes currently retained (spilled extents excluded) — the budget
/// arithmetic's input and the `extent_retained_bytes` gauge.
static RETAINED_BYTES: AtomicU64 = AtomicU64::new(0);
/// Fast gate for the read-overlay / flush probes (0 on every shipped
/// mount by construction).
static RETAINED_COUNT: AtomicU64 = AtomicU64::new(0);

/// The at-budget override seam (the `test_swap_range_table_budget`
/// precedent; production never calls it).
static RETENTION_BUDGET_OVERRIDE: AtomicU64 = AtomicU64::new(0);

/// **Test seam**: swap the retention byte budget.
pub fn test_swap_retention_budget(bytes: Option<u64>) -> Option<u64> {
    let prev = RETENTION_BUDGET_OVERRIDE.swap(bytes.unwrap_or(0), Ordering::AcqRel);
    (prev != 0).then_some(prev)
}

/// The retention byte budget: derived `R5 budget / 128`, floor 8 MiB —
/// a sliver of the memory budget for an exception-path store (D1:
/// sub-block sharing is rare), floored so a small-RAM box still holds a
/// few blocks' worth of extents before spilling. No env knob; the seam
/// above is a test seam.
pub fn retention_budget_bytes() -> u64 {
    let over = RETENTION_BUDGET_OVERRIDE.load(Ordering::Acquire);
    if over != 0 {
        return over;
    }
    (crate::mem_budget::MEM_BUDGET.budget_bytes() / 128).max(8 * 1024 * 1024)
}

/// The retained RAM bytes (the `extent_retained_bytes` stats row — → 0
/// at quiesce, falsifiable against all four release paths).
pub fn retained_bytes() -> u64 {
    RETAINED_BYTES.load(Ordering::Relaxed)
}

/// Retained extents (RAM + spilled) for `ino`.
pub fn retained_count(ino: u64) -> usize {
    RETAINED.read_sync(&ino, |_, v| v.len()).unwrap_or(0)
}

/// Any retention at all? (One relaxed load — the fsync/read gates.)
pub fn any_retained() -> bool {
    RETAINED_COUNT.load(Ordering::Relaxed) != 0
}

// ---------------------------------------------------------------------------
// The hooks (installed by the mount arm; test seams in the pins)
// ---------------------------------------------------------------------------

/// The at-budget spill sink: `(ino, block_index, offset_in_block, data)`
/// → the W2 `active_block_ext:` record write (local-durable, versioned +
/// fencing-stamped — the record IS the retention).
pub type SpillSink = Arc<dyn Fn(u64, u64, u32, bytes::Bytes) -> Result<()> + Send + Sync>;

static SPILL_SINK: Lazy<arc_swap::ArcSwapOption<SpillSink>> =
    Lazy::new(arc_swap::ArcSwapOption::empty);

/// Install the spill sink (mount arm / test).
pub fn install_spill_sink(sink: SpillSink) {
    SPILL_SINK.store(Some(Arc::new(sink)));
}

/// Uninstall it (unmount / test teardown).
pub fn uninstall_spill_sink() {
    SPILL_SINK.store(None);
}

/// The demotion quiesce hook: drain the ino's in-flight direct publishes
/// BEFORE the ack travels (the §9.3 order: mark local → quiesce → ack).
/// The mount arm installs the real drain (the write path's own lock
/// chain); a missing hook quiesces nothing — honest for wire-only tests.
pub type QuiesceHook = Arc<dyn Fn(u64) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

static QUIESCE: Lazy<arc_swap::ArcSwapOption<QuiesceHook>> =
    Lazy::new(arc_swap::ArcSwapOption::empty);

/// Install the quiesce hook (mount arm / test).
pub fn install_quiesce_hook(hook: QuiesceHook) {
    QUIESCE.store(Some(Arc::new(hook)));
}

/// Uninstall it.
pub fn uninstall_quiesce_hook() {
    QUIESCE.store(None);
}

/// The release notifier: an ino whose retention just released may serve
/// stale cached layout — the mount arm installs a metadata-cache
/// invalidation so the next read refetches the covering publish.
pub type ReleaseHook = Arc<dyn Fn(u64) + Send + Sync>;

static RELEASE_HOOK: Lazy<arc_swap::ArcSwapOption<ReleaseHook>> =
    Lazy::new(arc_swap::ArcSwapOption::empty);

/// Install the release notifier (mount arm).
pub fn install_release_hook(hook: ReleaseHook) {
    RELEASE_HOOK.store(Some(Arc::new(hook)));
}

/// Uninstall it.
pub fn uninstall_release_hook() {
    RELEASE_HOOK.store(None);
}

fn note_released(ino: u64) {
    if let Some(hook) = RELEASE_HOOK.load_full() {
        hook(ino);
    }
}

// ---------------------------------------------------------------------------
// Retain / spill / release
// ---------------------------------------------------------------------------

fn gauge_add(bytes: u64) {
    RETAINED_BYTES.fetch_add(bytes, Ordering::Relaxed);
    crate::fuse_client::METRICS
        .parked_extent_bytes
        .fetch_add(bytes, Ordering::Relaxed);
}

fn gauge_sub(bytes: u64) {
    crate::gauge_core::sub_saturating(&RETAINED_BYTES, bytes);
    crate::gauge_core::sub_saturating(&crate::fuse_client::METRICS.parked_extent_bytes, bytes);
}

/// The at-budget arm: spill the OLDEST un-spilled retained extents until
/// the store fits `incoming` more bytes. Spilled extents stay logically
/// retained; a missing sink keeps them in RAM (loud once — never block
/// the writer on bookkeeping).
fn spill_to_fit(incoming: u64) {
    let budget = retention_budget_bytes();
    if RETAINED_BYTES.load(Ordering::Relaxed) + incoming <= budget {
        return;
    }
    let Some(sink) = SPILL_SINK.load_full() else {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            log::warn!(
                "extent retention is past its budget with no spill sink installed — retained \
                 extents stay in RAM (the writer is never blocked; the mount arm installs the \
                 W2 record sink)"
            );
        });
        return;
    };
    let mut inos: Vec<u64> = Vec::new();
    RETAINED.iter_sync(|ino, _| {
        inos.push(*ino);
        true
    });
    for ino in inos {
        if RETAINED_BYTES.load(Ordering::Relaxed) + incoming <= budget {
            return;
        }
        let _ = RETAINED.update_sync(&ino, |_, list| {
            for r in list.iter_mut() {
                if r.spilled {
                    continue;
                }
                if RETAINED_BYTES.load(Ordering::Relaxed) + incoming <= budget {
                    break;
                }
                match (sink)(ino, r.block_index, r.offset_in_block, r.data.clone()) {
                    Ok(()) => {
                        r.spilled = true;
                        gauge_sub(r.data.len() as u64);
                        crate::meta_ship::publish::note_extent_spill();
                    }
                    Err(e) => {
                        log::warn!(
                            "extent spill for ino {ino} block {} failed ({e}) — the extent \
                             stays in RAM (never dropped: it is retained custody)",
                            r.block_index
                        );
                        break;
                    }
                }
            }
        });
    }
}

fn retain(
    ino: u64,
    block_index: u64,
    offset_in_block: u32,
    abs: u64,
    data: bytes::Bytes,
    request_id: u64,
) {
    spill_to_fit(data.len() as u64);
    gauge_add(data.len() as u64);
    RETAINED_COUNT.fetch_add(1, Ordering::Relaxed);
    let entry = Retained {
        block_index,
        offset_in_block,
        abs,
        data,
        request_id,
        spilled: false,
    };
    match RETAINED.entry_sync(ino) {
        scc::hash_map::Entry::Occupied(mut occ) => occ.get_mut().push(entry),
        scc::hash_map::Entry::Vacant(vac) => {
            let _ = vac.insert_entry(vec![entry]);
        }
    }
}

fn remove_where(ino: u64, mut pred: impl FnMut(&Retained) -> bool) -> usize {
    let mut removed = 0usize;
    let mut vacate = false;
    let _ = RETAINED.update_sync(&ino, |_, list| {
        list.retain(|r| {
            if pred(r) {
                if !r.spilled {
                    gauge_sub(r.data.len() as u64);
                }
                removed += 1;
                false
            } else {
                true
            }
        });
        vacate = list.is_empty();
    });
    if vacate {
        let _ = RETAINED.remove_if_sync(&ino, |v| v.is_empty());
    }
    if removed > 0 {
        crate::gauge_core::sub_saturating(&RETAINED_COUNT, removed as u64);
        note_released(ino);
    }
    removed
}

/// Release every retained extent of `ino` with `request_id ≤ upto` —
/// release path 2's apply (the renewal-carried watermark).
pub fn release_covered(ino: u64, upto_request_id: u64) -> usize {
    remove_where(ino, |r| r.request_id <= upto_request_id)
}

// ---------------------------------------------------------------------------
// Ship / flush / re-ship
// ---------------------------------------------------------------------------

/// Ship ONE sub-block extent of a shared block to its authority and
/// RETAIN it until coverage (KD-MW-8's symmetric law: both holders ship;
/// the write path calls this for every shared-block slice). Returns after
/// the assembler's ack; an ack carrying `Some(covering_version)` releases
/// at the round trip (path 1).
pub async fn ship_extent(
    be: &Arc<RoutedMetaBackend>,
    ino: u64,
    block_index: u64,
    offset_in_block: u32,
    data: bytes::Bytes,
    token: u64,
) -> Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    let request_id = crate::cowriter::next_ship_request_id();
    // The read-overlay key: the mount's live block geometry (one relaxed
    // load; extents are minted against the same geometry the write path
    // split them with).
    let abs = block_index
        .saturating_mul(crate::routing::default_block_size())
        .saturating_add(u64::from(offset_in_block));
    retain(
        ino,
        block_index,
        offset_in_block,
        abs,
        data.clone(),
        request_id,
    );
    match crate::meta_ship::publish::write_extent(
        be,
        ino,
        block_index,
        offset_in_block,
        data.to_vec(),
        token,
        request_id,
    )
    .await
    {
        Ok(covering) => {
            if covering.is_some() {
                // Path 1: the covering publish already ran.
                remove_where(ino, |r| r.request_id == request_id);
            }
            Ok(())
        }
        Err(e) => {
            // The ship failed: the write fails loud and the bytes were
            // never acked — nothing to retain (retention protects ACKED
            // custody, MW-10/11).
            remove_where(ino, |r| r.request_id == request_id);
            Err(e)
        }
    }
}

/// The fsync force (release path 3): ship `FlushExtents`, await the
/// covering version, release every retained extent of `ino`. Chains the
/// caller's fsync through the AUTHORITY's publish barrier — the
/// shipped-free precedent's synchronous form. `Ok(0)` with no retention
/// and no RPC.
pub async fn flush_ino(be: &Arc<RoutedMetaBackend>, ino: u64) -> Result<u64> {
    if retained_count(ino) == 0 {
        return Ok(0);
    }
    let covering = crate::meta_ship::publish::flush_extents(be, ino).await?;
    remove_where(ino, |_| true);
    Ok(covering)
}

/// MW-11's re-ship: after an authority failover (re-join, fresh lease
/// epoch), every retained extent re-ships — content-idempotent (the
/// bytes are byte-disjoint custody; a duplicate merge is newest-wins of
/// identical content), under FRESH witness ids (a new epoch is a new
/// act; retries never re-key WITHIN an epoch, and this is not a retry —
/// it is the retention law discharging). Returns the re-shipped count.
pub async fn reship_all(be: &Arc<RoutedMetaBackend>) -> Result<usize> {
    let mut work: Vec<(u64, Retained)> = Vec::new();
    RETAINED.iter_sync(|ino, list| {
        for r in list {
            work.push((*ino, r.clone()));
        }
        true
    });
    let mut shipped = 0usize;
    for (ino, r) in work {
        let request_id = crate::cowriter::next_ship_request_id();
        crate::meta_ship::publish::write_extent(
            be,
            ino,
            r.block_index,
            r.offset_in_block,
            r.data.to_vec(),
            0,
            request_id,
        )
        .await?;
        // The retained record re-keys to the id the NEW authority can
        // cover (the old id names a dead window).
        let _ = RETAINED.update_sync(&ino, |_, list| {
            if let Some(entry) = list.iter_mut().find(|e| e.request_id == r.request_id) {
                entry.request_id = request_id;
            }
        });
        shipped += 1;
    }
    Ok(shipped)
}

/// The §9.3 demotion notice's client handler: mark the region demoted in
/// the LOCAL custody table (subsequent writes classify extent-ship),
/// QUIESCE the ino's in-flight direct publishes, and only then may the
/// caller ack (the order that keeps a single publisher at every
/// instant).
pub async fn note_demotion(ino: u64, region: (u64, u64)) {
    let marked = crate::dlm::adopt_demoted_region(ino, region);
    if !marked {
        log::warn!(
            "demotion notice for ino {ino} {region:?} named custody this mount no longer \
             holds — nothing to re-route (the grant died underneath the notice)"
        );
    }
    if let Some(hook) = QUIESCE.load_full() {
        hook(ino).await;
    }
}

/// Overlay this mount's retained extents onto a read of
/// `[offset, offset + buf.len())` — read-your-writes for bytes whose
/// covering publish has not landed yet. One relaxed load on every mount
/// with no retention. Returns whether anything applied.
pub fn overlay_retained(ino: u64, offset: u64, buf: &mut [u8]) -> bool {
    if !any_retained() || buf.is_empty() {
        return false;
    }
    let end = offset + buf.len() as u64;
    let mut applied = false;
    let _ = RETAINED.read_sync(&ino, |_, list| {
        for r in list {
            let r_end = r.abs + r.data.len() as u64;
            let s = r.abs.max(offset);
            let e = r_end.min(end);
            if s >= e {
                continue;
            }
            let src = &r.data[(s - r.abs) as usize..(e - r.abs) as usize];
            buf[(s - offset) as usize..(e - offset) as usize].copy_from_slice(src);
            applied = true;
        }
    });
    applied
}

// ---------------------------------------------------------------------------
// The owner coverage ledger
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
struct OwnerCoverage {
    /// The highest witness id merged into the assembler for this
    /// `(client, ino)`.
    merged_upto: u64,
    /// The highest witness id whose covering publish has COMMITTED.
    covered_upto: u64,
    /// The covering publish's version (diagnostics; the watermark is the
    /// release key).
    covering_version: u64,
}

static OWNER_LEDGER: Lazy<scc::HashMap<(String, u64), OwnerCoverage>> =
    Lazy::new(scc::HashMap::new);

/// Record one merged extent (the assembler serve's bookkeeping).
pub fn owner_note_merge(client: &str, ino: u64, request_id: u64) {
    let key = (client.to_string(), ino);
    match OWNER_LEDGER.entry_sync(key) {
        scc::hash_map::Entry::Occupied(mut occ) => {
            let cov = occ.get_mut();
            cov.merged_upto = cov.merged_upto.max(request_id);
        }
        scc::hash_map::Entry::Vacant(vac) => {
            let _ = vac.insert_entry(OwnerCoverage {
                merged_upto: request_id,
                ..OwnerCoverage::default()
            });
        }
    }
}

/// The covering publish for `ino` COMMITTED at `version`: every merged
/// extent is covered — advance every client's watermark (the flush
/// force's and the assembler fold's completion hook).
pub fn owner_note_covered(ino: u64, version: u64) {
    let mut keys: Vec<(String, u64)> = Vec::new();
    OWNER_LEDGER.iter_sync(|k, _| {
        if k.1 == ino {
            keys.push(k.clone());
        }
        true
    });
    for k in keys {
        let _ = OWNER_LEDGER.update_sync(&k, |_, cov| {
            cov.covered_upto = cov.merged_upto;
            cov.covering_version = cov.covering_version.max(version);
        });
    }
}

/// The renewal reply's coverage read: `(ino, covered_upto_request_id)`
/// for every ino this client has covered extents on. Pull-only — the
/// client releases retention against these watermarks (path 2).
pub fn owner_covered_watermarks(client: &str) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    OWNER_LEDGER.iter_sync(|k, cov| {
        if k.0 == client && cov.covered_upto > 0 {
            out.push((k.1, cov.covered_upto));
        }
        true
    });
    out
}

/// **Test seam**: clear every client- and owner-side rung-17 static (the
/// suite's posture restoration).
pub fn test_reset() {
    let mut inos: Vec<u64> = Vec::new();
    RETAINED.iter_sync(|ino, _| {
        inos.push(*ino);
        true
    });
    for ino in inos {
        remove_where(ino, |_| true);
    }
    RETAINED_BYTES.store(0, Ordering::Release);
    RETAINED_COUNT.store(0, Ordering::Release);
    OWNER_LEDGER.retain_sync(|_, _| false);
}
