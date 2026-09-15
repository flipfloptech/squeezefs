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
    self, JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MemberSession,
    MembershipOwner,
};
use squeezefs::membership_sim::{run_sharded, SimConfig, SimMode};
use squeezefs::meta_backend::kv::alloc_lease::{
    self, dead_member_key, holdings, recovered_key, test_clear_holdings, AllocLeaseRecord,
};
use squeezefs::meta_backend::kv::appender::AppenderIdentity;
use squeezefs::meta_backend::kv::backend::{
    test_conveyor_hold_release, KvMetaBackend, TEST_CONVEYOR_HOLD_PRE_FANOUT,
    TEST_CONVEYOR_HOLD_STAGE,
};
use squeezefs::meta_backend::kv::builder::{format_v3_stamped, FormatV3Options, ROOT_INO};
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

/// Reset every process-global the contracts touch (the holdings, the free
/// targets, the park gate, custody) — a fresh mount in one binary.
fn reset_process_state() {
    test_clear_holdings();
    squeezefs::data_alloc_bitmap::test_clear_replayed_deltas();
    test_clear_free_targets();
    park_gate::test_reset();
    squeezefs::data_custody::test_clear_poison();
    std::env::remove_var("SQUEEZEFS_TIMEOUT");
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
    vol: &KvMetaBackend,
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

/// **The case that matters**: the holder journals a `BlockGrant` SET and
/// dies before its next page write (the pages still show the range FREE).
/// The home manager's ring replay applies the delta to the pages BEFORE
/// `recovered:` is written; the successor cannot be granted the lease
/// before that; it reads pages that show the range SET and never carves
/// inside it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grant_journaled_before_a_holder_death_is_never_regranted() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let (dead, refs, granted) = {
        let routed = open_armed(&uris).await;
        let vol = Arc::clone(&routed.volumes[0]);
        let me = identity_of(&vol);
        let (holding, refs) = take_fresh_lease(&vol, me).await;
        // The pages on the device are the FRESH all-clear image.
        let on_disk = vol
            .read_alloc_bitmap_image(&holding.pages, DATA_BLOCKS)
            .await
            .unwrap();
        assert_eq!(
            DataAllocBitmap::from_region_image(DATA_TAG, DATA_BLOCKS, &on_disk).population(),
            0
        );
        let g = grant_of(vol.holder_block_grant(DATA_TAG, "w", 64, 0).await.unwrap());
        // The crash: no shutdown, no leave, no checkpoint — the SET
        // deltas are in the ring, the holding's RAM dies with the mount.
        drop(vol);
        drop(routed);
        test_clear_holdings();
        park_gate::test_reset();
        (me, refs, g)
    };
    // The home manager remounts (its own-residue recovery replays ring 0
    // for the trees; the bitmap arm is PR 10's driver, driven here).
    let routed = open_armed(&uris).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let extents: Vec<ExtentRef> = refs.iter().map(|(_, e)| *e).collect();
    let stale = vol
        .read_alloc_bitmap_image(&extents, DATA_BLOCKS)
        .await
        .unwrap();
    let pages = DataAllocBitmap::from_region_image(DATA_TAG, DATA_BLOCKS, &stale);
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
    assert!(!vol.record_death(dead, 77).await.unwrap());
    assert!(vol.record_death(dead, 77).await.unwrap(), "idempotent");
    assert_eq!(
        alloc_lease::DEAD_MEMBERS_RECORDED.load(Ordering::Relaxed),
        1
    );
    match vol
        .manager_alloc_lease_acquire(DATA_TAG, succ, 1, 0, ROOT_INO, DATA_BLOCKS)
        .await
    {
        Err(KvError::LeaseDeferred(m)) => assert!(m.contains("not yet recovered"), "{m}"),
        other => panic!("a successor was elected before `recovered:`: {other:?}"),
    }
    // The recovery arm: the dead holder's ring replayed onto its pages —
    // the window's deltas were KEPT across the remount's own bring-up
    // checkpoint (which advanced the tail past them).
    assert!(squeezefs::data_alloc_bitmap::replayed_deltas_kept() >= 64);
    let changed = vol.replay_data_alloc_deltas(&pages).await.unwrap();
    assert_eq!(changed, 64, "the journaled SET landed on the pages");
    assert_eq!(squeezefs::data_alloc_bitmap::replayed_deltas_kept(), 0);
    for blk in granted.start..granted.end() {
        assert!(pages.is_set(blk));
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
        set_record(DATA_TAG, 200, 10),
        clear_record(DATA_TAG, 200, 11),
        set_record(DATA_TAG, 200, 12),
        clear_record(DATA_TAG, 201, 13),
    ];
    // Row 3/7: the replay is idempotent — run twice, same bits.
    for _ in 0..2 {
        pred.replay(recs.iter().map(|(t, r)| (*t, r)));
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
    let loaded = DataAllocBitmap::from_region_image(DATA_TAG, DATA_BLOCKS, &region);
    assert_eq!(loaded.population(), pred.population());
    assert!(decode_data_alloc_page(&torn, DATA_TAG, 0).is_err());
    // Row 8: the first successor copies (generation 7) and dies before
    // its refs land; the next successor copies AGAIN from the still-
    // Recovered image — the same bits, and no carve inside the
    // referenced set.
    let copy1 =
        DataAllocBitmap::from_region_image(DATA_TAG, DATA_BLOCKS, &loaded.region_image(7).unwrap());
    let copy2 =
        DataAllocBitmap::from_region_image(DATA_TAG, DATA_BLOCKS, &loaded.region_image(8).unwrap());
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

/// A dead holder's OUTSTANDING grants (the unpublished remainder a zombie
/// may still DMA into) are revoked from the ledger by the death record
/// and QUARANTINED on the successor's allocator under the dead epoch (S7)
/// — never carved again until a drain proof; the records themselves are
/// idempotent and keyed by the KD-MW-2 identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn holder_failover_keeps_the_dead_writers_grants_quarantined() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_armed(&uris).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let me = identity_of(&vol);
    let (holding, _) = take_fresh_lease(&vol, me).await;
    let w = successor_identity();
    let name = wire_writer_name(&wire(w));
    let g = grant_of(
        vol.holder_block_grant(DATA_TAG, &name, 64, 0)
            .await
            .unwrap(),
    );
    // The death ledger names the writer: its grants leave the table for
    // the quarantine; the bits stay SET (a clear waits on the drain
    // proof).
    assert!(!vol.record_death(w, 5).await.unwrap());
    let revoked = holding.ledger.revoke_dead(&name);
    assert_eq!(revoked, vec![g]);
    assert!(holding.ledger.revoke_dead(&name).is_empty(), "idempotent");
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
    alloc_lease::note_dead_member_acted(vol.dead_member_record(&w).await.unwrap().unwrap().ts_ms);
    assert_eq!(alloc_lease::DEAD_MEMBERS_ACTED.load(Ordering::Relaxed), 1);
    // The records' keys are the identity pair, and volume-qualified for
    // `recovered:`.
    assert_ne!(dead_member_key(&w), dead_member_key(&me));
    assert_ne!(recovered_key(&w, 0), recovered_key(&w, 1));
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

/// **The membership half of the home-shard failover**: a Writer member of
/// a symmetric appender passes `T_self` with its manager dead — it PARKS
/// (nothing poisoned, the session not fenced), a commit admitted before
/// the park lands with its ACK HELD, admission waits; the successor arms
/// and opens grace, the member reclaims with its epoch, the held ack
/// releases and admission resumes; `appender_park_expiries == 0`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn home_shard_members_park_and_reclaim_across_a_manager_failover() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_armed(&uris).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert!(
        park_gate::symmetric_appender_armed(),
        "the armed open arms the park posture"
    );
    assert!(
        park_gate::t_park_max_in_force_ms() > 30_000,
        "T_park_max > SQUEEZEFS_TIMEOUT"
    );

    // The shard: owner A, our member.
    let clock = LeaseClock::monotonic();
    let owner_a = MembershipOwner::arm("shard-a", 3, 2, shipped_clocks(), clock.clone()).unwrap();
    let grant = match owner_a.join(join_req("member-x")) {
        JoinOutcome::Granted(g) => g,
        other => panic!("{other:?}"),
    };
    let session = Arc::new(MemberSession::adopt(
        "member-x",
        MemberRole::Writer,
        &grant,
        clock.now_ms(),
        clock.clone(),
    ));
    let epoch = session.epoch();
    // A commit lands and is held at the lane's pre-fanout seam; THEN the
    // park is raised, so its ack is HELD by the park (in-flight entries
    // land, acks wait).
    TEST_CONVEYOR_HOLD_STAGE.store(TEST_CONVEYOR_HOLD_PRE_FANOUT, Ordering::SeqCst);
    let r1 = Arc::clone(&routed);
    let held = tokio::spawn(async move {
        r1.create(ROOT_INO, "held", libc::S_IFREG | 0o644, 0, 0)
            .await
            .map(|i| i.ino)
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!held.is_finished(), "the lane is at the seam");
    // The manager dies; T_self passes: the member's fence is the PARK.
    drop(owner_a);
    let fence = session.self_fence("renewal failed: connection refused");
    assert!(fence.parked);
    assert!(!fence.poisoned_data_custody);
    assert!(!session.fenced(), "a parked session is not fenced");
    assert!(park_gate::is_parked());
    assert!(!squeezefs::data_custody::poisoned());
    assert_eq!(park_gate::parks(), 1);
    // Release the seam: the lane reaches the park's ack hold.
    TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
    test_conveyor_hold_release();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!held.is_finished(), "the landed entry's ack is HELD");
    assert!(park_gate::acks_held() >= 1);
    // A new commit waits at the pre-admission door — never escalates.
    let r2 = Arc::clone(&routed);
    let parked = tokio::spawn(async move {
        r2.create(ROOT_INO, "parked", libc::S_IFREG | 0o644, 0, 0)
            .await
            .map(|i| i.ino)
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!parked.is_finished());
    assert!(!vol.is_failed(), "a park is not a fail-stop");
    // Reads keep serving and the token service continues.
    assert!(routed.lookup(ROOT_INO, "nothing").await.is_err());
    assert!(park_gate::admits_token_service());
    // The successor arms for the shard and opens grace; the member
    // reclaims with the epoch it holds.
    let owner_b = MembershipOwner::arm("shard-b", 4, 3, shipped_clocks(), clock.clone()).unwrap();
    owner_b.open_grace(vec!["member-x".to_string()]);
    let mut reclaim = join_req("member-x");
    reclaim.prior_epoch = Some(epoch);
    let t0 = std::time::Instant::now();
    match owner_b.join(reclaim) {
        JoinOutcome::Granted(_) => {}
        other => panic!("a reclaim in grace was refused: {other:?}"),
    }
    assert!(park_gate::release(
        park_gate::parked_for_ms(clock.now_ms()) * 1_000_000,
        t0.elapsed().as_nanos() as u64
    ));
    let held_ino = tokio::time::timeout(Duration::from_secs(10), held)
        .await
        .expect("the held ack releases on the grant")
        .unwrap()
        .unwrap();
    let parked_ino = tokio::time::timeout(Duration::from_secs(10), parked)
        .await
        .expect("the parked commit admits on the grant")
        .unwrap()
        .unwrap();
    assert!(held_ino < parked_ino, "journal order: the held ack first");
    assert_eq!(routed.lookup(ROOT_INO, "held").await.unwrap().ino, held_ino);
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

/// The sharded harness (KD-SYM-15, the SIM-1 shape): 4 shards, member
/// `i` renews with shard `i % 4`; shard 0's manager dies — its 16 members
/// park and every one reclaims in the successor's grace, zero expiries,
/// zero journal entries.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn membership_shards_by_home_volume_and_a_dead_shards_members_park_and_reclaim() {
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

/// A data volume's terminal frees route to THAT volume's allocation
/// holder (the free target the plane learns from `alloc_lease:`), the
/// authority stays the route for a volume the plane does not name, and on
/// the holder a terminal free clears the bit at `finish_free` — journaled
/// as a CLEAR delta and written to the pages at the checkpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn frees_route_to_the_volumes_allocation_holder_and_clear_its_bit_at_finish_free() {
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
    assert_eq!(holding.pending_clears(), 1);
    vol.checkpoint_now().await.unwrap();
    assert_eq!(holding.pending_clears(), 0);
    assert!(!holding.bitmap.has_dirty_pages());
    let on_disk = vol
        .read_alloc_bitmap_image(&holding.pages, DATA_BLOCKS)
        .await
        .unwrap();
    let loaded = DataAllocBitmap::from_region_image(DATA_TAG, DATA_BLOCKS, &on_disk);
    assert!(!loaded.is_set(g.start));
    assert!(loaded.is_set(g.start + 1));
    assert_eq!(loaded.population(), 63);
    // The checkpoint COVERED the deltas (the meta-bitmap law on one
    // device): a replay from the ledger's tail now finds nothing to apply
    // — the pages are the truth past the tail.
    let fresh = DataAllocBitmap::new(DATA_TAG, DATA_BLOCKS);
    assert_eq!(vol.replay_data_alloc_deltas(&fresh).await.unwrap(), 0);
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

/// `free_grace_deferrals ≡ releases + offsets` PER VOLUME with two rings
/// (two data volumes, one clock), and the timeout path counts the frees a
/// forced release moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn free_grace_closure_holds_per_volume_with_two_shards() {
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
