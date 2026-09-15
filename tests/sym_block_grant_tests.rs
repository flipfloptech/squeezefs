//! Symmetric metadata program, PR 8 — **the allocation lease, ranged block
//! grants, the data allocation bitmap, park-and-reclaim**
//! (`docs/design-symmetric-metadata.md` §5.5 the singular planes made
//! per-volume, §5.5.1 the data allocation bitmap + its crash table, §5.5.2
//! the death ledger's RECORD half, §5.5.3 the symmetric appender's `T_self`
//! = PARK, §5.3.5 idempotent verbs, §5.9 the successor's order, §6.3, §11
//! the Allocation-lease family; KD-SYM-9/15).
//!
//! Under `SQUEEZEFS_SYMMETRIC_META=1` on a bit-17 volume: every DATA volume
//! has a floating allocation lease recorded in tree 0 of volume 0
//! (`alloc_lease:{vol_tag}`), its holder carves RANGED block grants from a
//! durable per-volume bitmap co-located with its ring (bits SET and
//! journaled BEFORE the grant is answered; CLEARED at the terminal free),
//! a successor is granted the lease ONLY after the dead holder's home
//! region is `Recovered` (the ordering law — `recovered:` written by the
//! seam here, by PR 10's driver later), a symmetric appender past `T_self`
//! PARKS (acks held, admission waiting, nothing poisoned) and reclaims
//! under the successor's grace, and the maintenance coordinator is volume
//! 0's manager with each mount judging the inode plane over the slots it
//! leases.
//!
//! **`SQUEEZEFS_SYMMETRIC_META=0` is the shipped posture exactly**: no
//! holding, no park posture, the S9 lane path byte-identical.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::block_grant::{
    block_grant_derived, free_target_for, install_free_target, test_clear_free_targets,
    uninstall_free_target, BlockGrant, BlockGrantLedger, CarveOutcome, GrantWindow,
    BLOCK_GRANT_FLOOR,
};
use squeezefs::data_alloc_bitmap::{
    clear_record, decode_data_alloc_page, encode_data_alloc_page, note_drift, set_record,
    DataAllocBitmap, DATA_ALLOC_BITMAP_DRIFT, DATA_ALLOC_PAGE_LEN,
};
use squeezefs::membership::{
    self, member_renewal_tick, JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole,
    MemberSession, MembershipOwner, RenewalTick,
};
use squeezefs::membership_sim::{run_sharded, SimConfig, SimMode};
use squeezefs::membership_wire::{MemberClient, MembershipPlane, MembershipPlaneConfig};
use squeezefs::meta_backend::kv::alloc_lease::{
    self, dead_member_key, holdings, recovered_key, register_data_volume_blocks,
    test_clear_holdings, AllocLeaseRecord,
};
use squeezefs::meta_backend::kv::appender::AppenderIdentity;
use squeezefs::meta_backend::kv::backend::{
    test_conveyor_hold_release, KvMetaBackend, TEST_CONVEYOR_HOLD_PRE_FANOUT,
    TEST_CONVEYOR_HOLD_STAGE,
};
use squeezefs::meta_backend::kv::builder::{format_v3_stamped, FormatV3Options, ROOT_INO};
use squeezefs::meta_backend::kv::checkpoint::TEST_CHECKPOINT_HALT_AFTER_LEDGER;
use squeezefs::meta_backend::kv::slot_lease::SYMMETRIC_META_ENV;
use squeezefs::meta_backend::kv::superblock::ExtentRef;
use squeezefs::meta_backend::kv::{KvError, META_KV_CHECKPOINTS};
use squeezefs::meta_backend::{
    open_routed_meta_set, open_routed_meta_set_read_only, plan_meta_slot_set, Metadata,
    RoutedMetaBackend,
};
use squeezefs::meta_ship::manager::{
    decode_reply, decode_request, encode_reply, encode_request, wire_writer_name, ManagerCall,
    ManagerClient, ManagerReply, ManagerReplyFrame, ManagerRequestFrame, ManagerService,
    WireIdentity, MANAGER_SCHEMA, VERB_CODE_BLOCK_GRANT, VERB_CODE_RECORD_DEATH,
};
use squeezefs::park_gate::{self, FenceClass, TSelfAction};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Harness (the sym_slot_transfer_tests shape: 64 KiB nodes, a 1 MiB ring).
// ---------------------------------------------------------------------------

const VOL_LEN: u64 = 64 * 1024 * 1024;
const NODE_SIZE: usize = 64 * 1024;
const RING_LEN: u64 = 1024 * 1024;
/// The DATA volume the contracts lease: 4,096 blocks (16 GiB at 4 MiB) —
/// one bitmap page.
const DATA_BLOCKS: u64 = 4096;
const DATA_TAG: u64 = 0xD0DA_0000_0000_0001;
/// The durable id whose `volume_tag` is `DATA_TAG` (KD-5).
const DATA_ID: &str = "vol-d0da000000000001";

/// Every seam here is process-global; the contracts serialize on it.
static SEAM: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn set_opts() -> FormatV3Options {
    FormatV3Options {
        node_size: NODE_SIZE,
        journal_len_override: Some(RING_LEN),
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

async fn format_stamped_member(dir: &std::path::Path, name: &str) -> String {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
    let r = format_v3_stamped(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone()).await;
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    r.expect("format stamped member");
    p.display().to_string()
}

async fn format_flat_member(dir: &std::path::Path, name: &str) -> String {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    format_v3_stamped(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone())
        .await
        .expect("format flat member");
    p.display().to_string()
}

/// A stamped N-volume set under ONE derived plan (review round 1, Issue
/// 3: "every contract mounts one volume" — the design's shape is N
/// volumes with every appender homed on the slot-0 volume, and the
/// second volume is what exposed the misdirected page write).
async fn format_stamped_set(dir: &std::path::Path, n: usize) -> Vec<String> {
    let plan = plan_meta_slot_set(n).expect("derived plan");
    let mut uris = Vec::with_capacity(n);
    for (i, stamp) in plan.stamps.iter().enumerate() {
        let p = dir.join(format!("meta{i}"));
        std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
        std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
        let r = format_v3_stamped(&p, VOL_LEN, &set_opts(), stamp.clone()).await;
        std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
        r.expect("format stamped member");
        uris.push(p.display().to_string());
    }
    uris
}

/// The slot-0 volume's ordinal — the coordinator's home, every appender's
/// home volume until PR 12's join ladder.
fn slot0_of(routed: &RoutedMetaBackend) -> usize {
    routed.route_ino(1).0
}

/// Reset every process-global the contracts touch (the holdings, the free
/// targets, the park gate, custody) — a fresh mount in one binary.
fn reset_process_state() {
    test_clear_holdings();
    squeezefs::data_alloc_bitmap::test_clear_replayed_deltas();
    test_clear_free_targets();
    park_gate::test_reset();
    squeezefs::data_custody::test_clear_poison();
    TEST_CHECKPOINT_HALT_AFTER_LEDGER.store(false, Ordering::SeqCst);
    std::env::remove_var("SQUEEZEFS_TIMEOUT");
    // The manager's derived-state witness for the contracts' data volume
    // (review round 1, Issue 6): a wire `blocks` is validated against it.
    register_data_volume_blocks(DATA_TAG, DATA_BLOCKS);
}

/// A data volume's allocator for the contracts: `DATA_BLOCKS` blocks of
/// the shipped 4 MiB chunk, tagged `DATA_TAG` (`volume_tag` of its id).
async fn data_allocator(id: &str) -> Arc<BlockAllocator> {
    let a = Arc::new(BlockAllocator::new(id).await.expect("allocator"));
    a.set_capacity_bytes(DATA_BLOCKS * a.chunk_size());
    a
}

async fn open_armed(uris: &[String]) -> Arc<RoutedMetaBackend> {
    std::env::set_var(SYMMETRIC_META_ENV, "1");
    std::env::set_var("SQUEEZEFS_SYM_ALLOW_NON_PR", "1");
    let r = open_routed_meta_set(uris).await;
    std::env::remove_var(SYMMETRIC_META_ENV);
    std::env::remove_var("SQUEEZEFS_SYM_ALLOW_NON_PR");
    r.expect("open armed set")
}

async fn open_unarmed(uris: &[String]) -> Arc<RoutedMetaBackend> {
    std::env::remove_var(SYMMETRIC_META_ENV);
    open_routed_meta_set(uris).await.expect("open unarmed set")
}

async fn shutdown(routed: &RoutedMetaBackend) {
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

fn identity_of(vol: &KvMetaBackend) -> AppenderIdentity {
    vol.appenders_public().expect("a forest volume").identity
}

fn successor_identity() -> AppenderIdentity {
    AppenderIdentity {
        node_token: 0x5ECC_0000_0000_0002,
        mount_slot: 0x2002,
        writer_id: 0xBEEF_0002,
    }
}

fn wire(id: AppenderIdentity) -> WireIdentity {
    id.into()
}

/// A first holder: acquire at term 1, hold (fresh pages), publish the refs.
async fn take_fresh_lease(
    vol: &Arc<KvMetaBackend>,
    me: AppenderIdentity,
) -> (Arc<alloc_lease::AllocHolding>, Vec<(u16, ExtentRef)>) {
    let g = vol
        .manager_alloc_lease_acquire(DATA_TAG, me, 0, 0, ROOT_INO, DATA_BLOCKS)
        .await
        .expect("first-come grant");
    assert_eq!(g.term, 1);
    assert!(!g.already);
    let holding = vol
        .hold_alloc_lease(DATA_TAG, DATA_BLOCKS, 1, None)
        .await
        .expect("hold the lease");
    let refs: Vec<(u16, ExtentRef)> = holding.pages.iter().map(|e| (0u16, *e)).collect();
    assert!(!vol
        .manager_alloc_lease_bitmap(DATA_TAG, me, 1, refs.clone())
        .await
        .unwrap());
    (holding, refs)
}

fn grant_of(outcome: CarveOutcome) -> BlockGrant {
    match outcome {
        CarveOutcome::Granted(g) => g,
        other => panic!("expected a fresh grant, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// §5.5 — ranged grants: disjoint, journaled ahead, the zombie floor
// ---------------------------------------------------------------------------

/// Two writers' grants on one data volume never overlap, the bits are SET
/// on the holder's bitmap at the carve, `block_grants` / `block_grant_
/// blocks` count them, and the recovery floor is the grants' END: a later
/// carve starts at or above every open grant (a zombie may DMA into any
/// granted-but-unpublished block).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_writers_grants_are_disjoint_and_the_zombie_floor_is_the_grants_end() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_armed(&uris).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let me = identity_of(&vol);
    let (holding, _) = take_fresh_lease(&vol, me).await;

    let a = grant_of(vol.holder_block_grant(DATA_TAG, "a", 64, 0).await.unwrap());
    let b = grant_of(vol.holder_block_grant(DATA_TAG, "b", 64, 0).await.unwrap());
    assert!(!a.overlaps(&b), "{a:?} vs {b:?}");
    assert_eq!(a.len, 64);
    assert_eq!(b.len, 64);
    assert_eq!(
        holding.bitmap.population(),
        128,
        "both grants' bits are SET"
    );
    for blk in a.start..a.end() {
        assert!(holding.bitmap.is_set(blk));
    }
    let s = holding.stats();
    assert_eq!(s.block_grants, 2);
    assert_eq!(s.block_grant_blocks, 128);
    assert!(
        s.deltas_journaled >= 2,
        "one control entry per grant at least"
    );
    // The zombie floor: the ledger's frontier is the grants' end and a
    // third carve starts there.
    let frontier = holding.ledger.grant_frontier().unwrap();
    assert_eq!(frontier, a.end().max(b.end()));
    let c = grant_of(vol.holder_block_grant(DATA_TAG, "c", 16, 0).await.unwrap());
    assert!(c.start >= frontier, "{c:?} below the frontier {frontier}");
    // Idempotency (§5.3.5): a writer whose unconsumed remainder covers
    // the ask is answered it verbatim — nothing carved.
    match vol.holder_block_grant(DATA_TAG, "a", 64, 64).await.unwrap() {
        CarveOutcome::Already(gs) => assert_eq!(gs, vec![a]),
        other => panic!("{other:?}"),
    }
    assert_eq!(holding.stats().block_grants, 3);
    // A return clears the bits and its CLEAR deltas ride the ring.
    assert_eq!(
        vol.holder_return_blocks(DATA_TAG, "b", b).await.unwrap(),
        Some(64)
    );
    assert_eq!(holding.bitmap.population(), 80);
    assert_eq!(
        vol.holder_return_blocks(DATA_TAG, "b", b).await.unwrap(),
        None,
        "a range the ledger does not grant is refused, nothing cleared"
    );
    // The writer's window: mints lowest-first, wants a top-up at 50 %.
    let w = GrantWindow::new();
    assert!(w.install(a));
    assert!(!w.wants_topup());
    for i in 0..33 {
        assert_eq!(w.mint(), Some(a.start + i));
    }
    assert!(w.wants_topup(), "31 of 64 left < half");
    shutdown(&routed).await;
    reset_process_state();
}

/// The derivation: `G = clamp(2 × ewma × T_renewal, 64, cap/(2 × writers))`.
#[test]
fn the_block_grant_derivation_is_clamped_between_its_floor_and_cap() {
    assert_eq!(BLOCK_GRANT_FLOOR, 64);
    assert_eq!(block_grant_derived(0, 10_000, 1 << 30, 1), 64);
    assert_eq!(block_grant_derived(1_000_000, 10_000, 1 << 30, 1), 20_000);
    assert_eq!(block_grant_derived(1_000_000, 10_000, 4_096, 4), 512);
    assert_eq!(block_grant_derived(1_000_000, 10_000, 100, 4), 64);
}

// ---------------------------------------------------------------------------
// §5.5.1 — the crash table's headline rows
// ---------------------------------------------------------------------------

/// Where the holder dies relative to its checkpoint (review round 1,
/// Issue 1: the first build pinned the no-checkpoint window only, and the
/// data pages were written AFTER the tail-advancing ledger record — a
/// kill between the two lost every journaled SET below the new tail).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrashWindow {
    /// No checkpoint between the grant and the kill: the SET deltas are in
    /// the window, the pages are the fresh image.
    BeforeAnyCheckpoint,
    /// A whole checkpoint landed after the grant (pages + ledger).
    AfterACheckpoint,
    /// The checkpoint's ledger record landed and the process died before
    /// anything that follows it — the tail is past the SET deltas, so the
    /// pages MUST already carry them (the meta bitmap's order).
    AfterTheLedgerRecord,
}

/// **The case that matters**: the holder journals a `BlockGrant` SET and
/// dies before its next page write (the pages still show the range FREE).
/// The home manager's ring replay applies the delta to the pages BEFORE
/// `recovered:` is written; the successor cannot be granted the lease
/// before that; it reads pages that show the range SET and never carves
/// inside it. Driven at every crash window of the checkpoint cycle.
async fn a_journaled_grant_survives_the_holders_death(window: CrashWindow, volumes: usize) {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set(dir.path(), volumes).await;
    let (dead, refs, granted) = {
        let routed = open_armed(&uris).await;
        let vol = Arc::clone(&routed.volumes[slot0_of(&routed)]);
        let me = identity_of(&vol);
        let (holding, refs) = take_fresh_lease(&vol, me).await;
        // The pages on the device are the FRESH all-clear image.
        let on_disk = vol
            .read_alloc_bitmap_image(&holding.pages, DATA_BLOCKS)
            .await
            .unwrap();
        assert_eq!(
            DataAllocBitmap::from_region_image(DATA_TAG, DATA_BLOCKS, &on_disk)
                .unwrap()
                .population(),
            0
        );
        let g = grant_of(vol.holder_block_grant(DATA_TAG, "w", 64, 0).await.unwrap());
        match window {
            CrashWindow::BeforeAnyCheckpoint => {}
            CrashWindow::AfterACheckpoint => {
                vol.checkpoint_now().await.unwrap();
            }
            CrashWindow::AfterTheLedgerRecord => {
                // The cycle halts the instant its ledger record landed:
                // whatever the cycle writes AFTER the record never reaches
                // the device — exactly a kill −9 in that window.
                TEST_CHECKPOINT_HALT_AFTER_LEDGER.store(true, Ordering::SeqCst);
                assert!(vol.checkpoint_now().await.is_err(), "the halt fired");
            }
        }
        // The crash: no shutdown, no leave — the holding's RAM dies with
        // the mount.
        drop(vol);
        drop(routed);
        test_clear_holdings();
        park_gate::test_reset();
        TEST_CHECKPOINT_HALT_AFTER_LEDGER.store(false, Ordering::SeqCst);
        (me, refs, g)
    };
    // The home manager remounts (its own-residue recovery replays ring 0
    // for the trees; the bitmap arm is PR 10's driver, driven here).
    let routed = open_armed(&uris).await;
    let vol = Arc::clone(&routed.volumes[slot0_of(&routed)]);
    let extents: Vec<ExtentRef> = refs.iter().map(|(_, e)| *e).collect();
    let stale = vol
        .read_alloc_bitmap_image(&extents, DATA_BLOCKS)
        .await
        .unwrap();
    let pages = DataAllocBitmap::from_region_image(DATA_TAG, DATA_BLOCKS, &stale).unwrap();
    // Row "successor elected before the home recovery": REFUSED by
    // construction — a live holder refuses everyone, a dead one without
    // `recovered:` defers.
    let succ = successor_identity();
    match vol
        .manager_alloc_lease_acquire(DATA_TAG, succ, 1, 0, ROOT_INO, DATA_BLOCKS)
        .await
    {
        Err(KvError::Busy(m)) => assert!(m.contains("death ledger does not name it"), "{m}"),
        other => panic!("a live holder's lease moved: {other:?}"),
    }
    let recorded = alloc_lease::DEAD_MEMBERS_RECORDED.load(Ordering::Relaxed);
    assert!(!vol.record_death(dead, 77).await.unwrap());
    assert!(vol.record_death(dead, 77).await.unwrap(), "idempotent");
    assert_eq!(
        alloc_lease::DEAD_MEMBERS_RECORDED.load(Ordering::Relaxed),
        recorded + 1
    );
    match vol
        .manager_alloc_lease_acquire(DATA_TAG, succ, 1, 0, ROOT_INO, DATA_BLOCKS)
        .await
    {
        Err(KvError::LeaseDeferred(m)) => assert!(m.contains("not yet recovered"), "{m}"),
        other => panic!("a successor was elected before `recovered:`: {other:?}"),
    }
    // The recovery arm: the dead holder's ring replayed onto its pages.
    // Before any checkpoint the window's deltas were KEPT across the
    // remount's own bring-up checkpoint (which advanced the tail past
    // them); after one, the pages themselves carry the grant — the page
    // write precedes the ledger record that passes the deltas.
    let changed = vol.replay_data_alloc_deltas(&pages, 1).await.unwrap();
    match window {
        CrashWindow::BeforeAnyCheckpoint => {
            assert_eq!(changed, 64, "the journaled SET landed on the pages")
        }
        _ => assert_eq!(changed, 0, "the pages already carried the grant"),
    }
    assert_eq!(squeezefs::data_alloc_bitmap::replayed_deltas_kept(), 0);
    for blk in granted.start..granted.end() {
        assert!(
            pages.is_set(blk),
            "block {blk} of {granted:?} reads FREE ({window:?})"
        );
    }
    assert!(!vol.manager_record_recovered(dead, 0).await.unwrap());
    assert!(vol.manager_record_recovered(dead, 0).await.unwrap());
    assert!(vol.recovered_record(&dead, 0).await.unwrap().is_some());
    // Now the successor is granted, copies the RECOVERED pages into its
    // own extents and publishes the new refs.
    let g2 = vol
        .manager_alloc_lease_acquire(DATA_TAG, succ, 1, 0, ROOT_INO, DATA_BLOCKS)
        .await
        .unwrap();
    assert_eq!(g2.term, 2);
    assert_eq!(
        g2.predecessor_bitmap, refs,
        "the dead holder's pages, volume-qualified"
    );
    // Crash row 8 (review round 1, Issue 14): the successor dies before
    // its copy and RETRIES — `already` answers the record's pages and
    // block count, so it can still learn what to copy.
    let again = vol
        .manager_alloc_lease_acquire(DATA_TAG, succ, 1, 0, ROOT_INO, DATA_BLOCKS)
        .await
        .unwrap();
    assert!(again.already);
    assert_eq!(again.term, 2);
    assert_eq!(again.predecessor_bitmap, refs);
    assert_eq!(again.predecessor_blocks, DATA_BLOCKS);
    let image = pages.region_image(2).unwrap();
    let holding = vol
        .hold_alloc_lease(DATA_TAG, DATA_BLOCKS, 2, Some(&image))
        .await
        .unwrap();
    assert_ne!(
        holding.pages, extents,
        "the copy lives in the successor's own extents"
    );
    let new_refs: Vec<(u16, ExtentRef)> = holding.pages.iter().map(|e| (0u16, *e)).collect();
    assert!(!vol
        .manager_alloc_lease_bitmap(DATA_TAG, succ, 2, new_refs.clone())
        .await
        .unwrap());
    let rec = vol.alloc_lease_record(DATA_TAG).await.unwrap().unwrap();
    assert_eq!(rec.bitmap, new_refs);
    assert_eq!(rec.holder, succ);
    // The headline: the successor never carves inside the dead holder's
    // journaled grant.
    for _ in 0..8 {
        let g = grant_of(vol.holder_block_grant(DATA_TAG, "x", 64, 0).await.unwrap());
        assert!(!g.overlaps(&granted), "{g:?} re-granted inside {granted:?}");
    }
    shutdown(&routed).await;
    reset_process_state();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grant_journaled_before_a_holder_death_is_never_regranted() {
    a_journaled_grant_survives_the_holders_death(CrashWindow::BeforeAnyCheckpoint, 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grant_journaled_before_a_holder_death_is_never_regranted_after_a_checkpoint() {
    a_journaled_grant_survives_the_holders_death(CrashWindow::AfterACheckpoint, 1).await;
}

/// Review round 1, Issue 1 — the headline crash row: a kill between the
/// ledger record and whatever the cycle writes after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kill_after_the_ledger_record_still_finds_the_journaled_grant_set() {
    a_journaled_grant_survives_the_holders_death(CrashWindow::AfterTheLedgerRecord, 1).await;
}

/// The same three windows on the design's 2-volume shape (the holder homed
/// on the slot-0 volume, a second volume checkpointing beside it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_journaled_grant_survives_the_holders_death_on_a_two_volume_set() {
    a_journaled_grant_survives_the_holders_death(CrashWindow::BeforeAnyCheckpoint, 2).await;
    a_journaled_grant_survives_the_holders_death(CrashWindow::AfterACheckpoint, 2).await;
    a_journaled_grant_survives_the_holders_death(CrashWindow::AfterTheLedgerRecord, 2).await;
}

/// Review round 1, Issue 3 (reviewer-reproduced): `write_data_alloc_pages`
/// ran on EVERY volume's checkpoint — volume 1 wrote a `KVDA` page onto
/// ITS device at volume 0's heap offset and consumed the holder's dirty
/// bits. The pages are written ONLY by the holder's checkpoint on the
/// holder's HOME volume.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_volumes_checkpoint_never_writes_the_holders_pages_onto_its_own_device() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set(dir.path(), 2).await;
    let routed = open_armed(&uris).await;
    let slot0 = slot0_of(&routed);
    let home = Arc::clone(&routed.volumes[slot0]);
    let other = Arc::clone(&routed.volumes[1 - slot0]);
    let me = identity_of(&home);
    let (holding, _) = take_fresh_lease(&home, me).await;
    let base = holding.pages[0].start;
    grant_of(home.holder_block_grant(DATA_TAG, "w", 64, 0).await.unwrap());
    assert!(holding.bitmap.has_dirty_pages());
    // The OTHER volume's checkpoint: no `KVDA` page lands on its device at
    // the holder's page offsets (its own heap may legitimately place a
    // NODE there — the two volumes share a geometry and a claim history —
    // so the witness is the page magic, never the raw bytes), and the
    // holder's dirty bits still pend its OWN cycle.
    other.checkpoint_now().await.unwrap();
    let after = squeezefs::uring_fs::read_at(other.device_path(), base, 4096)
        .await
        .unwrap();
    assert_ne!(
        u32::from_le_bytes(after[..4].try_into().unwrap()),
        squeezefs::data_alloc_bitmap::DATA_ALLOC_PAGE_MAGIC,
        "the other volume's device received a data bitmap page"
    );
    assert!(
        holding.bitmap.has_dirty_pages(),
        "the other volume's checkpoint consumed the holder's dirty bits"
    );
    // The HOME volume's checkpoint writes them where the record says.
    home.checkpoint_now().await.unwrap();
    assert!(!holding.bitmap.has_dirty_pages());
    let on_home = home
        .read_alloc_bitmap_image(&holding.pages, DATA_BLOCKS)
        .await
        .unwrap();
    assert_eq!(
        DataAllocBitmap::from_region_image(DATA_TAG, DATA_BLOCKS, &on_home)
            .unwrap()
            .population(),
        64
    );
    shutdown(&routed).await;
    reset_process_state();
}

/// Review round 1, Issue 2 (reviewer-reproduced): a terminal free's CLEAR
/// was journaled at the NEXT checkpoint, after a re-grant's SET of the
/// same block — the replay's seq-LWW folded the granted block CLEAR. A
/// delta is journaled at the instant of the decision it records, in RAM
/// order with every other delta of the holding: the CLEAR lands at
/// `finish_free` (no checkpoint in its path), the re-grant's SET after it,
/// and the recovery arm reads the block SET.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_frees_clear_is_journaled_at_finish_free_in_order_with_a_regrants_set() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set(dir.path(), 2).await;
    let routed = open_armed(&uris).await;
    let vol = Arc::clone(&routed.volumes[slot0_of(&routed)]);
    let me = identity_of(&vol);
    let (holding, _) = take_fresh_lease(&vol, me).await;
    // Grant the whole volume so the next carve falls back below the
    // frontier.
    let mut first = None;
    while let CarveOutcome::Granted(g) = vol.holder_block_grant(DATA_TAG, "w", 64, 0).await.unwrap()
    {
        first.get_or_insert(g);
    }
    let b = first.unwrap().start;
    let ckpts = META_KV_CHECKPOINTS.load(Ordering::Relaxed);
    let journaled = holding.stats().deltas_journaled;
    // The terminal free of `b`: the bit clears in RAM and its CLEAR is
    // journaled NOW — the queue drains without a checkpoint.
    assert!(alloc_lease::note_finish_free(DATA_TAG, b));
    let landed = tokio::time::timeout(Duration::from_secs(5), async {
        while holding.queued_deltas() != 0 || holding.stats().deltas_journaled == journaled {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
    assert!(landed.is_ok(), "the CLEAR waited for a checkpoint");
    assert_eq!(META_KV_CHECKPOINTS.load(Ordering::Relaxed), ckpts);
    // The re-grant of the hole to another writer: its SET journaled after
    // the CLEAR, in RAM order.
    let regrant = grant_of(vol.holder_block_grant(DATA_TAG, "v", 1, 0).await.unwrap());
    assert_eq!(regrant.start, b, "the hole is re-granted");
    assert!(holding.bitmap.is_set(b));
    vol.checkpoint_now().await.unwrap();
    let on_disk = vol
        .read_alloc_bitmap_image(&holding.pages, DATA_BLOCKS)
        .await
        .unwrap();
    let pages = DataAllocBitmap::from_region_image(DATA_TAG, DATA_BLOCKS, &on_disk).unwrap();
    assert!(pages.is_set(b));
    vol.replay_data_alloc_deltas(&pages, 1).await.unwrap();
    assert!(
        pages.is_set(b),
        "block {b} is granted to `v` yet the recovered bitmap reads it CLEAR"
    );
    // A holder's own re-mint of a freed block is a grant (the bitmap IS
    // the free list): the allocator armed on this volume never hands a
    // freed block back off a local free list with its bit clear.
    shutdown(&routed).await;
    reset_process_state();
}

/// The rest of the §5.5.1 table, driven on the bitmap and the ledger: a
/// referenced block is never inside a successor's carve across (3) a
/// clear-then-die, (6) a torn newest page, (7) a re-run recovery and (8)
/// a successor dying after its copy before the refs landed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_allocation_lease_successor_never_regrants_a_referenced_block() {
    let _g = SEAM.lock().await;
    reset_process_state();
    // The referenced population: three files' blocks inside a grant the
    // predecessor made durable, plus one block whose bit the predecessor
    // cleared (a terminal free) and immediately re-granted — the SET
    // supersedes the CLEAR by seq in the fold.
    let pred = DataAllocBitmap::new(DATA_TAG, DATA_BLOCKS);
    let ledger = BlockGrantLedger::new();
    let g = grant_of(ledger.carve(&pred, "p", 64, 0, 0));
    let referenced: std::collections::BTreeSet<u64> = (g.start..g.start + 3).collect();
    let recs = [
        set_record(DATA_TAG, 200, 1, 10),
        clear_record(DATA_TAG, 200, 1, 11),
        set_record(DATA_TAG, 200, 1, 12),
        clear_record(DATA_TAG, 201, 1, 13),
        // Another holder TERM's delta never folds into this recovery
        // (review round 1, Issue 9: the kept window and the ring scan are
        // keyed per (volume, holder term)).
        clear_record(DATA_TAG, 200, 2, 14),
    ];
    // Row 3/7: the replay is idempotent — run twice, same bits.
    for _ in 0..2 {
        pred.replay(recs.iter().map(|(t, r)| (*t, r)), 1);
        assert!(pred.is_set(200) && !pred.is_set(201));
    }
    // Row 6: a torn NEWEST page falls back to the predecessor copy. Write
    // generation 5 with the grant, then a torn generation 6 in the other
    // slot: the load must select generation 5.
    let mut region = vec![0u8; squeezefs::data_alloc_bitmap::region_len(DATA_BLOCKS) as usize];
    let good = pred.region_image(5).unwrap();
    region[..DATA_ALLOC_PAGE_LEN as usize].copy_from_slice(&good[..DATA_ALLOC_PAGE_LEN as usize]);
    let mut torn = encode_data_alloc_page(DATA_TAG, 0, 6, &[0u8; 8]).unwrap();
    torn[100] ^= 0xFF;
    region[DATA_ALLOC_PAGE_LEN as usize..2 * DATA_ALLOC_PAGE_LEN as usize].copy_from_slice(&torn);
    let loaded = DataAllocBitmap::from_region_image(DATA_TAG, DATA_BLOCKS, &region).unwrap();
    assert_eq!(loaded.population(), pred.population());
    assert!(decode_data_alloc_page(&torn, DATA_TAG, 0).is_err());
    // Review round 1, Issue 6: a page pair with NO valid slot is FRESH
    // only when both slots are all-zero; a non-zero pair nobody can decode
    // (bogus refs, an unreadable predecessor region) REFUSES the load —
    // never "all free".
    let mut corrupt = region.clone();
    corrupt[..DATA_ALLOC_PAGE_LEN as usize].copy_from_slice(&torn);
    assert!(
        DataAllocBitmap::from_region_image(DATA_TAG, DATA_BLOCKS, &corrupt).is_err(),
        "two invalid non-zero slots read as an all-free bitmap"
    );
    assert_eq!(
        DataAllocBitmap::from_region_image(DATA_TAG, DATA_BLOCKS, &[])
            .unwrap()
            .population(),
        0,
        "a fresh (all-zero) region is a fresh bitmap"
    );
    // Row 8: the first successor copies (generation 7) and dies before
    // its refs land; the next successor copies AGAIN from the still-
    // Recovered image — the same bits, and no carve inside the
    // referenced set.
    let copy1 =
        DataAllocBitmap::from_region_image(DATA_TAG, DATA_BLOCKS, &loaded.region_image(7).unwrap())
            .unwrap();
    let copy2 =
        DataAllocBitmap::from_region_image(DATA_TAG, DATA_BLOCKS, &loaded.region_image(8).unwrap())
            .unwrap();
    assert_eq!(copy1.set_blocks(), copy2.set_blocks());
    let succ = BlockGrantLedger::new();
    succ.adopt("p", g);
    for _ in 0..16 {
        let c = grant_of(succ.carve(&copy2, "s", 64, copy2.highest_set().map_or(0, |h| h + 1), 0));
        for b in &referenced {
            assert!(!c.contains(*b), "referenced block {b} inside {c:?}");
        }
        assert!(!c.contains(200));
    }
    // The oracle agrees: nothing referenced is clear, nothing set is
    // outside a reference or an open grant.
    let mut refd = referenced.clone();
    refd.insert(200);
    let d = copy2.drift(&refd, &succ.open_ranges());
    assert!(d.loss.is_empty(), "{:?}", d.loss);
    assert!(d.leak.is_empty(), "{:?}", d.leak);
    reset_process_state();
}

// ---------------------------------------------------------------------------
// §5.5.2 — the death ledger's records; holder failover keeps quarantine
// ---------------------------------------------------------------------------

/// A dead WRITER's OUTSTANDING grants (the unpublished remainder a zombie
/// may still DMA into) are revoked from the holder's ledger BY THE DEATH
/// RECORD and QUARANTINED on the holder's allocator under a dead epoch
/// (S7 — `dlm_quarantined_offsets`) — never carved again until a drain
/// proof; the records themselves are idempotent and keyed by the KD-MW-2
/// identity. The holder is the production arm's (review round 1, Issue 7:
/// `record_death` → `revoke_dead` → the quarantine is the wired chain).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_death_record_revokes_the_dead_writers_grants_into_the_quarantine() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set(dir.path(), 2).await;
    let routed = open_armed(&uris).await;
    let vol = Arc::clone(&routed.volumes[slot0_of(&routed)]);
    let me = identity_of(&vol);
    let alloc = data_allocator(DATA_ID).await;
    assert_eq!(
        alloc_lease::arm_symmetric_allocation(&routed, &[Arc::clone(&alloc)])
            .await
            .unwrap(),
        1,
        "the armed mount holds its data volume's lease first-come"
    );
    let holding = alloc_lease::holding(DATA_TAG).expect("held");
    assert_eq!(holding.term, 1);
    let w = successor_identity();
    let name = wire_writer_name(&wire(w));
    let g = grant_of(
        vol.holder_block_grant(DATA_TAG, &name, 64, 0)
            .await
            .unwrap(),
    );
    let quarantined_before = alloc.quarantined_count();
    // The death ledger names the writer: its grants leave the table for
    // the quarantine; the bits stay SET (a clear waits on the drain
    // proof).
    assert!(!vol.record_death(w, 5).await.unwrap());
    assert_eq!(holding.ledger.grants_of(&name), Vec::<BlockGrant>::new());
    assert_eq!(holding.stats().grants_revoked, 1);
    assert_eq!(
        alloc.quarantined_count() - quarantined_before,
        64,
        "the dead writer's unpublished remainder is quarantined on the allocator"
    );
    assert!(vol.record_death(w, 5).await.unwrap(), "idempotent");
    assert_eq!(holding.stats().grants_revoked, 1);
    for b in g.start..g.end() {
        assert!(holding.bitmap.is_set(b), "quarantined bits stay set");
    }
    // A later carve for anyone else never lands inside the quarantined range.
    for _ in 0..4 {
        let c = grant_of(
            vol.holder_block_grant(DATA_TAG, "other", 64, 0)
                .await
                .unwrap(),
        );
        assert!(!c.overlaps(&g));
    }
    assert!(alloc_lease::DEAD_MEMBERS_ACTED.load(Ordering::Relaxed) >= 1);
    // The records' keys are the identity pair, and volume-qualified for
    // `recovered:`.
    assert_ne!(dead_member_key(&w), dead_member_key(&me));
    assert_ne!(recovered_key(&w, 0), recovered_key(&w, 1));
    shutdown(&routed).await;
    reset_process_state();
}

/// **The undelivered arm delivered** (review round 1, Issue 7): on an
/// armed mount `BlockAllocator`'s fresh-block mint comes from the writer's
/// GRANTED WINDOW — the production arm acquires the data volume's
/// allocation lease first-come and installs the grant arm; a second
/// writer's allocator (its window fed by the holder over the wire) mints
/// disjoint ranges; `block_grants` > 0, the S9 lane family flat; a writer's
/// terminal free clears the bit at the holder (the holder's own allocator
/// has no local free list under the plane — the bitmap IS the free list),
/// and a wire writer's frees are re-homed to the holder's endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_armed_two_writer_set_allocates_disjoint_ranges_from_grants() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set(dir.path(), 2).await;
    let routed = open_armed(&uris).await;
    let vol = Arc::clone(&routed.volumes[slot0_of(&routed)]);
    let a = data_allocator(DATA_ID).await;
    assert!(!a.block_grant_armed());
    assert_eq!(
        alloc_lease::arm_symmetric_allocation(&routed, &[Arc::clone(&a)])
            .await
            .unwrap(),
        1
    );
    assert!(a.block_grant_armed(), "the armed mount mints from grants");
    let lanes = &squeezefs::fuse_client::METRICS;
    let lane_writers = lanes.alloc_lane_writers.load(Ordering::Relaxed);
    let lane_reservations = lanes.alloc_lane_reservations.load(Ordering::Relaxed);
    let holding = alloc_lease::holding(DATA_TAG).expect("held");
    // Writer B: another daemon's allocator on the same data volume, its
    // window topped up through the manager's wire venue.
    let host = squeezefs::cluster_wire::RpcListener::start_async(
        listener_cfg(),
        SECRET.to_vec(),
        ManagerService::new(Arc::clone(&vol)),
    )
    .expect("manager listener");
    let endpoint = host.endpoint().to_string();
    let b = data_allocator(DATA_ID).await;
    let w = wire(successor_identity());
    assert!(b.install_block_grant_arm(
        DATA_TAG,
        alloc_lease::wire_block_grant_sink(endpoint.clone(), SECRET.to_vec(), w, 0, DATA_TAG),
    ));
    assert!(alloc_lease::install_wire_free_target(DATA_TAG, &endpoint));
    let mut mine_a = std::collections::BTreeSet::new();
    let mut mine_b = std::collections::BTreeSet::new();
    for _ in 0..200 {
        mine_a.insert(a.allocate_block().await.unwrap() / a.chunk_size());
        mine_b.insert(b.allocate_block().await.unwrap() / b.chunk_size());
    }
    assert_eq!(mine_a.len(), 200);
    assert_eq!(mine_b.len(), 200);
    assert!(mine_a.is_disjoint(&mine_b), "two writers minted one block");
    let s = holding.stats();
    assert!(s.block_grants >= 2, "{s:?}");
    assert!(a.block_grant_topups() >= 1, "A topped up past 64 blocks");
    assert!(b.block_grant_topups() >= 1, "B topped up past 64 blocks");
    for blk in mine_a.iter().chain(mine_b.iter()) {
        assert!(holding.bitmap.is_set(*blk), "minted block {blk} not SET");
    }
    assert_eq!(
        lanes.alloc_lane_writers.load(Ordering::Relaxed),
        lane_writers,
        "the S9 partition stays disengaged"
    );
    assert_eq!(
        lanes.alloc_lane_reservations.load(Ordering::Relaxed),
        lane_reservations,
        "the lane family stays flat"
    );
    // A's terminal free clears the bit at the holder and never lands on a
    // local free list: the next mint is a GRANTED block, never the freed
    // one off the list with its bit clear.
    let freed = *mine_a.iter().next().unwrap();
    a.begin_free(freed * a.chunk_size());
    a.finish_free(freed * a.chunk_size());
    assert!(!holding.bitmap.is_set(freed));
    let next = a.allocate_block().await.unwrap() / a.chunk_size();
    assert!(holding.bitmap.is_set(next), "a mint whose bit is CLEAR");
    // B's frees are re-homed: the free target for this data volume is the
    // holder's venue (what `cowriter::ship_displaced_frees` ships to).
    assert_eq!(
        free_target_for(DATA_TAG).as_deref(),
        Some(endpoint.as_str())
    );
    drop(host);
    shutdown(&routed).await;
    reset_process_state();
}

/// The `alloc_lease:` record round-trips, refuses a future version, and a
/// release by the holder at its term deletes it (a stale term is a replay).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_alloc_lease_record_round_trips_and_a_release_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let rec = AllocLeaseRecord {
        holder: successor_identity(),
        holder_appender_id: 3,
        home_vol: 2,
        control_ino: 9,
        blocks: 262_144,
        term: 4,
        bitmap: vec![(
            2,
            ExtentRef {
                start: 4096,
                len: 65536,
            },
        )],
    };
    let img = rec.encode().unwrap();
    assert_eq!(AllocLeaseRecord::decode(&img).unwrap(), rec);
    let mut future = img.clone();
    future[0] = 2;
    assert!(AllocLeaseRecord::decode(&future).is_err());
    assert!(AllocLeaseRecord::decode(&img[..img.len() - 1]).is_err());

    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_armed(&uris).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let me = identity_of(&vol);
    let (_holding, _) = take_fresh_lease(&vol, me).await;
    assert!(
        vol.manager_alloc_lease_acquire(DATA_TAG, me, 0, 0, ROOT_INO, DATA_BLOCKS)
            .await
            .unwrap()
            .already
    );
    assert!(
        vol.manager_alloc_lease_release(DATA_TAG, me, 7)
            .await
            .unwrap(),
        "a stale term's release is a replay"
    );
    assert!(vol.alloc_lease_record(DATA_TAG).await.unwrap().is_some());
    assert!(!vol
        .manager_alloc_lease_release(DATA_TAG, me, 1)
        .await
        .unwrap());
    assert!(vol.alloc_lease_record(DATA_TAG).await.unwrap().is_none());
    assert!(vol
        .manager_alloc_lease_release(DATA_TAG, me, 1)
        .await
        .unwrap());
    shutdown(&routed).await;
    reset_process_state();
}

// ---------------------------------------------------------------------------
// §5.5.3 — park-and-reclaim
// ---------------------------------------------------------------------------

fn shipped_clocks() -> LeaseClocks {
    LeaseClocks::derive(Duration::ZERO).expect("shipped clocks")
}

fn join_req(id: &str) -> JoinRequest {
    JoinRequest {
        id: id.to_string(),
        role: MemberRole::Writer,
        endpoint: Some("10.0.0.9:7100".to_string()),
        pid: std::process::id(),
        boot: "boot-pr8".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    }
}

/// Wait (bounded, no fixed sleep) until `cond` holds — the contracts'
/// synchronization on the conveyor's observable words.
async fn wait_until(what: &str, cond: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for: {what}"));
}

/// A manual lease clock plus its tick word.
fn manual_clock() -> (LeaseClock, Arc<std::sync::atomic::AtomicU64>) {
    let ticks = Arc::new(std::sync::atomic::AtomicU64::new(1_000));
    (LeaseClock::manual(Arc::clone(&ticks)), ticks)
}

fn plane_cfg() -> MembershipPlaneConfig {
    MembershipPlaneConfig {
        bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
        service_threads: 2,
        session_idle: Duration::from_secs(60),
    }
}

/// **The membership half of the home-shard failover, through the
/// PRODUCTION renewal tick** (review round 1, Issues 5/10/12): a Writer
/// member of a symmetric appender, joined over the wire to owner A,
/// passes `T_self` with A dead — the tick answers `Parked` (nothing
/// poisoned, the session not fenced, the renewal loop keeps ticking), a
/// commit admitted before the park lands with its ACK HELD, admission
/// waits; the successor B arms at a NEW venue and opens grace; the ledger
/// observation names it (`note_successor_observed` — PR 10's poll is the
/// production caller, the seam here) and the parked tick reclaims AGAINST
/// THE SUCCESSOR with its epoch; the held ack releases and admission
/// resumes; `appender_park_expiries == 0`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parked_member_reclaims_against_the_successor_the_ledger_names() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    // ONE volume: the journal-order witness below is per RING (a 2-volume
    // set routes the two creates by slot, possibly onto two lanes, whose
    // fan-outs are unordered by design).
    let uris = format_stamped_set(dir.path(), 1).await;
    let routed = open_armed(&uris).await;
    let vol = Arc::clone(&routed.volumes[slot0_of(&routed)]);
    assert!(
        park_gate::symmetric_appender_armed(),
        "the armed open arms the park posture"
    );
    assert!(
        park_gate::t_park_max_in_force_ms() > 30_000,
        "T_park_max > SQUEEZEFS_TIMEOUT"
    );

    // Owner A's plane at venue E1; our member joins over the wire.
    let (clock, ticks) = manual_clock();
    let owner_a = MembershipOwner::arm("shard-a", 3, 2, shipped_clocks(), clock.clone()).unwrap();
    let plane_a = MembershipPlane::start(plane_cfg(), SECRET.to_vec(), Arc::clone(&owner_a))
        .expect("owner A listens");
    let e1 = plane_a.endpoint().to_string();
    let req = join_req("member-x");
    let mut client = MemberClient::join(&e1, SECRET, req.clone(), clock.clone())
        .await
        .expect("the member joins");
    let session = Arc::clone(client.session());
    let epoch = session.epoch();
    let t_self_ms = shipped_clocks().t_self.as_millis() as u64;

    // A commit lands and is held at the lane's pre-fanout seam; THEN the
    // park is raised, so its ack is HELD by the park (in-flight entries
    // land, acks wait).
    // The completion ORDINAL is the journal-order witness (inos are
    // per-slot cursors on a forest, never an order).
    let order = Arc::new(std::sync::atomic::AtomicU64::new(0));
    TEST_CONVEYOR_HOLD_STAGE.store(TEST_CONVEYOR_HOLD_PRE_FANOUT, Ordering::SeqCst);
    let r1 = Arc::clone(&routed);
    let o1 = Arc::clone(&order);
    let held = tokio::spawn(async move {
        let r = r1
            .create(ROOT_INO, "held", libc::S_IFREG | 0o644, 0, 0)
            .await
            .map(|i| i.ino);
        (r, o1.fetch_add(1, Ordering::SeqCst))
    });
    wait_until("the lane at the pre-fanout seam", || {
        park_gate::inflight() >= 1
    })
    .await;
    assert!(!held.is_finished(), "the lane is at the seam");

    // A dies: the venue is gone. Before T_self the tick RE-ASSERTS (a
    // reclaim, refused unreachable) — never a fence, never a park.
    plane_a.shutdown();
    drop(plane_a);
    drop(owner_a);
    ticks.fetch_add(1_000, Ordering::SeqCst);
    match member_renewal_tick(&mut client, &e1, SECRET, &req, &clock, None).await {
        RenewalTick::RejoinRefused => {}
        other => panic!("before T_self: {other:?}"),
    }
    assert!(!park_gate::is_parked());
    // T_self passes: the tick's fence is the PARK, and the tick is
    // RETRYABLE — the loop keeps ticking (a `Fenced` here is the silent
    // permanent park the review found).
    ticks.fetch_add(t_self_ms, Ordering::SeqCst);
    match member_renewal_tick(&mut client, &e1, SECRET, &req, &clock, None).await {
        RenewalTick::Parked => {}
        other => panic!("at T_self: {other:?}"),
    }
    assert!(park_gate::is_parked());
    assert!(!session.fenced(), "a parked session is not fenced");
    assert!(!squeezefs::data_custody::poisoned());
    assert_eq!(park_gate::parks(), 1);
    // Release the seam: the lane reaches the park's ack hold.
    TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
    test_conveyor_hold_release();
    wait_until("the landed entry's ack held", || {
        park_gate::acks_held() >= 1
    })
    .await;
    assert!(!held.is_finished(), "the landed entry's ack is HELD");
    // A new commit waits at the pre-admission door — never escalates.
    let r2 = Arc::clone(&routed);
    let o2 = Arc::clone(&order);
    let parked = tokio::spawn(async move {
        let r = r2
            .create(ROOT_INO, "parked", libc::S_IFREG | 0o644, 0, 0)
            .await
            .map(|i| i.ino);
        (r, o2.fetch_add(1, Ordering::SeqCst))
    });
    tokio::task::yield_now().await;
    assert!(!parked.is_finished());
    assert!(!vol.is_failed(), "a park is not a fail-stop");
    // Reads keep serving and the token service continues.
    assert!(routed.lookup(ROOT_INO, "nothing").await.is_err());
    assert!(park_gate::admits_token_service());
    // Another parked tick against the dead venue: the park stands
    // (`Parked` again — the beat paces it, nothing spins), no expiry.
    ticks.fetch_add(1, Ordering::SeqCst);
    match member_renewal_tick(&mut client, &e1, SECRET, &req, &clock, None).await {
        RenewalTick::Parked => {}
        other => panic!("parked tick: {other:?}"),
    }
    assert_eq!(park_gate::expiries(), 0);
    // The successor B arms at venue E2 and opens grace; the ledger
    // observation names it; the parked tick reclaims AGAINST B.
    let owner_b = MembershipOwner::arm("shard-b", 4, 3, shipped_clocks(), clock.clone()).unwrap();
    owner_b.open_grace(vec!["member-x".to_string()]);
    let plane_b = MembershipPlane::start(plane_cfg(), SECRET.to_vec(), Arc::clone(&owner_b))
        .expect("owner B listens");
    let e2 = plane_b.endpoint().to_string();
    membership::note_successor_observed(Some(e2.clone()));
    assert_eq!(
        membership::successor_endpoint().as_deref(),
        Some(e2.as_str())
    );
    match member_renewal_tick(&mut client, &e1, SECRET, &req, &clock, None).await {
        RenewalTick::Rejoined => {}
        other => panic!("the reclaim against the successor: {other:?}"),
    }
    assert_eq!(
        client.session().epoch(),
        epoch,
        "the reclaim kept its epoch"
    );
    let (held_ino, held_order) = tokio::time::timeout(Duration::from_secs(10), held)
        .await
        .expect("the held ack releases on the grant")
        .unwrap();
    let held_ino = held_ino.unwrap();
    let (parked_ino, parked_order) = tokio::time::timeout(Duration::from_secs(10), parked)
        .await
        .expect("the parked commit admits on the grant")
        .unwrap();
    let parked_ino = parked_ino.unwrap();
    assert!(
        held_order < parked_order,
        "journal order: the held ack is fanned out before the parked commit lands"
    );
    assert_eq!(routed.lookup(ROOT_INO, "held").await.unwrap().ino, held_ino);
    assert_eq!(
        routed.lookup(ROOT_INO, "parked").await.unwrap().ino,
        parked_ino
    );
    assert!(!park_gate::is_parked());
    assert_eq!(park_gate::reclaims(), 1);
    assert_eq!(
        park_gate::expiries(),
        0,
        "must-stay-0 on a healthy failover"
    );
    let [wait, rtt, total] = park_gate::park_ns();
    assert_eq!(wait + rtt, total, "exact-sum");
    assert!(!owner_b.grace_active());
    client.leave().await.expect("clean leave");
    plane_b.shutdown();
    shutdown(&routed).await;
    reset_process_state();
}

/// Review round 1, Issue 5 — the not-custody arm never parks: before
/// `T_self` the owner answers "not custody" (an owner that lost its RAM
/// leases, an eviction) — the production tick fences TERMINALLY (custody
/// poisoned, the session fenced, `Fenced`) on an armed appender exactly as
/// shipped; the gate never stands `Parked`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_not_custody_answer_before_t_self_fences_terminally_and_never_parks() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set(dir.path(), 1).await;
    let routed = open_armed(&uris).await;
    assert!(park_gate::symmetric_appender_armed());
    let (clock, ticks) = manual_clock();
    let owner = MembershipOwner::arm("shard-a", 3, 2, shipped_clocks(), clock.clone()).unwrap();
    let plane = MembershipPlane::start(plane_cfg(), SECRET.to_vec(), Arc::clone(&owner))
        .expect("owner listens");
    let e = plane.endpoint().to_string();
    let req = join_req("member-y");
    let mut client = MemberClient::join(&e, SECRET, req.clone(), clock.clone())
        .await
        .expect("joins");
    let session = Arc::clone(client.session());
    // The owner forgets the lease (the eviction shape); the member is
    // well before T_self.
    assert!(owner
        .evict("member-y", "the review's not-custody shape")
        .is_some());
    ticks.fetch_add(500, Ordering::SeqCst);
    match member_renewal_tick(&mut client, &e, SECRET, &req, &clock, None).await {
        RenewalTick::Fenced => {}
        other => panic!("not-custody before T_self: {other:?}"),
    }
    assert!(session.fenced(), "the lease is gone by the owner's verdict");
    assert!(
        squeezefs::data_custody::poisoned(),
        "a Writer's fence poisons"
    );
    assert!(!park_gate::is_parked(), "the not-custody arm PARKED");
    assert_eq!(park_gate::parks(), 0);
    plane.shutdown();
    shutdown(&routed).await;
    reset_process_state();
}

/// Review round 1, Issue 5 — a park that is never granted EXPIRES at
/// `T_park_max` through the production tick: the parked committer gets
/// EIO, the held ack fails, custody is poisoned, `appender_park_expiries`
/// moves — never a silent permanent park.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_park_never_granted_expires_at_t_park_max_with_eio_never_silent() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set(dir.path(), 1).await;
    let routed = open_armed(&uris).await;
    let (clock, ticks) = manual_clock();
    let owner = MembershipOwner::arm("shard-a", 3, 2, shipped_clocks(), clock.clone()).unwrap();
    let plane = MembershipPlane::start(plane_cfg(), SECRET.to_vec(), Arc::clone(&owner))
        .expect("owner listens");
    let e = plane.endpoint().to_string();
    let req = join_req("member-z");
    let mut client = MemberClient::join(&e, SECRET, req.clone(), clock.clone())
        .await
        .expect("joins");
    plane.shutdown();
    drop(plane);
    drop(owner);
    let t_self_ms = shipped_clocks().t_self.as_millis() as u64;
    ticks.fetch_add(t_self_ms + 1, Ordering::SeqCst);
    match member_renewal_tick(&mut client, &e, SECRET, &req, &clock, None).await {
        RenewalTick::Parked => {}
        other => panic!("{other:?}"),
    }
    assert!(park_gate::is_parked());
    let r = Arc::clone(&routed);
    let parked = tokio::spawn(async move {
        r.create(ROOT_INO, "doomed", libc::S_IFREG | 0o644, 0, 0)
            .await
            .map(|i| i.ino)
    });
    tokio::task::yield_now().await;
    assert!(!parked.is_finished());
    // T_park_max passes with no successor: the next tick expires the
    // park — terminal, loud, counted.
    ticks.fetch_add(park_gate::t_park_max_in_force_ms() + 1, Ordering::SeqCst);
    match member_renewal_tick(&mut client, &e, SECRET, &req, &clock, None).await {
        RenewalTick::Fenced => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(park_gate::expiries(), 1, "the one terminal signal moved");
    assert!(park_gate::is_expired());
    assert!(squeezefs::data_custody::poisoned());
    assert!(client.session().fenced());
    let outcome = tokio::time::timeout(Duration::from_secs(10), parked)
        .await
        .expect("the parked committer is answered, never left parked")
        .unwrap();
    assert!(outcome.is_err(), "EIO to the parked op, got {outcome:?}");
    shutdown(&routed).await;
    reset_process_state();
}

/// Review round 2, Issue 22 — the checkpoint's page step never lets a
/// USER-class admission decide the cycle's outcome (§4.4 pt 5: the
/// checkpoint task's own admissions never park and never fail the cycle):
/// with the ring's user window EXHAUSTED and a holding whose queued deltas
/// cannot be journaled, the cycle still lands its pages, barrier and
/// ledger record — no `JournalReserveExhausted`, no D1.b escalation, the
/// volume never fails; the deltas stay queued (counted) and are journaled
/// once the window frees.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_checkpoint_under_ring_pressure_lands_the_pages_and_never_fails_on_a_queued_delta() {
    use squeezefs::meta_backend::kv::journal_core::AdmissionClass;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set(dir.path(), 2).await;
    let routed = open_armed(&uris).await;
    let vol = Arc::clone(&routed.volumes[slot0_of(&routed)]);
    let me = identity_of(&vol);
    let (holding, _) = take_fresh_lease(&vol, me).await;
    let g = grant_of(vol.holder_block_grant(DATA_TAG, "w", 64, 0).await.unwrap());
    vol.checkpoint_now().await.unwrap();
    // Exhaust the USER window: every admission the ring will give, held.
    let core = vol.journal_ring().core();
    let mut held = Vec::new();
    for grain in [4096u64, 64, 1] {
        while let Some(adm) = core.try_admit(grain, AdmissionClass::User) {
            held.push(adm);
        }
    }
    assert!(!held.is_empty());
    assert!(
        core.try_admit(1, AdmissionClass::User).is_none(),
        "the user window is exhausted"
    );
    // A terminal free under that pressure: the bit clears, its CLEAR is
    // queued, the journaler cannot land it.
    assert!(alloc_lease::note_finish_free(DATA_TAG, g.start));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(holding.queued_deltas(), 1, "the CLEAR waits for the window");
    assert!(holding.bitmap.has_dirty_pages());
    let ckpts = META_KV_CHECKPOINTS.load(Ordering::Relaxed);
    // The cycle that exists to relieve the pressure COMPLETES.
    vol.checkpoint_now()
        .await
        .expect("a queued delta never fails the checkpoint cycle");
    assert!(META_KV_CHECKPOINTS.load(Ordering::Relaxed) > ckpts);
    assert!(!vol.is_failed(), "the D1.b lattice never fired");
    assert!(!holding.bitmap.has_dirty_pages(), "the pages landed");
    let on_disk = vol
        .read_alloc_bitmap_image(&holding.pages, DATA_BLOCKS)
        .await
        .unwrap();
    let pages = DataAllocBitmap::from_region_image(DATA_TAG, DATA_BLOCKS, &on_disk).unwrap();
    assert!(!pages.is_set(g.start), "the page carries the RAM clear");
    assert_eq!(
        holding.queued_deltas(),
        1,
        "still queued — deferred, not lost"
    );
    assert!(
        holding.stats().deltas_deferred >= 1,
        "the deferral is counted"
    );
    // The window frees: the next cycle's re-kick journals the queue.
    for adm in held {
        core.release(adm);
    }
    vol.checkpoint_now().await.unwrap();
    wait_until("the deferred CLEAR journaled", || {
        holding.queued_deltas() == 0
    })
    .await;
    shutdown(&routed).await;
    reset_process_state();
}

/// Review round 2, Issue 23 (reviewer-reproduced) — the FIRST hold of a
/// data volume's lease is SEEDED from the allocator's derived truth at
/// that instant (every block below the cursor that is not on the free
/// list — the refcount map's population plus every in-limbo, quarantined
/// or grace-held offset — reads SET; the free list's blocks read CLEAR and
/// the list is drained into the bitmap): arming a POPULATED volume with
/// freed-and-reused history re-grants no live block, `block_claim_
/// anomalies` stays flat, the C6/C8 oracle reads clean; and on an armed
/// allocator the flat free-list-first pass is UNREACHABLE — a block
/// planted on the local list is never handed out (the bitmap IS the free
/// list).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn arming_a_populated_volume_seeds_the_bitmap_and_never_regrants_a_live_block() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let a = data_allocator(DATA_ID).await;
    // The pre-arm era: five blocks minted by the shipped loop, two freed
    // (they land on the local free list), one of those reused, one more
    // freed — three live, two free below the cursor.
    let mut minted = Vec::new();
    for _ in 0..5 {
        minted.push(a.allocate_block().await.unwrap() / a.chunk_size());
    }
    for b in [minted[1], minted[3]] {
        assert!(a.begin_free(b * a.chunk_size()));
        a.finish_free(b * a.chunk_size());
    }
    let reused = a.allocate_block().await.unwrap() / a.chunk_size();
    assert!(
        reused == minted[1] || reused == minted[3],
        "the shipped loop reuses the list"
    );
    assert!(a.begin_free(minted[0] * a.chunk_size()));
    a.finish_free(minted[0] * a.chunk_size());
    let live: std::collections::BTreeSet<u64> = a
        .tracked_offsets()
        .into_iter()
        .map(|(o, _)| o / a.chunk_size())
        .collect();
    assert_eq!(live.len(), 3);
    assert_eq!(a.free_blocks_count(), 2);
    let uris = format_stamped_set(dir.path(), 2).await;
    let routed = open_armed(&uris).await;
    let anomalies0 = squeezefs::fuse_client::METRICS
        .block_claim_anomalies
        .load(Ordering::Relaxed);
    assert_eq!(
        alloc_lease::arm_symmetric_allocation(&routed, &[Arc::clone(&a)])
            .await
            .unwrap(),
        1
    );
    let holding = alloc_lease::holding(DATA_TAG).expect("held");
    assert_eq!(
        holding.bitmap.population(),
        live.len() as u64,
        "the first hold is seeded from the derived truth"
    );
    for b in &live {
        assert!(holding.bitmap.is_set(*b), "live block {b} reads CLEAR");
    }
    assert_eq!(
        a.free_blocks_count(),
        0,
        "the local list drained into the bitmap"
    );
    // The seeded pages are on the device BEFORE the record named them.
    let vol = Arc::clone(&routed.volumes[slot0_of(&routed)]);
    let on_disk = vol
        .read_alloc_bitmap_image(&holding.pages, DATA_BLOCKS)
        .await
        .unwrap();
    assert_eq!(
        DataAllocBitmap::from_region_image(DATA_TAG, DATA_BLOCKS, &on_disk)
            .unwrap()
            .set_blocks(),
        live.iter().copied().collect::<Vec<_>>()
    );
    // The first armed mints: blocks the derived allocator holds FREE (the
    // two freed ones, then fresh), never a live one, no anomaly.
    let mut armed = std::collections::BTreeSet::new();
    for _ in 0..8 {
        armed.insert(a.allocate_block().await.unwrap() / a.chunk_size());
    }
    assert!(
        armed.is_disjoint(&live),
        "a live block was re-granted: {armed:?} ∩ {live:?}"
    );
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .block_claim_anomalies
            .load(Ordering::Relaxed),
        anomalies0
    );
    // The oracle: SET ≡ referenced ∪ open grants, nothing referenced clear.
    let referenced: std::collections::BTreeSet<u64> = a
        .tracked_offsets()
        .into_iter()
        .map(|(o, _)| o / a.chunk_size())
        .collect();
    let d = holding
        .bitmap
        .drift(&referenced, &holding.ledger.open_ranges());
    assert!(d.loss.is_empty(), "{:?}", d.loss);
    assert!(d.leak.is_empty(), "{:?}", d.leak);
    assert_eq!(DATA_ALLOC_BITMAP_DRIFT.load(Ordering::Relaxed), 0);
    // The gate: a LIVE block planted on the local free list (the seam that
    // would make the flat pass hand it out) is never minted armed.
    let planted = *live.iter().next().unwrap();
    a.test_plant_free_list(planted);
    for _ in 0..4 {
        let b = a.allocate_block().await.unwrap() / a.chunk_size();
        assert_ne!(
            b, planted,
            "the free-list-first pass ran on an armed allocator"
        );
        assert!(holding.bitmap.is_set(b));
    }
    shutdown(&routed).await;
    reset_process_state();
}

/// Review round 3, Issue 27 — the seed's truth on a FOREST populated
/// through the ROUTED layer: the pre-arm era publishes striped layouts
/// whose references live in the slot trees (PR 7's forest keying); the
/// remount seeds the allocator the way the mount path does (the ONE
/// by-block scan — `block_ref_scan`'s union over the set, PR 7's law 2
/// kept it as the free list's derivation); the first hold is seeded from
/// that state ∪ the durable ledger, every referenced block reads SET and
/// no armed mint lands on one. And the structural guard: an UN-SEEDED
/// allocator (cursor 0) on a set whose ledger names blocks of the volume
/// REFUSES the arm loud — never an all-clear bitmap over live data.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn arming_a_populated_forest_set_seeds_from_the_refs_union_and_refuses_an_unseeded_allocator()
{
    use squeezefs::layout_wire::LayoutMetadata;
    use squeezefs::meta_backend::kv::block_refs::{BlockRef, BlockRefOp};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set(dir.path(), 2).await;
    // The pre-arm era: an UNARMED routed set, blocks minted by the shipped
    // loop and published as striped layouts with their durable references.
    let mut referenced = std::collections::BTreeSet::new();
    {
        let routed = open_unarmed(&uris).await;
        let a0 = data_allocator(DATA_ID).await;
        for i in 0..6 {
            let ino = routed
                .create(ROOT_INO, &format!("f{i}"), libc::S_IFREG | 0o644, 0, 0)
                .await
                .unwrap()
                .ino;
            let offset = a0.allocate_block().await.unwrap();
            a0.publish_block(offset);
            let block_idx = offset / a0.chunk_size();
            let len = 64 * 1024u64;
            let layout = LayoutMetadata {
                file_type: "staged".into(),
                size: len,
                file_id: Some(format!("tenant-{i}")),
                block_map: Some(std::collections::HashMap::from([(
                    0u32,
                    format!("{offset}:0:{len}"),
                )])),
                ..Default::default()
            };
            routed
                .set_layout_and_size(
                    ino,
                    &bincode::serialize(&layout).unwrap(),
                    len,
                    &[BlockRefOp::taken(BlockRef {
                        vol_tag: DATA_TAG,
                        block_idx,
                        owner_ino: ino,
                        block_index: 0,
                    })],
                )
                .await
                .unwrap();
            referenced.insert(block_idx);
        }
        shutdown(&routed).await;
    }
    assert_eq!(referenced.len(), 6);
    let routed = open_armed(&uris).await;
    // (a) The guard: a fresh, UN-SEEDED allocator on this populated set.
    let unseeded = data_allocator(DATA_ID).await;
    match alloc_lease::arm_symmetric_allocation(&routed, &[Arc::clone(&unseeded)]).await {
        Err(e) => assert!(e.to_string().contains("UN-SEEDED"), "{e}"),
        Ok(n) => panic!("an un-seeded allocator seeded an all-clear bitmap ({n} held)"),
    }
    assert!(alloc_lease::holding(DATA_TAG).is_none());
    // (b) The mount path's seed: the ONE by-block scan's union over the
    // set's volumes (every slot tree on a forest), then the arm.
    let a1 = data_allocator(DATA_ID).await;
    let mut indices = Vec::new();
    for kv in &routed.volumes {
        for r in kv.block_ref_scan(DATA_TAG).await.unwrap() {
            indices.push(r.block_idx);
        }
    }
    assert_eq!(
        indices
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>(),
        referenced,
        "the forest's slot trees carry every reference"
    );
    assert_eq!(a1.seed_from_durable_refs(&indices).await, 6);
    assert_eq!(
        alloc_lease::arm_symmetric_allocation(&routed, &[Arc::clone(&a1)])
            .await
            .unwrap(),
        1
    );
    let holding = alloc_lease::holding(DATA_TAG).expect("held");
    assert_eq!(holding.bitmap.population(), 6);
    for b in &referenced {
        assert!(
            holding.bitmap.is_set(*b),
            "referenced block {b} reads CLEAR"
        );
    }
    let mut minted = std::collections::BTreeSet::new();
    for _ in 0..8 {
        minted.insert(a1.allocate_block().await.unwrap() / a1.chunk_size());
    }
    assert!(
        minted.is_disjoint(&referenced),
        "{minted:?} ∩ {referenced:?}"
    );
    assert_eq!(DATA_ALLOC_BITMAP_DRIFT.load(Ordering::Relaxed), 0);
    shutdown(&routed).await;
    reset_process_state();
}

/// Review round 3, Issue 28 — the free-grace valve's supply terms on a
/// GRANT-ARMED allocator read the window's remainder plus (on the holder)
/// the bitmap's clear population — never the drained flat list and the
/// virgin tail, which at a FULL cursor read 0 and would make the valve
/// prod, tighten and fence readers for a writer that is not short of
/// space.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_armed_holders_supply_terms_read_the_window_and_the_clear_population() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set(dir.path(), 1).await;
    let routed = open_armed(&uris).await;
    let a = data_allocator(DATA_ID).await;
    assert_eq!(
        alloc_lease::arm_symmetric_allocation(&routed, &[Arc::clone(&a)])
            .await
            .unwrap(),
        1
    );
    let holding = alloc_lease::holding(DATA_TAG).expect("held");
    let prods = squeezefs::free_grace::prods();
    let tightenings = squeezefs::free_grace::bound_tightenings();
    let fences = squeezefs::free_grace::laggard_fences();
    // Mint the whole volume through grants: the cursor reaches capacity,
    // the virgin tail is 0, the flat list is empty.
    let mut minted = Vec::with_capacity(DATA_BLOCKS as usize);
    for _ in 0..DATA_BLOCKS {
        minted.push(a.allocate_block().await.unwrap());
    }
    assert_eq!(a.highest_block_index(), DATA_BLOCKS);
    assert_eq!(a.free_blocks_count(), 0);
    assert_eq!(holding.bitmap.population(), DATA_BLOCKS);
    assert_eq!(a.free_supply_blocks(), 0, "genuinely full");
    assert_eq!(a.lane_reachable_blocks(), 0);
    // 100 terminal frees: the bits clear at the holder — that IS the
    // supply, though the flat list stays empty and the tail stays 0.
    for off in &minted[..100] {
        assert!(a.begin_free(*off));
        a.finish_free(*off);
    }
    assert_eq!(a.free_blocks_count(), 0);
    assert_eq!(
        a.free_supply_blocks(),
        100,
        "the clear population is the supply"
    );
    assert_eq!(a.lane_reachable_blocks(), 100);
    // One mint carves a grant into the holes: supply = the window's
    // remainder + the clear population, exactly one block fewer.
    let _ = a.allocate_block().await.unwrap();
    let clear = DATA_BLOCKS - holding.bitmap.population();
    assert_eq!(a.free_supply_blocks(), a.block_grant_remaining() + clear);
    assert_eq!(a.free_supply_blocks(), 99);
    assert!(
        a.block_grant_remaining() > 0,
        "a half-consumed grant is supply"
    );
    // No reader plane is armed here, so the valve's rungs are trivially
    // flat — pinned so a future arm cannot make an armed holder's full
    // cursor read as a trough.
    assert_eq!(squeezefs::free_grace::prods(), prods);
    assert_eq!(squeezefs::free_grace::bound_tightenings(), tightenings);
    assert_eq!(squeezefs::free_grace::laggard_fences(), fences);
    shutdown(&routed).await;
    reset_process_state();
}

/// The rebase seam onto PR 7 (round 4): the shared-block index's HOME is
/// resolved behind ONE function, and PR 8 re-points it to the data
/// volume's allocation-lease holder's home volume — a held volume's index
/// home is the holder's `home_vol` (the slot-0 volume this mount is homed
/// on), an unheld volume's is PR 7's default (volume 0).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_shared_index_home_follows_the_allocation_holder() {
    use squeezefs::meta_backend::kv::shared_refs::{index_home_volume, index_home_volume_for};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set(dir.path(), 2).await;
    let routed = open_armed(&uris).await;
    assert_eq!(
        index_home_volume_for(DATA_TAG),
        index_home_volume(),
        "unheld: PR 7's default"
    );
    let a = data_allocator(DATA_ID).await;
    assert_eq!(
        alloc_lease::arm_symmetric_allocation(&routed, &[Arc::clone(&a)])
            .await
            .unwrap(),
        1
    );
    let holding = alloc_lease::holding(DATA_TAG).expect("held");
    assert_eq!(
        index_home_volume_for(DATA_TAG),
        usize::from(holding.home_vol)
    );
    assert_eq!(index_home_volume_for(DATA_TAG), slot0_of(&routed));
    assert_eq!(index_home_volume_for(DATA_TAG + 99), index_home_volume());
    shutdown(&routed).await;
    reset_process_state();
}

/// Review round 2, Issue 24 — the parked-leave refusal is SET-WIDE: on a
/// 2-volume set every volume's `shutdown()` refuses (the latch outlives
/// the gate word `close_at_leave` clears), so every page stays `Live` and
/// the next open of this identity recovers each region as own residue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parked_leave_is_refused_on_every_volume_of_the_set() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set(dir.path(), 2).await;
    let routed = open_armed(&uris).await;
    for i in 0..3 {
        routed
            .create(ROOT_INO, &format!("f{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
    }
    assert_eq!(
        park_gate::fence_at_t_self(FenceClass::SymmetricAppender, 1_000, "manager dead"),
        TSelfAction::Parked
    );
    for v in &routed.volumes {
        match v.shutdown().await {
            Err(KvError::Busy(m)) => assert!(m.contains("PARKED"), "{m}"),
            other => panic!(
                "{}: a parked appender's leave was admitted: {other:?}",
                v.device_path().display()
            ),
        }
    }
    assert!(park_gate::leave_refused());
    drop(routed);
    park_gate::test_reset();
    test_clear_holdings();
    // Every region was left `Live`: the same identity's open recovers each
    // as own residue.
    let again = open_armed(&uris).await;
    for v in &again.volumes {
        let s = v.appender_stats().expect("forest volume");
        assert!(
            s.self_recoveries >= 1,
            "{}: the region was released at the refused leave",
            v.device_path().display()
        );
    }
    assert!(again.lookup(ROOT_INO, "f2").await.is_ok());
    shutdown(&again).await;
    reset_process_state();
}

/// Review round 1, Issue 18 — the LEAVE over a standing park: the clean
/// shutdown is REFUSED loud (the region may be under a successor's
/// recovery, so nothing more is admitted into its ring — the page stays
/// `Live` for the next open's own-residue recovery), the committers
/// parked at the door are failed (never landed), and it is not an expiry
/// (`appender_park_expiries` stays 0).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shutdown_over_a_standing_park_is_refused_and_fails_the_parked_committers() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set(dir.path(), 1).await;
    let routed = open_armed(&uris).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(
        park_gate::fence_at_t_self(FenceClass::SymmetricAppender, 1_000, "manager dead"),
        TSelfAction::Parked
    );
    let r = Arc::clone(&routed);
    let parked = tokio::spawn(async move {
        r.create(ROOT_INO, "never", libc::S_IFREG | 0o644, 0, 0)
            .await
            .map(|i| i.ino)
    });
    tokio::task::yield_now().await;
    assert!(!parked.is_finished());
    match vol.shutdown().await {
        Err(KvError::Busy(m)) => assert!(m.contains("PARKED"), "{m}"),
        other => panic!("a parked appender's leave was admitted: {other:?}"),
    }
    let outcome = tokio::time::timeout(Duration::from_secs(10), parked)
        .await
        .expect("the parked committer is failed at the leave")
        .unwrap();
    assert!(outcome.is_err(), "{outcome:?}");
    assert_eq!(park_gate::expiries(), 0, "a leave is not an expiry");
    assert!(park_gate::is_expired(), "the door is closed");
    assert!(routed.lookup(ROOT_INO, "never").await.is_err());
    drop(vol);
    drop(routed);
    reset_process_state();
}

/// Review round 1, Issue 4 — the `poison` split classifies by OBJECT: on
/// an armed symmetric appender the S9 remote-custody client's `T_self`
/// fence POISONS process data custody exactly as shipped (DMA refused,
/// `membership_self_fences` counts it, the gate untouched), and only the
/// appender's own membership lease PARKS (counted on `appender_parks`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_s9_custody_clients_fence_poisons_while_the_appenders_lease_parks() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set(dir.path(), 1).await;
    let routed = open_armed(&uris).await;
    assert!(park_gate::symmetric_appender_armed());
    // The S9 custody client — its own authority on its own wire.
    let clocks = shipped_clocks();
    let term = squeezefs::dlm::durable_term();
    let authority = squeezefs::data_grant::WriteCustodyOwner::arm(
        "authority-pr8",
        term + 1,
        term,
        clocks.clone(),
        LeaseClock::monotonic(),
        None,
    )
    .expect("the custody authority arms");
    let router = squeezefs::data_grant::AsyncVerbRouter::new().with_custody(Arc::clone(&authority));
    let host = squeezefs::cluster_wire::RpcListener::start_async(
        listener_cfg(),
        SECRET.to_vec(),
        Arc::new(router),
    )
    .expect("the authority listens");
    let custody = squeezefs::data_grant::WriteCustodyClient::connect_with_clock(
        &host.endpoint().to_string(),
        SECRET,
        "node-pr8",
        LeaseClock::monotonic(),
        0,
    )
    .await
    .expect("the custody client joins");
    let fences_before = squeezefs::fuse_client::METRICS
        .membership_self_fences
        .load(Ordering::Relaxed);
    let fence = custody.self_fence("custody renewal failed: connection refused");
    assert!(
        fence.poisoned_data_custody,
        "remote custody POISONS at T_self"
    );
    assert!(!fence.parked);
    assert!(squeezefs::data_custody::poisoned(), "DMA refused");
    assert!(
        !park_gate::is_parked(),
        "the custody object PARKED the appender"
    );
    assert_eq!(park_gate::parks(), 0);
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .membership_self_fences
            .load(Ordering::Relaxed),
        fences_before + 1
    );
    // The appender's OWN membership lease — the one object whose lease is
    // reclaimable under grace — parks, on its own gauge.
    let clock = LeaseClock::monotonic();
    let owner = MembershipOwner::arm("shard-a", 3, 2, shipped_clocks(), clock.clone()).unwrap();
    let grant = match owner.join(join_req("member-x")) {
        JoinOutcome::Granted(g) => g,
        other => panic!("{other:?}"),
    };
    let session = MemberSession::adopt(
        "member-x",
        MemberRole::Writer,
        &grant,
        clock.now_ms(),
        clock,
    );
    let parked = session.self_fence_as(FenceClass::SymmetricAppender, "manager dead");
    assert!(parked.parked);
    assert!(!parked.poisoned_data_custody);
    assert!(!session.fenced());
    assert!(park_gate::is_parked());
    assert_eq!(park_gate::parks(), 1);
    // And `self_fence` on that same session is the SHIPPED terminal fence
    // (the S9 client's delegate) — never the park.
    let terminal = session.self_fence("terminal");
    assert!(terminal.poisoned_data_custody && !terminal.parked);
    assert!(session.fenced());
    drop(custody);
    host.shutdown();
    // The standing park would hold the leave's own commits: release it
    // (the successor's grant) before the clean shutdown.
    assert!(park_gate::release(0, 0));
    shutdown(&routed).await;
    reset_process_state();
}

/// **The D1.b exemption**: `SQUEEZEFS_TIMEOUT=5` and a park of 12 s — the
/// parked commit never reaches `note_journal_failure`, the volume never
/// fail-stops, the barrier's bounded-error timer is suspended, and the
/// commit completes on the release.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_park_longer_than_squeezefs_timeout_never_escalates() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    std::env::set_var("SQUEEZEFS_TIMEOUT", "5");
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_armed(&uris).await;
    std::env::remove_var("SQUEEZEFS_TIMEOUT");
    let vol = Arc::clone(&routed.volumes[0]);
    let now = 1_000u64;
    assert_eq!(
        park_gate::fence_at_t_self(FenceClass::SymmetricAppender, now, "manager dead"),
        TSelfAction::Parked
    );
    let r = Arc::clone(&routed);
    let parked = tokio::spawn(async move {
        r.create(ROOT_INO, "late", libc::S_IFREG | 0o644, 0, 0)
            .await
            .map(|i| i.ino)
    });
    tokio::time::sleep(Duration::from_secs(12)).await;
    assert!(
        !parked.is_finished(),
        "still parked past 2 × SQUEEZEFS_TIMEOUT"
    );
    assert!(!vol.is_failed(), "the D1.b lattice never fired");
    assert!(park_gate::bounded_timers_suspended());
    assert!(park_gate::release(11_000_000_000, 1_000_000));
    let ino = tokio::time::timeout(Duration::from_secs(10), parked)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(routed.lookup(ROOT_INO, "late").await.unwrap().ino, ino);
    assert_eq!(park_gate::expiries(), 0);
    shutdown(&routed).await;
    reset_process_state();
}

/// The checkpoint/flush task CONTINUES through a park: dirty leaves
/// committed before the park are flushed within the ceiling, the
/// checkpoint count advances while parked, and `appender_flush_ceiling_
/// overruns` stays 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_flush_ceiling_holds_through_a_park() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_armed(&uris).await;
    let vol = Arc::clone(&routed.volumes[0]);
    for i in 0..32 {
        routed
            .create(ROOT_INO, &format!("d{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
    }
    let ckpts = META_KV_CHECKPOINTS.load(Ordering::Relaxed);
    assert_eq!(
        park_gate::fence_at_t_self(FenceClass::SymmetricAppender, 5, "manager dead"),
        TSelfAction::Parked
    );
    // Well past the flush ceiling (1,100 ms) — the task ran through the park.
    tokio::time::sleep(Duration::from_millis(2_600)).await;
    assert!(
        META_KV_CHECKPOINTS.load(Ordering::Relaxed) > ckpts,
        "the checkpoint task ran while parked"
    );
    let stats = vol.appender_stats().unwrap();
    assert_eq!(stats.flush_ceiling_overruns, 0);
    assert!(park_gate::release(0, 0));
    shutdown(&routed).await;
    reset_process_state();
}

/// A parked member's grants stay valid — liveness is the death ledger,
/// never a TTL: nothing in the holder's ledger moves while its writer is
/// parked, the writer's remainder is answered verbatim, and only the
/// death record revokes. And a parked lessee keeps its token service
/// (the seam-level stand-in PR 5's plane reads).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parked_members_grants_stay_valid_and_its_token_service_continues() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_armed(&uris).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let me = identity_of(&vol);
    let (holding, _) = take_fresh_lease(&vol, me).await;
    let g = grant_of(
        vol.holder_block_grant(DATA_TAG, "parked-w", 64, 0)
            .await
            .unwrap(),
    );
    assert_eq!(
        park_gate::fence_at_t_self(FenceClass::SymmetricAppender, 9, "manager dead"),
        TSelfAction::Parked
    );
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(
        holding.ledger.grants_of("parked-w"),
        vec![g],
        "no TTL touched it"
    );
    assert!(park_gate::admits_token_service());
    match vol
        .holder_block_grant(DATA_TAG, "parked-w", 64, 64)
        .await
        .unwrap()
    {
        CarveOutcome::Already(gs) => assert_eq!(gs, vec![g]),
        other => panic!("{other:?}"),
    }
    assert!(park_gate::release(0, 0));
    shutdown(&routed).await;
    reset_process_state();
}

/// `data_custody::poison` SPLITS: remote custody poisons at `T_self`, a
/// symmetric appender parks; a park past `T_park_max` expires into the
/// terminal poison (`appender_park_expiries` = 1, the one terminal
/// signal), and an expired gate admits no token service.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn data_custody_poison_splits_by_fence_class() {
    let _g = SEAM.lock().await;
    reset_process_state();
    assert_eq!(
        park_gate::fence_at_t_self(FenceClass::RemoteCustody, 1, "s9 custody"),
        TSelfAction::Poisoned
    );
    assert!(squeezefs::data_custody::poisoned());
    squeezefs::data_custody::test_clear_poison();
    park_gate::arm_symmetric_appender(0, park_gate::t_park_max_ms(46_500, 45_000));
    assert_eq!(park_gate::t_park_max_in_force_ms(), 91_500);
    assert_eq!(
        park_gate::fence_at_t_self(FenceClass::SymmetricAppender, 1_000, "manager dead"),
        TSelfAction::Parked
    );
    assert_eq!(
        park_gate::fence_at_t_self(FenceClass::SymmetricAppender, 2_000, "again"),
        TSelfAction::AlreadyParked
    );
    assert!(!squeezefs::data_custody::poisoned());
    assert!(!park_gate::expire_if_due(1_000 + 91_499, "not yet"));
    assert!(park_gate::expire_if_due(
        1_000 + 91_500,
        "successor never came"
    ));
    assert!(park_gate::is_expired());
    assert!(squeezefs::data_custody::poisoned());
    assert_eq!(park_gate::expiries(), 1);
    assert!(!park_gate::admits_token_service());
    assert!(
        park_gate::pre_admission().await.is_err(),
        "EIO to parked ops"
    );
    reset_process_state();
}

/// The sharded harness (KD-SYM-15, the SIM-1 shape) at the CORE's API: 4
/// shards, member `i` renews with shard `i % 4`; shard 0's manager dies —
/// its 16 members park and every one reclaims in the successor's grace,
/// zero expiries, zero journal entries. The park leg here drives
/// `ParkCore` directly (the production `RenewalTick::Parked` arm is
/// `a_parked_member_reclaims_against_the_successor_the_ledger_names`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sharded_sim_parks_and_reclaims_a_dead_shards_members_at_the_cores_api() {
    let _g = SEAM.lock().await;
    reset_process_state();
    let report = run_sharded(
        SimConfig {
            clients: 64,
            readers_pct: 25,
            beats: 2,
            mode: SimMode::Direct,
            evict_fraction_permille: 0,
            failover: true,
        },
        4,
    )
    .await
    .unwrap();
    assert_eq!(report.shards, 4);
    assert_eq!(report.renewals, 128);
    assert_eq!(report.parked, 16);
    assert_eq!(report.reclaimed, 16);
    assert_eq!(report.park_expiries, 0);
    assert_eq!(
        report.journal_entries_delta, 0,
        "the S6 gate holds per shard"
    );
    assert_eq!(report.census_rows, 64, "`clients` unions the shards");
    assert!(membership::renewal_beat_ms() > 0);
    reset_process_state();
}

// ---------------------------------------------------------------------------
// §5.5 — frees to the right holder; the bitmap vs the census oracle
// ---------------------------------------------------------------------------

/// The free-target MAP names a data volume's holder (the authority stays
/// the route for a volume the plane does not name), and on the holder a
/// terminal free clears the bit at `finish_free` — journaled as a CLEAR
/// delta at once, written to the pages at the checkpoint, which then
/// COVERS it. (The production route — `cowriter::ship_displaced_frees`
/// to the holder's venue, the allocator's own terminal free — is driven
/// by `an_armed_two_writer_set_allocates_disjoint_ranges_from_grants`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_free_target_map_names_the_holder_and_finish_free_clears_its_bit() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    install_free_target(DATA_TAG, "10.0.0.1:7100".to_string());
    assert_eq!(free_target_for(DATA_TAG).as_deref(), Some("10.0.0.1:7100"));
    assert!(
        free_target_for(DATA_TAG + 1).is_none(),
        "the authority's route"
    );
    uninstall_free_target(DATA_TAG);
    assert!(free_target_for(DATA_TAG).is_none());

    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_armed(&uris).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let me = identity_of(&vol);
    let (holding, _) = take_fresh_lease(&vol, me).await;
    let g = grant_of(vol.holder_block_grant(DATA_TAG, "w", 64, 0).await.unwrap());
    // The terminal free of the grant's first block (the allocator's
    // `publish_free_list` hook) clears the bit in RAM; the CLEAR delta
    // and the page ride the holder's checkpoint.
    assert!(alloc_lease::note_finish_free(DATA_TAG, g.start));
    assert!(
        !alloc_lease::note_finish_free(DATA_TAG, g.start),
        "already clear"
    );
    assert!(
        !alloc_lease::note_finish_free(DATA_TAG + 1, g.start),
        "not our volume"
    );
    vol.checkpoint_now().await.unwrap();
    assert_eq!(holding.queued_deltas(), 0);
    assert!(!holding.bitmap.has_dirty_pages());
    let on_disk = vol
        .read_alloc_bitmap_image(&holding.pages, DATA_BLOCKS)
        .await
        .unwrap();
    let loaded = DataAllocBitmap::from_region_image(DATA_TAG, DATA_BLOCKS, &on_disk).unwrap();
    assert!(!loaded.is_set(g.start));
    assert!(loaded.is_set(g.start + 1));
    assert_eq!(loaded.population(), 63);
    // The checkpoint COVERED the deltas (the meta-bitmap law on one
    // device): a replay from the ledger's tail now finds nothing to apply
    // — the pages are the truth past the tail.
    let fresh = DataAllocBitmap::new(DATA_TAG, DATA_BLOCKS);
    assert_eq!(vol.replay_data_alloc_deltas(&fresh, 1).await.unwrap(), 0);
    shutdown(&routed).await;
    reset_process_state();
}

/// The C6/C8 census is the ORACLE: set bits ≡ referenced ∪ open grants; a
/// leak-direction bit is released, a loss-direction finding is reported
/// on the must-stay-0 `data_alloc_bitmap_drift` and never "repaired".
#[test]
fn the_bitmap_agrees_with_the_census_oracle() {
    let bm = DataAllocBitmap::new(DATA_TAG, 512);
    let ledger = BlockGrantLedger::new();
    let g = grant_of(ledger.carve(&bm, "w", 64, 0, 0));
    let referenced: std::collections::BTreeSet<u64> = (g.start..g.start + 10).collect();
    let d = bm.drift(&referenced, &ledger.open_ranges());
    assert!(d.loss.is_empty() && d.leak.is_empty());
    // A stray set bit outside every reference and grant: the LEAK
    // direction — released.
    bm.set(400);
    let d = bm.drift(&referenced, &ledger.open_ranges());
    assert_eq!(d.leak, vec![400]);
    for b in &d.leak {
        assert!(bm.clear(*b));
    }
    let before = DATA_ALLOC_BITMAP_DRIFT.load(Ordering::Relaxed);
    assert_eq!(note_drift(&bm.drift(&referenced, &ledger.open_ranges())), 0);
    assert_eq!(DATA_ALLOC_BITMAP_DRIFT.load(Ordering::Relaxed), before);
    // A referenced block whose bit is CLEAR: the LOSS direction (S1) —
    // the tripwire counts it, the bit is not set behind the census.
    bm.clear(g.start);
    let d = bm.drift(&referenced, &ledger.open_ranges());
    assert_eq!(d.loss, vec![g.start]);
    assert_eq!(note_drift(&d), 1);
    assert_eq!(DATA_ALLOC_BITMAP_DRIFT.load(Ordering::Relaxed), before + 1);
    assert!(!bm.is_set(g.start), "report-only");
}

/// `free_grace_deferrals ≡ releases + offsets` PER RING on two hand-made
/// rings (the per-data-volume shape — one ring per volume, one clock), and
/// the timeout path counts the frees a forced release moved. No shard is
/// stood up here: the rings are the unit the gauge is per.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn free_grace_closure_holds_per_ring_on_two_hand_made_rings() {
    let _g = SEAM.lock().await;
    reset_process_state();
    let ms = Arc::new(std::sync::atomic::AtomicU64::new(1_000));
    let clock = LeaseClock::manual(Arc::clone(&ms));
    let owner =
        MembershipOwner::arm("grace-owner-8", 3, 2, shipped_clocks(), clock.clone()).unwrap();
    membership::install_owner(Arc::clone(&owner));
    let cycle = squeezefs::free_grace::ack_cycle(owner.clocks());
    squeezefs::free_grace::arm_owner_plane_with(clock.clone(), cycle * 10, cycle * 5);
    match owner.join(JoinRequest {
        id: "reader-8".to_string(),
        role: MemberRole::Reader,
        endpoint: None,
        pid: std::process::id(),
        boot: "boot-pr8".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    }) {
        JoinOutcome::Granted(_) => {}
        other => panic!("{other:?}"),
    }
    owner.refresh_free_grace_bound();
    let ring_a = squeezefs::free_grace::GraceRing::new(3);
    let ring_b = squeezefs::free_grace::GraceRing::new(64);
    for i in 0..3u64 {
        assert!(ring_a.defer(i * 4096, 4096));
    }
    for i in 0..5u64 {
        assert!(ring_b.defer(i * 4096, 4096));
    }
    // Ring B is far inside the bound: a routine harvest releases nothing.
    assert!(ring_b.harvest(64).is_empty());
    // Ring A is AT its cap: the harvest forces progress — the TIMEOUT
    // path (the laggard reader is fenced, which unbounds the plane).
    let released_a = ring_a.harvest(64);
    assert!(!released_a.is_empty());
    assert_eq!(ring_a.timeout_deferrals(), released_a.len() as u64);
    // With the laggard gone ring B's next harvest is ROUTINE: releases,
    // no timeout deferral.
    assert_eq!(ring_b.harvest(64).len(), 5);
    assert_eq!(ring_b.timeout_deferrals(), 0);
    for (ring, name) in [(&ring_a, "a"), (&ring_b, "b")] {
        assert_eq!(
            ring.deferrals(),
            ring.releases() + ring.len() as u64,
            "closure on ring {name}"
        );
    }
    assert_eq!(ring_a.deferrals(), 3);
    assert_eq!(ring_b.deferrals(), 5);
    squeezefs::free_grace::disarm_owner_plane();
    membership::uninstall();
    reset_process_state();
}

// ---------------------------------------------------------------------------
// §5.5 — the coordinator, the lessee shards, the vol-0 rule
// ---------------------------------------------------------------------------

/// Under the plane the maintenance coordinator is volume 0's MANAGER: the
/// armed manager coordinates, a reader of the same set is refused naming
/// it; each mount's inode-plane coverage is `leased ∪ unleased` (every
/// hosted slot on the solo manager, `foreign == 0`), and a slot's ino is
/// this mount's to judge iff it leases the slot or manages it unleased.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_coordinator_is_volume_zeros_manager_and_coverage_is_leased_union_unleased() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_armed(&uris).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert!(alloc_lease::symmetric_coordinator_refusal().is_none());
    assert!(squeezefs::jobs::maintenance_coordinator_refusal().is_none());
    let cov = vol.inode_plane_slot_coverage().await.unwrap();
    assert_eq!(cov.covered, cov.leased + cov.unleased);
    assert_eq!(cov.foreign, 0, "a solo manager leaves nothing to a lessee");
    assert!(cov.leased >= 65, "native + 64 rotor");
    assert!(cov.covered >= cov.leased);
    // A leased slot's ino and an unleased slot's ino are both this
    // mount's; a foreign lessee's would not be (none exists solo).
    let f = routed
        .create(ROOT_INO, "in-a-rotor", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let local = routed.route_ino(f.ino).1;
    assert!(vol.inode_plane_owns_slot(local));
    assert!(vol.inode_plane_owns_slot(squeezefs::meta_backend::guest_local_ino(4000, 7)));
    // A reader of the set is NOT the coordinator.
    let reader = open_routed_meta_set_read_only(&uris).await.unwrap();
    alloc_lease::install_coordinator_volume(Some(&reader.volumes[0]));
    let refusal = alloc_lease::symmetric_coordinator_refusal().expect("a reader refuses");
    assert!(refusal.contains("VOLUME 0's MANAGER"), "{refusal}");
    assert!(squeezefs::jobs::maintenance_coordinator_refusal().is_some());
    for v in &reader.volumes {
        v.shutdown().await.unwrap();
    }
    alloc_lease::install_coordinator_volume(Some(&vol));
    assert!(alloc_lease::symmetric_coordinator_refusal().is_none());
    shutdown(&routed).await;
    assert!(
        alloc_lease::symmetric_coordinator_refusal().is_none(),
        "the leave withdraws the home: an unarmed process coordinates"
    );
    reset_process_state();
}

/// The §5.5.2 vol-0 rule, WIRED: a manager whose probe of volume 0's
/// ledger fails for longer than `T_owner` releases its role — the lease
/// word reads `vacant`, `manager_vol0_unreachable` counts, the coordinator
/// predicate refuses; a reachable probe in between resets the clock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_vol0_unreachable_releases_the_role() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_armed(&uris).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let t_owner = 45_000;
    assert!(!vol.note_vol0_ledger_probe(false, 1_000, t_owner));
    assert!(
        !vol.note_vol0_ledger_probe(false, 1_000 + t_owner, t_owner),
        "not yet >"
    );
    assert!(
        !vol.note_vol0_ledger_probe(true, 2_000 + t_owner, t_owner),
        "reset"
    );
    assert!(!vol.note_vol0_ledger_probe(false, 3_000 + t_owner, t_owner));
    assert!(!vol.note_vol0_ledger_probe(false, 3_000 + 2 * t_owner, t_owner));
    assert!(vol.note_vol0_ledger_probe(false, 3_001 + 2 * t_owner, t_owner));
    assert_eq!(vol.appender_stats().unwrap().manager_lease.word(), "vacant");
    assert_eq!(vol.appender_stats().unwrap().vol0_unreachable, 1);
    assert!(alloc_lease::symmetric_coordinator_refusal().is_some());
    assert!(
        !vol.note_vol0_ledger_probe(false, 4_000 + 3 * t_owner, t_owner),
        "once"
    );
    shutdown(&routed).await;
    reset_process_state();
}

// ---------------------------------------------------------------------------
// §6.3 — the wire: PR 8's verbs, RecordDeath reserved
// ---------------------------------------------------------------------------

const SECRET: &[u8] = b"sym-block-grant-tests-enroll-secret";

fn listener_cfg() -> squeezefs::cluster_wire::RpcListenerConfig {
    squeezefs::cluster_wire::RpcListenerConfig {
        bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
        service_threads: 2,
        ..squeezefs::cluster_wire::RpcListenerConfig::default()
    }
}

/// The PR-8 frames round-trip under the unreleased schema, the documented
/// verb codes sit in the assigned range, and over the wire: a grant to a
/// wire writer is served by the holder, a successor before `recovered:`
/// is answered `Deferred` (the typed retry class), `RecordRecovered`
/// lands idempotently, and `RecordDeath` is REFUSED naming PR 10.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_wire_serves_pr8s_verbs_and_record_death_is_reserved_for_pr10() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    assert_eq!(VERB_CODE_BLOCK_GRANT, 0x80);
    assert_eq!(VERB_CODE_RECORD_DEATH, 0x86);
    let req = ManagerRequestFrame {
        schema: MANAGER_SCHEMA,
        request_id: 8,
        volume: 0,
        call: ManagerCall::BlockGrant {
            vol_tag: DATA_TAG,
            writer: wire(successor_identity()),
            want: 64,
            held_unconsumed: 0,
        },
    };
    assert_eq!(decode_request(&encode_request(&req).unwrap()).unwrap(), req);
    let rep = ManagerReplyFrame {
        schema: MANAGER_SCHEMA,
        request_id: 8,
        reply: ManagerReply::AllocLeaseGranted {
            term: 2,
            already: false,
            predecessor_bitmap: vec![(0, (4096, 65536))],
            predecessor_blocks: DATA_BLOCKS,
        },
    };
    assert_eq!(decode_reply(&encode_reply(&rep).unwrap()).unwrap(), rep);

    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_armed(&uris).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let me = identity_of(&vol);
    let (holding, _) = take_fresh_lease(&vol, me).await;
    let host = squeezefs::cluster_wire::RpcListener::start_async(
        listener_cfg(),
        SECRET.to_vec(),
        ManagerService::new(Arc::clone(&vol)),
    )
    .expect("manager listener");
    let mut client = ManagerClient::connect(&host.endpoint().to_string(), SECRET, "peer-8", 0)
        .await
        .expect("storage-trust enrollment");
    let w = wire(successor_identity());
    let grants = client
        .block_grant(DATA_TAG, w, 64, 0)
        .await
        .unwrap()
        .expect("granted");
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].len, 64);
    assert_eq!(holding.ledger.grants_of(&wire_writer_name(&w)), grants);
    let again = client
        .block_grant(DATA_TAG, w, 64, 64)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again, grants, "a covered remainder is answered verbatim");
    assert_eq!(
        client.return_blocks(DATA_TAG, w, grants[0]).await.unwrap(),
        Some(64)
    );
    // A live holder refuses a foreign acquire; after the (in-process)
    // death record it DEFERS until `recovered:`; then it grants.
    let succ = wire(AppenderIdentity {
        node_token: 0x5ECC_0000_0000_0003,
        mount_slot: 3,
        writer_id: 3,
    });
    assert!(client
        .alloc_lease_acquire(DATA_TAG, succ, 1, 0, ROOT_INO, DATA_BLOCKS)
        .await
        .is_err());
    vol.record_death(me, 9).await.unwrap();
    match client
        .alloc_lease_acquire(DATA_TAG, succ, 1, 0, ROOT_INO, DATA_BLOCKS)
        .await
        .unwrap()
    {
        ManagerReply::Deferred { reason } => assert!(reason.contains("recovered"), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert!(!client.record_recovered(wire(me), 0).await.unwrap());
    assert!(client.record_recovered(wire(me), 0).await.unwrap());
    match client
        .alloc_lease_acquire(DATA_TAG, succ, 1, 0, ROOT_INO, DATA_BLOCKS)
        .await
        .unwrap()
    {
        ManagerReply::AllocLeaseGranted { term, already, .. } => {
            assert_eq!(term, 2);
            assert!(!already);
        }
        other => panic!("{other:?}"),
    }
    // Review round 1, Issue 6 — PR 3/4's bounded-execution law on the
    // PR-8 verbs: every wire integer and ref is validated against
    // durable / derived state BEFORE any effect. `blocks` above the data
    // volume's block count (or `u64::MAX` — the successor would size its
    // allocations by it), an unknown data volume, a `home_vol` outside
    // the set, a bitmap ref outside the home volume's heap / unaligned /
    // short: each REJECTED on `manager_verb_rejected`, nothing written.
    let rejected_before = vol.appender_stats().unwrap().manager_verb_rejected;
    let newcomer = wire(AppenderIdentity {
        node_token: 0x5ECC_0000_0000_0004,
        mount_slot: 4,
        writer_id: 4,
    });
    let mut rejections = 0u64;
    for (tag, blocks, home_vol) in [
        (DATA_TAG, u64::MAX, 0u16),
        (DATA_TAG, DATA_BLOCKS + 1, 0),
        (DATA_TAG, DATA_BLOCKS, 9),
        (DATA_TAG + 99, DATA_BLOCKS, 0),
    ] {
        match client
            .alloc_lease_acquire(tag, newcomer, 1, home_vol, ROOT_INO, blocks)
            .await
        {
            Err(e) => assert!(e.to_string().contains("REJECTED"), "{e}"),
            other => panic!("an unvalidated wire integer reached an effect: {other:?}"),
        }
        rejections += 1;
    }
    assert!(vol
        .alloc_lease_record(DATA_TAG + 99)
        .await
        .unwrap()
        .is_none());
    let rec_before = vol.alloc_lease_record(DATA_TAG).await.unwrap().unwrap();
    assert_eq!(rec_before.holder, succ.into());
    let heap = vol.superblock().heap;
    let node = vol.node_cache().config().layout.node_size() as u64;
    for bad in [
        (0u16, (heap.end(), node)),  // past the heap
        (0, (heap.start + 1, node)), // unaligned
        (0, (heap.start, node / 2)), // not a node multiple
        (0, (heap.start, 0)),        // empty
        (0, (u64::MAX - 8, node)),   // overflow
        (9, (heap.start, node)),     // a volume the set lacks
    ] {
        match client
            .alloc_lease_bitmap(DATA_TAG, succ, 2, vec![bad])
            .await
        {
            Err(e) => assert!(e.to_string().contains("REJECTED"), "{bad:?}: {e}"),
            other => panic!("an unvalidated bitmap ref reached the record: {bad:?} {other:?}"),
        }
        rejections += 1;
    }
    assert_eq!(
        vol.alloc_lease_record(DATA_TAG).await.unwrap().unwrap(),
        rec_before,
        "a rejected frame wrote nothing"
    );
    assert_eq!(
        vol.appender_stats().unwrap().manager_verb_rejected,
        rejected_before + rejections
    );
    // RecordDeath is RESERVED for PR 10's driver.
    match client.record_death(succ, 1).await.unwrap() {
        ManagerReply::Refused { reason } => assert!(reason.contains("PR 10"), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert!(
        vol.dead_member_record(&succ.into())
            .await
            .unwrap()
            .is_none(),
        "the wire wrote nothing"
    );
    drop(client);
    host.shutdown();
    shutdown(&routed).await;
    reset_process_state();
}

// ---------------------------------------------------------------------------
// The negative contract — `=0` and a flat volume are the shipped posture
// ---------------------------------------------------------------------------

/// `SQUEEZEFS_SYMMETRIC_META=0` (the dark forest) and a bit-17-absent
/// volume: no holding, no park posture, no coordinator home, no shard,
/// every gauge 0/absent — the S9 lane path untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn symmetric_meta_off_is_the_shipped_allocation_plane() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    for (name, stamped) in [("forest0", true), ("flat0", false)] {
        let uris = vec![if stamped {
            format_stamped_member(dir.path(), name).await
        } else {
            format_flat_member(dir.path(), name).await
        }];
        let routed = open_unarmed(&uris).await;
        let vol = Arc::clone(&routed.volumes[0]);
        assert!(holdings().is_empty());
        assert!(!park_gate::symmetric_appender_armed());
        assert!(!park_gate::is_parked());
        assert_eq!(park_gate::t_park_max_in_force_ms(), 0);
        assert!(alloc_lease::symmetric_coordinator_refusal().is_none());
        assert_eq!(membership::membership_shards(), 0);
        assert!(vol.alloc_lease_record(DATA_TAG).await.unwrap().is_none());
        assert!(vol.inode_plane_owns_slot(squeezefs::meta_backend::guest_local_ino(4000, 7)));
        let cov = vol.inode_plane_slot_coverage().await.unwrap();
        assert_eq!(cov.foreign, 0);
        assert_eq!(cov.covered, cov.leased + cov.unleased);
        // The shipped Writer fence is the POISON (no park posture armed).
        let clock = LeaseClock::monotonic();
        let owner =
            MembershipOwner::arm("solo-owner", 3, 2, shipped_clocks(), clock.clone()).unwrap();
        let grant = match owner.join(join_req("solo-w")) {
            JoinOutcome::Granted(g) => g,
            other => panic!("{other:?}"),
        };
        let session =
            MemberSession::adopt("solo-w", MemberRole::Writer, &grant, clock.now_ms(), clock);
        let fence = session.self_fence("renewal failed");
        assert!(!fence.parked);
        assert!(fence.poisoned_data_custody);
        assert!(squeezefs::data_custody::poisoned());
        squeezefs::data_custody::test_clear_poison();
        shutdown(&routed).await;
    }
    reset_process_state();
}

/// The bitmap's derivation: 32 KiB per TiB at the shipped 4 MiB block.
#[test]
fn the_data_bitmap_is_thirty_two_kib_per_tib() {
    assert_eq!(
        squeezefs::data_alloc_bitmap::bitmap_bytes_for(1 << 40, 4 << 20),
        32 * 1024
    );
    assert_eq!(squeezefs::data_alloc_bitmap::pages_for(DATA_BLOCKS), 1);
}
