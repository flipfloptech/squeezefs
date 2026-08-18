//! **The AUTHORITY ASSEMBLER** — DLM S11 rung 17 (KD-MW-8 revised,
//! `docs/design-full-multi-writer.md` §9.3 + PR-plan row 17; the shipped
//! era-gate/witness shape inherited verbatim from
//! `docs/design-mw-layout-versions.md` §6a).
//!
//! # What this suite pins (each red-first against the rung-16 tip)
//!
//! 1. **`WriteExtent` (publish schema 6)** joins the RETRIED class —
//!    stated against `publish.rs`'s no-retry doctrine: era-gated
//!    (`PUBLISH_STALE_LEASE` before the window), witnessed
//!    (`(lease_epoch, request_id)` through the owner's dedup window,
//!    `extent_replays` counted), served through the installed assembler
//!    executor (`extent_served`).
//! 2. **The demotion barrier** (§9.3): a second holder's block-sharing
//!    range acquire PARKS; the grant is withheld until the incumbent's
//!    renewal-carried notice is ACKED (or its lease expires on the
//!    OWNER's clock); the closed ledger `demotions ≡ acks +
//!    fence_resolves` holds on every posture;
//!    `range_custody_demotion_fenced_publishes` stays 0 on the clean
//!    path; post-demotion BOTH holders' block writes classify
//!    extent-ship (`span_range_shared` true — the rung-16 clause's
//!    firing venue: `patch_ineligible_range_shared` and
//!    `overlay_ineligible_range_shared` move).
//! 3. **The in-flight-renewal race pin**: the notice is composed under
//!    the SAME `FileCustody` serialization that parked B — a renewal
//!    reply composed after the pending-mark always carries it; one
//!    composed before simply pushes it to the next renewal (still inside
//!    the bound; B's grant never issues before the ack).
//! 4. **Extent retention + the FOUR pull release paths**, one red-first
//!    case each: ack-carried `covering_version`; renewal observation;
//!    `FlushExtents` (the fsync force); the at-budget W2 spill (rides
//!    `parked_extent_bytes` — spills instead of blocking).
//! 5. **MW-10/11/13 crash windows**: a surviving client's un-acked /
//!    un-covered extents re-ship idempotently; an authority death
//!    between ACK and PUBLISH loses no acked-fsynced bytes (retention
//!    releases on covering-version VISIBILITY, never on ack); an
//!    authority death MID-DEMOTION dies cleanly — A re-asserts its
//!    ORIGINAL grant in the successor's grace window and the demotion
//!    restarts from zero.
//! 6. **The concurrent same-ino publish composition** (rung 15's
//!    standing-red gate, adjudicated wholly to this rung): a SHIPPED
//!    layout merge chains onto the durable head instead of refusing (or
//!    re-basing with the co-writer's private full layout — the clobber
//!    mint), the reply carries the staged link's version so a co-writer
//!    chains without a refetch, and the LOCAL publish path keeps the
//!    version gate byte-identical (solo untouched).
//!
//! **No numbers here — ruling D11.** The live acceptance is the
//! `s11-range` composition gate flipping green (`tests/run_mw_matrix.sh`);
//! this file is the deterministic cargo port per the repro-port mandate.

use squeezefs::data_custody;
use squeezefs::data_grant::{self, WriteCustodyClient, WriteCustodyOwner};
use squeezefs::extent_ship;
use squeezefs::fuse_client::METRICS;
use squeezefs::layout_wire::{LayoutDelta, LayoutMetadata};
use squeezefs::membership::{LeaseClock, LeaseClocks};
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::meta_backend::{open_routed_meta_set, plan_meta_slot_set, RoutedMetaBackend};
use squeezefs::meta_ship::{self as ship, publish, OwnerMap, PeerOwner};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

const SECRET: &[u8] = b"s11-authority-assembler-storage-trust-secret";
const VOL_LEN: u64 = 256 * 1024 * 1024;
const NODE_A: &str = "node-assembler-a";
const BLOCK: u64 = 4 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Serialization + posture restoration (process-global state everywhere)
// ---------------------------------------------------------------------------

static SERIAL_HELD: AtomicBool = AtomicBool::new(false);

struct Serial;

fn serial() -> Serial {
    while SERIAL_HELD
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        std::thread::yield_now();
    }
    Serial
}

impl Drop for Serial {
    fn drop(&mut self) {
        SERIAL_HELD.store(false, Ordering::Release);
    }
}

struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        data_grant::uninstall_custody_client();
        data_grant::uninstall_custody_owner();
        publish::uninstall_client();
        publish::uninstall_extent_merge_executor();
        publish::uninstall_extent_flush_executor();
        publish::uninstall_served_layout_invalidation();
        ship::disarm_ownership();
        data_custody::test_reset_custody_generation();
        data_custody::test_clear_poison();
        extent_ship::test_reset();
        extent_ship::test_swap_retention_budget(None);
        extent_ship::uninstall_spill_sink();
        extent_ship::uninstall_quiesce_hook();
    }
}

fn restore() -> Restore {
    Restore
}

// ---------------------------------------------------------------------------
// Fixtures (the mw_publish_era_gate_tests shapes)
// ---------------------------------------------------------------------------

fn make_file(dir: &Path, name: &str, len: u64) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    p
}

fn opts() -> squeezefs::meta_backend::kv::builder::FormatV3Options {
    squeezefs::meta_backend::kv::builder::FormatV3Options {
        node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
        journal_len_override: None,
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// One opened single-volume set; `stamp` engages bit 15 (the version
/// gate) between format and open — the mw-format shape (KD-MW-1 stamps
/// all nine bits, so every fleet volume carries it).
async fn sandbox(dir: &Path, tag: &str, stamp: bool) -> (Arc<RoutedMetaBackend>, PathBuf) {
    let plan = plan_meta_slot_set(1).expect("derived plan");
    let p = make_file(dir, &format!("{tag}-meta0"), VOL_LEN);
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &p,
        VOL_LEN,
        &opts(),
        plan.stamps[0].clone(),
    )
    .await
    .expect("format meta volume");
    if stamp {
        sb::set_layout_versions_bit(&p)
            .await
            .expect("stamp bit 15 (KV_LAYOUT_VERSIONS)");
    }
    let routed = open_routed_meta_set(&[p.display().to_string()])
        .await
        .expect("open routed set");
    (routed, p)
}

async fn shutdown(routed: &Arc<RoutedMetaBackend>) {
    for vol in &routed.volumes {
        vol.shutdown().await.expect("volume shutdown");
    }
}

struct Authority {
    listener: Arc<squeezefs::cluster_wire::RpcListener>,
    owner: Arc<WriteCustodyOwner>,
    endpoint: String,
    /// The owner's manual clock (ms) — "on the AUTHORITY's clock" is a
    /// literal statement in the demotion-expiry pin.
    clock_ms: Arc<AtomicU64>,
}

fn start_authority(inner: Arc<RoutedMetaBackend>, tag: &str) -> Authority {
    start_authority_geo(
        inner,
        tag,
        // The §9.2 geometry source: one 64 MiB file of 4 MiB blocks — what
        // makes the demotion barrier's block-sharing probe decidable.
        Some(data_grant::fixed_range_geometry(16 * BLOCK, BLOCK)),
    )
}

/// [`start_authority`] with the geometry source EXPLICIT — `None` is the
/// shape the PRODUCTION arm actually ran until the zeros-interleave
/// conviction (`arm_multi_writer` installed no source at all), which is
/// what the no-geometry pins below reproduce.
fn start_authority_geo(
    inner: Arc<RoutedMetaBackend>,
    tag: &str,
    geometry: Option<Arc<dyn data_grant::RangeGeometry>>,
) -> Authority {
    let ms = Arc::new(AtomicU64::new(1_000));
    let clock = LeaseClock::manual(Arc::clone(&ms));
    let clocks = LeaseClocks::with_params(
        Duration::from_millis(3_000),
        Duration::from_millis(200),
        Duration::from_millis(400),
    )
    .expect("positive T_self");
    let owner = WriteCustodyOwner::arm(
        tag,
        squeezefs::dlm::durable_term() + 1,
        squeezefs::dlm::durable_term(),
        clocks,
        clock,
        None,
    )
    .expect("the custody authority arms");
    if let Some(geometry) = geometry {
        owner.install_range_geometry(geometry);
    }
    data_grant::install_custody_owner(Arc::clone(&owner));
    let router = data_grant::AsyncVerbRouter::new()
        .with_custody(Arc::clone(&owner))
        .with_publish(publish::PublishService::new(inner));
    let listener = squeezefs::cluster_wire::RpcListener::start_async(
        squeezefs::cluster_wire::RpcListenerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            service_threads: 2,
            ..squeezefs::cluster_wire::RpcListenerConfig::default()
        },
        SECRET.to_vec(),
        Arc::new(router),
    )
    .expect("the authority listens");
    let endpoint = listener.endpoint().to_string();
    Authority {
        listener,
        owner,
        endpoint,
        clock_ms: ms,
    }
}

/// Arm the CLIENT half: all-foreign ownership over `client_be`, a publish
/// client, and a REAL custody join.
async fn arm_client(
    auth: &Authority,
    client_be: &Arc<RoutedMetaBackend>,
) -> (Arc<WriteCustodyClient>, Arc<publish::PublishClient>) {
    let foreign: Vec<(usize, PeerOwner)> = (0..client_be.volumes.len())
        .map(|v| (v, PeerOwner::new("assembler-authority", &auth.endpoint)))
        .collect();
    ship::arm_ownership(OwnerMap::for_volumes(client_be, foreign).expect("owner map"));
    let pc = publish::PublishClient::new(NODE_A, SECRET.to_vec());
    publish::install_client(Arc::clone(&pc));
    let client = WriteCustodyClient::connect(&auth.endpoint, SECRET, NODE_A)
        .await
        .expect("the co-writer joins the custody plane");
    data_grant::install_custody_client(Arc::clone(&client));
    (client, pc)
}

fn journal_entries() -> u64 {
    squeezefs::meta_backend::kv::META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed)
}

/// A DECODABLE base layout `Put` with one mapped block per entry.
fn base_layout_bytes(size: u64, map: &[(u32, &str)]) -> Vec<u8> {
    bincode::serialize(&LayoutMetadata {
        file_type: "striped".into(),
        size,
        block_map_id: None,
        block_prefix: Some("be://data".into()),
        file_id: None,
        data_key: None,
        block_map: Some(map.iter().map(|(b, k)| (*b, k.to_string())).collect()),
    })
    .expect("serialize base layout")
}

fn layout_of(bytes: &[u8]) -> LayoutMetadata {
    squeezefs::layout_wire::decode_base_layout(bytes).expect("decodable layout")
}

async fn owner_layout(be: &Arc<RoutedMetaBackend>, ino: u64) -> LayoutMetadata {
    use squeezefs::meta_backend::Metadata;
    let raw = be
        .getxattr(ino, "layout")
        .await
        .expect("owner layout read")
        .expect("layout present");
    layout_of(&raw)
}

/// One layout delta at the shipped shape.
fn delta(size: u64, entries: &[(u32, &str)], versions: Option<(u64, u64)>) -> LayoutDelta {
    let mut d = LayoutDelta::from_final_state(
        "striped",
        size,
        None,
        Some("be://data"),
        None,
        None,
        entries
            .iter()
            .map(|(b, k)| (*b, k.to_string()))
            .collect::<Vec<_>>(),
    );
    if let Some((base, version)) = versions {
        d.set_versions(base, version);
    }
    d
}

/// A shipped-shape create on the client backend.
async fn shipped_create(client_be: &Arc<RoutedMetaBackend>, name: &str) -> u64 {
    publish::create_with_rdev_size(client_be, 1, name, libc::S_IFREG | 0o644, 0, 0, 0, 0)
        .await
        .expect("a live-era shipped create lands")
        .ino
}

/// The mock assembler: records every merged extent, applies it to a
/// per-(ino, block) image, and answers a canned covering version.
/// One recorded assembler merge: `(client, ino, block_index,
/// offset_in_block, data)`.
type MergeRec = (String, u64, u64, u32, Vec<u8>);

#[derive(Debug, Default)]
struct MockAssembler {
    merges: parking_lot::Mutex<Vec<MergeRec>>,
    images: parking_lot::Mutex<std::collections::HashMap<(u64, u64), Vec<u8>>>,
    covering: AtomicU64, // 0 = answer None
}

impl MockAssembler {
    fn install(self: &Arc<Self>) {
        let me = Arc::clone(self);
        publish::install_extent_merge_executor(Arc::new(move |frame: publish::ExtentFrame| {
            let me = Arc::clone(&me);
            Box::pin(async move {
                me.merges.lock().push((
                    frame.client.clone(),
                    frame.ino,
                    frame.block_index,
                    frame.offset_in_block,
                    frame.data.clone(),
                ));
                let mut images = me.images.lock();
                let img = images
                    .entry((frame.ino, frame.block_index))
                    .or_insert_with(|| vec![0u8; BLOCK as usize]);
                let off = frame.offset_in_block as usize;
                img[off..off + frame.data.len()].copy_from_slice(&frame.data);
                let v = me.covering.load(Ordering::Relaxed);
                Ok((v != 0).then_some(v))
            })
        }));
    }

    fn install_flush(self: &Arc<Self>, version: u64) {
        publish::install_extent_flush_executor(Arc::new(move |_ino: u64| {
            Box::pin(async move { Ok(version) })
        }));
    }
}

fn extent_stats() -> publish::PublishStats {
    publish::stats()
}

// ===========================================================================
// 1. WriteExtent: era-gated, witnessed, served through the assembler
// ===========================================================================

/// Contract (KD-MW-8 + the §6a inheritance): a `WriteExtent` from a LIVE
/// custody epoch is served through the installed assembler executor and
/// counted (`extent_shipped`/`extent_served`); a verbatim re-ship of the
/// SAME frame answers from the `(lease_epoch, request_id)` witness
/// (`extent_replays` — the executor ran ONCE); a dead-era frame refuses
/// `PUBLISH_STALE_LEASE` BEFORE the window with nothing merged
/// (`extent_stale_refusals`) — the retried-class adjudication stated
/// against the module's no-retry doctrine.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_extent_is_era_gated_witnessed_and_served_through_the_assembler() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli", false).await;
    let auth = start_authority(Arc::clone(&owner_be), "assembler-authority");
    let (client, pc) = arm_client(&auth, &client_be).await;
    let epoch = client.lease_epoch();
    let ino = shipped_create(&client_be, "extent.bin").await;

    let asm = Arc::new(MockAssembler::default());
    asm.install();

    let s0 = extent_stats();
    let call = publish::PublishCall::WriteExtent {
        ino,
        block_index: 0,
        offset_in_block: 4096,
        data: b"assembled-bytes".to_vec(),
        token: 0,
        lease_epoch: epoch,
        request_id: 0xE1,
    };
    let first = pc
        .ship(&auth.endpoint, call.clone())
        .await
        .expect("a live-era WriteExtent is served");
    assert!(
        matches!(first, publish::PublishReply::ExtentAck { .. }),
        "the reply shape is the extent ack: {first:?}"
    );
    assert_eq!(asm.merges.lock().len(), 1, "the assembler merged ONCE");
    let s1 = extent_stats();
    assert_eq!(s1.extent_served - s0.extent_served, 1);

    // The witness: the SAME frame re-ships (the lost-reply retry) and is
    // ANSWERED, never re-merged.
    let replayed = pc
        .ship(&auth.endpoint, call)
        .await
        .expect("the duplicate is answered from the window");
    assert_eq!(replayed, first, "a replay answers the winner's own ack");
    assert_eq!(
        asm.merges.lock().len(),
        1,
        "the executor ran exactly once — exactly-once under the dedup window"
    );
    let s2 = extent_stats();
    assert_eq!(s2.extent_replays - s1.extent_replays, 1);

    // The era gate: a frame naming an epoch that is not live custody
    // refuses BEFORE the window, merging nothing and staging nothing.
    let journal_before = journal_entries();
    let stale = publish::PublishCall::WriteExtent {
        ino,
        block_index: 0,
        offset_in_block: 0,
        data: b"zombie".to_vec(),
        token: 0,
        lease_epoch: epoch + 1000,
        request_id: 0xE2,
    };
    let err = pc
        .ship(&auth.endpoint, stale)
        .await
        .expect_err("a dead era's extent refuses");
    assert!(
        matches!(err, squeezefs::error::SqueezefsError::WriterGuardFenced)
            || format!("{err}").contains("STALE"),
        "the refusal is the era gate's class: {err:?}"
    );
    assert_eq!(asm.merges.lock().len(), 1, "nothing was merged");
    assert_eq!(
        journal_entries(),
        journal_before,
        "a refused extent stages no journal entry (applies nothing)"
    );
    let s3 = extent_stats();
    assert_eq!(s3.extent_stale_refusals - s2.extent_stale_refusals, 1);

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

// ===========================================================================
// 2. The demotion barrier (dlm core face) + the clause firing venue
// ===========================================================================

/// Contract (§9.3's barrier, the charter's headline red-first): holder A
/// holds a block-aligned range grant and holder B's sub-block acquire
/// overlapping A's block PARKS — the grant is withheld until A's
/// demotion is ACKED; a single publisher holds at every instant
/// (`range_custody_demotion_fenced_publishes` 0 on the clean path); the
/// ledger closes `demotions ≡ acks + fence_resolves`; and POST-demotion
/// the block is range-shared for BOTH holders — the rung-16 clauses'
/// live firing venue (`patch_ineligible_range_shared` and
/// `overlay_ineligible_range_shared` move).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_demotion_barrier_withholds_the_grant_until_the_incumbents_ack() {
    let _serial = serial();
    let _restore = restore();
    let mgr = squeezefs::dlm::LocalLockManager::new().unwrap();
    let ino: u64 = 0xDE01;
    let path = format!("inode_{ino}");
    let geometry = Some((16 * BLOCK, BLOCK));
    let d0 = squeezefs::dlm::range_custody_stats();

    // A: a block-aligned grant over blocks 0..2 (scope 0xA).
    let a = mgr
        .acquire_lock_range_scoped(
            &path,
            (0, 2 * BLOCK),
            (0, 2 * BLOCK),
            Duration::from_secs(1),
            geometry,
            Some(0xA),
        )
        .await
        .expect("A's aligned grant issues");
    let a_token = match &a {
        squeezefs::dlm::RangeAcquired::New { lease, .. } => lease.fencing_token(),
        other => panic!("A's grant is NEW: {other:?}"),
    };

    // B: a sub-block required window INSIDE A's block 0 (scope 0xB) —
    // the §9.3 shared shape. The acquire must PARK (grant withheld).
    let b_task = {
        let mgr = mgr.clone();
        let path = path.clone();
        tokio::spawn(async move {
            mgr.acquire_lock_range_scoped(
                &path,
                (1024 * 1024, 2 * 1024 * 1024),
                (1024 * 1024, 2 * 1024 * 1024),
                Duration::from_secs(30),
                geometry,
                Some(0xB),
            )
            .await
        })
    };
    // The pending mark lands (the barrier engaged) and B stays parked.
    let mut marked = false;
    for _ in 0..200 {
        if squeezefs::dlm::range_custody_stats().demotions > d0.demotions {
            marked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        marked,
        "B's block-sharing acquire marked a demotion pending"
    );
    assert!(!b_task.is_finished(), "B's grant is WITHHELD (parked)");

    // The notice names the incumbent: A's token, block 0's region.
    let notices = squeezefs::dlm::demotion_notices_for(ino, a_token);
    assert_eq!(notices.len(), 1, "one unacked notice for A");
    let region = notices[0];
    assert!(
        region.0 == 0 && region.1 >= BLOCK,
        "the region is the shared block's block-aligned hull: {region:?}"
    );

    // A acks (having quiesced + re-routed): B's grant now issues.
    assert!(
        squeezefs::dlm::ack_demotion(ino, a_token, region),
        "the ack lands"
    );
    let b = tokio::time::timeout(Duration::from_secs(10), b_task)
        .await
        .expect("B's acquire resolves after the ack")
        .expect("join")
        .expect("B's grant issues");
    let b_token = match &b {
        squeezefs::dlm::RangeAcquired::New { lease, .. } => lease.fencing_token(),
        other => panic!("B's grant is NEW: {other:?}"),
    };

    // The closed ledger: one demotion, one ack, zero fence resolves,
    // zero fenced publishes (the clean path).
    let d1 = squeezefs::dlm::range_custody_stats();
    assert_eq!(d1.demotions - d0.demotions, 1);
    assert_eq!(d1.demotion_acks - d0.demotion_acks, 1);
    assert_eq!(d1.demotion_fence_resolves, d0.demotion_fence_resolves);
    assert_eq!(
        d1.demotion_fenced_publishes, d0.demotion_fenced_publishes,
        "the fenced-publish tripwire stays 0 on the clean path"
    );
    assert_eq!(
        d1.demotions - d0.demotions,
        (d1.demotion_acks - d0.demotion_acks)
            + (d1.demotion_fence_resolves - d0.demotion_fence_resolves),
        "demotions ≡ acks + fence_resolves"
    );

    // Post-demotion the block is range-shared for BOTH holders (the
    // extent-ship classification) — and the rung-16 clauses FIRE (their
    // live venue: the ledgers move).
    for token in [a_token, b_token] {
        assert!(
            squeezefs::dlm::span_range_shared(ino, 0, BLOCK, token),
            "the demoted block classifies range-shared under {token:#x}"
        );
    }
    let prs0 = METRICS
        .patch_ineligible_range_shared
        .load(Ordering::Relaxed);
    let ors0 = METRICS
        .overlay_ineligible_range_shared
        .load(Ordering::Relaxed);
    assert!(
        squeezefs::block_allocator::BlockAllocator::patch_range_shared(ino, 0, BLOCK, a_token),
        "W1 clause 7 fires on the demoted block"
    );
    assert!(
        squeezefs::device_overlay::overlay_range_shared(ino, 0, BLOCK, b_token),
        "the B4 §5.1 clause fires on the demoted block"
    );
    assert_eq!(
        METRICS
            .patch_ineligible_range_shared
            .load(Ordering::Relaxed)
            - prs0,
        1
    );
    assert_eq!(
        METRICS
            .overlay_ineligible_range_shared
            .load(Ordering::Relaxed)
            - ors0,
        1
    );

    // Blocks OUTSIDE the demoted region keep A's whole-block custody:
    // the demotion is per-block, never per-file.
    assert!(
        !squeezefs::dlm::span_range_shared(ino, BLOCK, 2 * BLOCK, a_token),
        "A's block 1 custody is untouched by block 0's demotion"
    );

    match a {
        squeezefs::dlm::RangeAcquired::New { lease, .. } => lease.release().await.unwrap(),
        _ => unreachable!(),
    }
    match b {
        squeezefs::dlm::RangeAcquired::New { lease, .. } => lease.release().await.unwrap(),
        _ => unreachable!(),
    }
}

/// Contract: an UNACKED demotion resolves at the incumbent's lease death
/// — its grants retire (release/revocation/expiry), the pending resolves
/// through the FENCE column, and the ledger still closes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unacked_demotion_resolves_when_the_incumbents_grant_dies() {
    let _serial = serial();
    let _restore = restore();
    let mgr = squeezefs::dlm::LocalLockManager::new().unwrap();
    let ino: u64 = 0xDE02;
    let path = format!("inode_{ino}");
    let geometry = Some((16 * BLOCK, BLOCK));
    let d0 = squeezefs::dlm::range_custody_stats();

    let a = mgr
        .acquire_lock_range_scoped(
            &path,
            (0, BLOCK),
            (0, BLOCK),
            Duration::from_secs(1),
            geometry,
            Some(0xA),
        )
        .await
        .expect("A's aligned grant issues");
    let b_task = {
        let mgr = mgr.clone();
        let path = path.clone();
        tokio::spawn(async move {
            mgr.acquire_lock_range_scoped(
                &path,
                (4096, 8192),
                (4096, 8192),
                Duration::from_secs(30),
                geometry,
                Some(0xB),
            )
            .await
        })
    };
    for _ in 0..200 {
        if squeezefs::dlm::range_custody_stats().demotions > d0.demotions {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!b_task.is_finished(), "B parks behind the barrier");

    // A's grant DIES without an ack (the kill-matrix shape: release here,
    // revocation/expiry on the wire face below).
    match a {
        squeezefs::dlm::RangeAcquired::New { lease, .. } => lease.release().await.unwrap(),
        _ => unreachable!(),
    }
    let b = tokio::time::timeout(Duration::from_secs(10), b_task)
        .await
        .expect("B resolves at the incumbent's death")
        .expect("join")
        .expect("B's grant issues");
    let d1 = squeezefs::dlm::range_custody_stats();
    assert_eq!(d1.demotions - d0.demotions, 1);
    assert_eq!(
        d1.demotion_fence_resolves - d0.demotion_fence_resolves,
        1,
        "the kill row closes the ledger through the FENCE column"
    );
    assert_eq!(d1.demotion_acks, d0.demotion_acks);
    // A fence-resolved barrier marks NOTHING demoted: the survivor gets
    // clean (un-demoted) custody — the dead incumbent's era is fenced by
    // quarantine/era machinery, not by an assembly that never started.
    assert!(
        squeezefs::dlm::demoted_regions(ino).is_empty(),
        "a fence-resolved demotion marks no demoted region"
    );
    match b {
        squeezefs::dlm::RangeAcquired::New { lease, .. } => lease.release().await.unwrap(),
        _ => unreachable!(),
    }
}

// ===========================================================================
// 3. The wire face: renewal-carried notice, ack-or-owner-clock-expiry,
//    the in-flight-renewal race pin
// ===========================================================================

/// Contract: the demotion notice rides the incumbent's RENEWAL REPLY,
/// composed under the same `FileCustody` serialization that parked B (a
/// reply composed after the pending-mark always carries it; one composed
/// BEFORE the mark simply pushes it to the next renewal); the client's
/// renewal machinery quiesces, marks the region locally, and ACKS —
/// only then does B's grant issue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_renewal_carries_the_notice_and_the_ack_releases_the_parked_grant() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli", false).await;
    let auth = start_authority(Arc::clone(&owner_be), "assembler-authority");
    let (client, _pc) = arm_client(&auth, &client_be).await;
    let ino: u64 = 0xDE03;

    // The quiesce hook is observable: the client must run it BEFORE the
    // ack travels.
    let quiesced = Arc::new(AtomicU64::new(0));
    {
        let quiesced = Arc::clone(&quiesced);
        extent_ship::install_quiesce_hook(Arc::new(move |_ino| {
            let quiesced = Arc::clone(&quiesced);
            Box::pin(async move {
                quiesced.fetch_add(1, Ordering::Relaxed);
            })
        }));
    }

    // A (this client) takes the block-aligned grant over block 0.
    let a = client
        .acquire_range(ino, (0, BLOCK), (0, BLOCK), Duration::from_secs(1))
        .await
        .expect("A's aligned grant issues");
    let a_token = match &a {
        data_grant::RangeAcquireOutcome::New { lease, .. } => lease.fencing_token(),
        other => panic!("A's grant is NEW: {other:?}"),
    };

    // A renewal composed BEFORE the mark carries NO notice.
    let epoch = client.lease_epoch();
    let pre = auth
        .owner
        .renew(NODE_A, epoch, &[])
        .expect("pre-mark renewal");
    assert!(
        pre.demotions.is_empty(),
        "a reply composed before the mark carries nothing"
    );

    // B (a second holder, driven as raw frames) parks on a sub-block
    // acquire inside A's block.
    let b_lease = auth
        .owner
        .join(&data_grant::JoinFrame {
            schema: data_grant::CUSTODY_SCHEMA,
            client: "node-assembler-b".to_string(),
            pr_key: 0,
            prior_epoch: None,
        })
        .expect("B joins");
    let b_task = {
        let owner = Arc::clone(&auth.owner);
        let b_epoch = b_lease.epoch;
        tokio::spawn(async move {
            owner
                .grant_ranged(
                    &data_grant::AcquireFrame {
                        schema: data_grant::CUSTODY_SCHEMA,
                        client: "node-assembler-b".to_string(),
                        lease_epoch: b_epoch,
                        ino,
                        span: Some((4096, 12288)),
                        concurrent_write: false,
                        wait_ms: 30_000,
                        desired: Some((4096, 12288)),
                    },
                    (4096, 12288),
                )
                .await
        })
    };
    let d0 = squeezefs::dlm::range_custody_stats();
    let mut marked = false;
    for _ in 0..300 {
        if squeezefs::dlm::range_custody_stats().demotions >= 1
            && !squeezefs::dlm::demotion_notices_for(ino, a_token).is_empty()
        {
            marked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(marked, "the pending mark landed");
    assert!(!b_task.is_finished(), "B's grant is withheld pre-ack");

    // The in-flight-renewal race pin: a reply composed AFTER the mark
    // ALWAYS carries the notice (the mark and the reply composition
    // serialize on the same FileCustody entry).
    let post = auth
        .owner
        .renew(NODE_A, epoch, &[])
        .expect("post-mark renewal");
    assert_eq!(
        post.demotions.len(),
        1,
        "the renewal reply composed after the mark carries the notice"
    );
    assert_eq!(post.demotions[0].ino, ino);
    assert_eq!(post.demotions[0].incumbent_token, a_token);

    // The CLIENT machinery: renew_all observes the notice, quiesces,
    // marks locally, and acks — B's grant then issues.
    client.renew_all().await.expect("the incumbent's renewal");
    assert!(
        quiesced.load(Ordering::Relaxed) >= 1,
        "the client quiesced before acking"
    );
    let b = tokio::time::timeout(Duration::from_secs(10), b_task)
        .await
        .expect("B resolves after the ack")
        .expect("join")
        .expect("B's grant issues");
    assert!(b.span == Some((4096, 12288)) || b.span.is_some());
    let d1 = squeezefs::dlm::range_custody_stats();
    assert_eq!(
        d1.demotion_acks - d0.demotion_acks,
        1,
        "the ack travelled as a client-initiated RPC"
    );
    // The incumbent marked the region locally: its own probe now
    // classifies the block extent-ship.
    assert!(
        squeezefs::dlm::span_range_shared(ino, 0, BLOCK, a_token),
        "A's local mark re-routes its own writes to extent-ship"
    );

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

/// Contract: an incumbent that NEVER acks resolves at its lease expiry
/// on the AUTHORITY's clock — the owner-side sweep retires its grants,
/// the pending resolves through the fence column, and B's grant issues.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unacked_demotion_resolves_at_lease_expiry_on_the_owners_clock() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli", false).await;
    let auth = start_authority(Arc::clone(&owner_be), "assembler-authority");
    let (client, _pc) = arm_client(&auth, &client_be).await;
    let ino: u64 = 0xDE04;
    let d0 = squeezefs::dlm::range_custody_stats();

    let _a = client
        .acquire_range(ino, (0, BLOCK), (0, BLOCK), Duration::from_secs(1))
        .await
        .expect("A's aligned grant issues");
    let b_lease = auth
        .owner
        .join(&data_grant::JoinFrame {
            schema: data_grant::CUSTODY_SCHEMA,
            client: "node-assembler-b".to_string(),
            pr_key: 0,
            prior_epoch: None,
        })
        .expect("B joins");
    let b_task = {
        let owner = Arc::clone(&auth.owner);
        let b_epoch = b_lease.epoch;
        tokio::spawn(async move {
            owner
                .grant_ranged(
                    &data_grant::AcquireFrame {
                        schema: data_grant::CUSTODY_SCHEMA,
                        client: "node-assembler-b".to_string(),
                        lease_epoch: b_epoch,
                        ino,
                        span: Some((0, 4096)),
                        concurrent_write: false,
                        wait_ms: 30_000,
                        desired: Some((0, 4096)),
                    },
                    (0, 4096),
                )
                .await
        })
    };
    for _ in 0..300 {
        if squeezefs::dlm::range_custody_stats().demotions > d0.demotions {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!b_task.is_finished(), "B parks behind the barrier");

    // A never acks. Advance the AUTHORITY's clock past A's TTL, but keep
    // B's lease alive by renewing it as the clock moves (B must survive
    // the same sweep). This is the "owner-side re-grant instant" — the
    // barrier's expiry arm resolves on the OWNER's clock, never on a
    // guess about A's.
    for step in 0..8u64 {
        auth.clock_ms.fetch_add(500, Ordering::Relaxed);
        let _ = auth.owner.renew("node-assembler-b", b_lease.epoch, &[]);
        let dead = auth.owner.expire_due();
        if !dead.is_empty() {
            break;
        }
        assert!(step < 7, "A's lease must expire within the ladder");
    }
    let b = tokio::time::timeout(Duration::from_secs(10), b_task)
        .await
        .expect("B resolves at A's expiry on the owner's clock")
        .expect("join")
        .expect("B's grant issues after the fence resolution");
    assert!(b.span.is_some());
    let d1 = squeezefs::dlm::range_custody_stats();
    assert_eq!(d1.demotions - d0.demotions, 1);
    assert_eq!(
        d1.demotion_fence_resolves - d0.demotion_fence_resolves,
        1,
        "the expiry arm closes the ledger through the fence column"
    );
    assert_eq!(
        (d1.demotions - d0.demotions),
        (d1.demotion_acks - d0.demotion_acks)
            + (d1.demotion_fence_resolves - d0.demotion_fence_resolves),
        "demotions ≡ acks + fence_resolves on the kill posture too"
    );

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

/// MW-13: the authority dies MID-DEMOTION (B's grant parked, A un-acked).
/// The pending state was RAM and dies with it; A re-asserts its ORIGINAL
/// block-aligned grant in the successor's grace window; B's acquire
/// re-issues against the successor and the demotion RESTARTS from zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mw13_authority_death_mid_demotion_a_reasserts_and_the_demotion_restarts() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli", false).await;
    let auth1 = start_authority(Arc::clone(&owner_be), "assembler-authority-1");
    let (client1, _pc) = arm_client(&auth1, &client_be).await;
    let ino: u64 = 0xDE05;

    let _a1 = client1
        .acquire_range(ino, (0, BLOCK), (0, BLOCK), Duration::from_secs(1))
        .await
        .expect("A's aligned grant issues on authority 1");
    let b_lease = auth1
        .owner
        .join(&data_grant::JoinFrame {
            schema: data_grant::CUSTODY_SCHEMA,
            client: "node-assembler-b".to_string(),
            pr_key: 0,
            prior_epoch: None,
        })
        .expect("B joins authority 1");
    // B's parked acquire, bounded: the kill lands while it parks, so it
    // resolves as a refusal (the park re-issues against the successor —
    // MW-13's "B's parked acquire re-issues" is the CLIENT's retry).
    let b_task = {
        let owner = Arc::clone(&auth1.owner);
        let b_epoch = b_lease.epoch;
        tokio::spawn(async move {
            owner
                .grant_ranged(
                    &data_grant::AcquireFrame {
                        schema: data_grant::CUSTODY_SCHEMA,
                        client: "node-assembler-b".to_string(),
                        lease_epoch: b_epoch,
                        ino,
                        span: Some((4096, 8192)),
                        concurrent_write: false,
                        wait_ms: 2_000,
                        desired: Some((4096, 8192)),
                    },
                    (4096, 8192),
                )
                .await
        })
    };
    let d0 = squeezefs::dlm::range_custody_stats();
    for _ in 0..300 {
        if squeezefs::dlm::range_custody_stats().demotions >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!b_task.is_finished(), "B is parked mid-demotion");

    // THE KILL: authority 1 dies with the demotion pending. In-process,
    // the owner's table drop retires its arbiter leases (the RAM state
    // dying with the process); B's parked acquire resolves as a refusal.
    auth1.listener.shutdown();
    data_grant::uninstall_custody_owner();
    drop(auth1);
    let b1 = b_task.await.expect("join");
    assert!(
        b1.is_err(),
        "B never held a grant from the dead authority: {b1:?}"
    );
    assert!(
        squeezefs::dlm::demotion_notices_for(ino, 0).is_empty(),
        "the pending demotion state died with the authority (RAM)"
    );

    // The successor opens with a grace window expecting A.
    let auth2 = start_authority(Arc::clone(&owner_be), "assembler-authority-2");
    auth2.owner.open_grace(vec![NODE_A.to_string()]);
    let client2 = WriteCustodyClient::connect(&auth2.endpoint, SECRET, NODE_A)
        .await
        .expect("A re-joins the successor");
    data_grant::install_custody_client(Arc::clone(&client2));
    // MW-13's law verbatim: A re-asserts its ORIGINAL BLOCK-ALIGNED
    // RANGE grant (never a whole-file widening, which would conflict
    // with surviving peers' ranges and make B's re-ask a plain wait).
    let reclaimed = client2
        .reclaim_with_ranges(&[], &[(ino, (0, BLOCK))])
        .await
        .expect("A re-asserts");
    assert_eq!(
        reclaimed.len(),
        1,
        "A re-asserted its ORIGINAL grant in the grace window"
    );
    assert_eq!(
        reclaimed[0].span,
        Some((0, BLOCK)),
        "the re-assertion is the original span"
    );

    // B re-issues against the successor: the demotion restarts from
    // zero — a fresh pending, resolved by A's ack (via A's renewal).
    let b2_lease = auth2
        .owner
        .join(&data_grant::JoinFrame {
            schema: data_grant::CUSTODY_SCHEMA,
            client: "node-assembler-b".to_string(),
            pr_key: 0,
            prior_epoch: None,
        })
        .expect("B re-joins");
    let b2_task = {
        let owner = Arc::clone(&auth2.owner);
        let b_epoch = b2_lease.epoch;
        tokio::spawn(async move {
            owner
                .grant_ranged(
                    &data_grant::AcquireFrame {
                        schema: data_grant::CUSTODY_SCHEMA,
                        client: "node-assembler-b".to_string(),
                        lease_epoch: b_epoch,
                        ino,
                        span: Some((4096, 8192)),
                        concurrent_write: false,
                        wait_ms: 30_000,
                        desired: Some((4096, 8192)),
                    },
                    (4096, 8192),
                )
                .await
        })
    };
    let mut restarted = false;
    for _ in 0..300 {
        if squeezefs::dlm::range_custody_stats().demotions >= d0.demotions + 2 {
            restarted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(restarted, "the demotion RESTARTED on the successor");
    client2.renew_all().await.expect("A's renewal acks");
    let b2 = tokio::time::timeout(Duration::from_secs(10), b2_task)
        .await
        .expect("B's grant issues on the successor")
        .expect("join")
        .expect("the restarted demotion resolves");
    assert!(b2.span.is_some());

    auth2.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

// ===========================================================================
// 4. Retention: the four pull release paths (one red-first case each)
// ===========================================================================

/// Release path 1 — **ack-carried `covering_version`**: `Some` iff the
/// covering publish has already run; the extent releases immediately and
/// never enters long retention.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn release_path_1_an_ack_with_a_covering_version_releases_immediately() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli", false).await;
    let auth = start_authority(Arc::clone(&owner_be), "assembler-authority");
    let (_client, _pc) = arm_client(&auth, &client_be).await;
    let ino = shipped_create(&client_be, "rel1.bin").await;
    let asm = Arc::new(MockAssembler::default());
    asm.covering.store(0x77, Ordering::Relaxed); // already covered
    asm.install();

    let before = extent_ship::retained_bytes();
    extent_ship::ship_extent(
        &client_be,
        ino,
        0,
        0,
        bytes::Bytes::from_static(b"covered"),
        1,
    )
    .await
    .expect("the extent ships");
    assert_eq!(
        extent_ship::retained_bytes(),
        before,
        "an ack carrying Some(covering_version) releases retention at the round trip"
    );
    assert_eq!(extent_ship::retained_count(ino), 0);

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

/// Release path 2 — **renewal/revalidation observation**: retention holds
/// through the un-covered ack; when the owner's covering publish commits
/// (the fold), the next RENEWAL carries the coverage watermark and the
/// client releases. An ACK ALONE NEVER RELEASES — the MW-11 law's
/// live half.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn release_path_2_renewal_observation_releases_covered_extents() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli", false).await;
    let auth = start_authority(Arc::clone(&owner_be), "assembler-authority");
    let (client, _pc) = arm_client(&auth, &client_be).await;
    let ino = shipped_create(&client_be, "rel2.bin").await;
    let asm = Arc::new(MockAssembler::default());
    asm.install(); // covering = 0 → the ack answers None

    let before = extent_ship::retained_bytes();
    extent_ship::ship_extent(
        &client_be,
        ino,
        0,
        0,
        bytes::Bytes::from_static(b"retained"),
        1,
    )
    .await
    .expect("the extent ships");
    assert!(
        extent_ship::retained_bytes() > before,
        "an ack WITHOUT a covering version retains — ack alone never releases"
    );
    // A renewal BEFORE coverage releases nothing.
    client.renew_all().await.expect("renewal");
    assert!(extent_ship::retained_bytes() > before);

    // The owner's covering publish commits (the fold force) — the next
    // renewal carries the watermark and the client releases.
    extent_ship::owner_note_covered(ino, 0x99);
    client.renew_all().await.expect("renewal");
    assert_eq!(
        extent_ship::retained_bytes(),
        before,
        "renewal observation of the covering version releases retention"
    );
    assert_eq!(extent_ship::retained_count(ino), 0);

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

/// Release path 3 — **`FlushExtents`, the fsync force**: a synchronous
/// flush-force publish RPC whose reply carries the covering version;
/// fsync returns only after release (`extent_flush_forces` counts).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn release_path_3_flush_extents_forces_the_publish_and_releases() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli", false).await;
    let auth = start_authority(Arc::clone(&owner_be), "assembler-authority");
    let (_client, _pc) = arm_client(&auth, &client_be).await;
    let ino = shipped_create(&client_be, "rel3.bin").await;
    let asm = Arc::new(MockAssembler::default());
    asm.install();
    asm.install_flush(0xF1);

    let s0 = extent_stats();
    let before = extent_ship::retained_bytes();
    extent_ship::ship_extent(
        &client_be,
        ino,
        0,
        0,
        bytes::Bytes::from_static(b"fsynced"),
        1,
    )
    .await
    .expect("the extent ships");
    assert!(extent_ship::retained_bytes() > before);
    let covering = extent_ship::flush_ino(&client_be, ino)
        .await
        .expect("the fsync force flushes");
    assert_eq!(covering, 0xF1, "the reply carries the covering version");
    assert_eq!(
        extent_ship::retained_bytes(),
        before,
        "fsync returns only after release"
    );
    let s1 = extent_stats();
    assert_eq!(s1.extent_flush_forces - s0.extent_flush_forces, 1);

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

/// Release path 4 — **the at-budget W2 spill**: when retention hits its
/// budget, retained extents SPILL to the local-durable record sink
/// instead of blocking the writer; the spilled extent stays logically
/// retained (the record IS the retention) but leaves the RAM gauge.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn release_path_4_at_budget_retention_spills_instead_of_blocking() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli", false).await;
    let auth = start_authority(Arc::clone(&owner_be), "assembler-authority");
    let (_client, _pc) = arm_client(&auth, &client_be).await;
    let ino = shipped_create(&client_be, "rel4.bin").await;
    let asm = Arc::new(MockAssembler::default());
    asm.install();

    let spilled = Arc::new(AtomicU64::new(0));
    {
        let spilled = Arc::clone(&spilled);
        extent_ship::install_spill_sink(Arc::new(move |_ino, _block, _off, data| {
            spilled.fetch_add(data.len() as u64, Ordering::Relaxed);
            Ok(())
        }));
    }
    extent_ship::test_swap_retention_budget(Some(64));

    let s0 = extent_stats();
    // Two extents: the second pushes past the 64-byte budget and the
    // FIRST (oldest, acked) spills — the writer is never blocked.
    extent_ship::ship_extent(&client_be, ino, 0, 0, bytes::Bytes::from(vec![0xAA; 48]), 1)
        .await
        .expect("extent 1 ships");
    extent_ship::ship_extent(
        &client_be,
        ino,
        0,
        8192,
        bytes::Bytes::from(vec![0xBB; 48]),
        1,
    )
    .await
    .expect("extent 2 ships (never blocks at budget)");
    assert!(
        spilled.load(Ordering::Relaxed) >= 48,
        "the at-budget arm SPILLED to the record sink"
    );
    assert!(
        extent_ship::retained_bytes() <= 64,
        "spilled bytes left the RAM gauge"
    );
    assert_eq!(
        extent_ship::retained_count(ino),
        2,
        "a spilled extent stays LOGICALLY retained — the record is the retention"
    );
    let s1 = extent_stats();
    assert!(s1.extent_spills - s0.extent_spills >= 1);

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

// ===========================================================================
// 5. MW-10/11: retention across authority death — zero acked-fsynced loss
// ===========================================================================

/// MW-11 (and MW-10's surviving-client half): the authority dies BETWEEN
/// the ack and the covering publish. Retention released NOTHING on the
/// ack, so after the successor's grace window every retained extent
/// re-ships idempotently and the fsync completes with zero acked-fsynced
/// loss.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mw11_authority_death_between_ack_and_publish_loses_no_acked_fsynced_bytes() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli", false).await;
    let auth1 = start_authority(Arc::clone(&owner_be), "assembler-authority-1");
    let (_client1, _pc) = arm_client(&auth1, &client_be).await;
    let ino = shipped_create(&client_be, "mw11.bin").await;
    let asm1 = Arc::new(MockAssembler::default());
    asm1.install();

    let before = extent_ship::retained_bytes();
    extent_ship::ship_extent(
        &client_be,
        ino,
        0,
        4096,
        bytes::Bytes::from_static(b"acked-but-unpublished"),
        1,
    )
    .await
    .expect("the extent ships and is ACKED");
    assert_eq!(asm1.merges.lock().len(), 1, "merged on authority 1");
    assert!(
        extent_ship::retained_bytes() > before,
        "retention holds through the ack — ack releases NOTHING"
    );

    // THE KILL: authority 1 dies with the extent merged but unpublished.
    auth1.listener.shutdown();
    data_grant::uninstall_custody_owner();
    drop(auth1);
    assert!(
        extent_ship::retained_bytes() > before,
        "retention survives the authority's death"
    );

    // The successor: fresh assembler (its overlay is EMPTY — the merge
    // died with authority 1's RAM), grace window, re-join, RE-SHIP. The
    // ownership map re-arms at the successor's endpoint (the failover
    // relearn every S9-b row already exercises).
    let auth2 = start_authority(Arc::clone(&owner_be), "assembler-authority-2");
    let foreign2: Vec<(usize, PeerOwner)> = (0..client_be.volumes.len())
        .map(|v| (v, PeerOwner::new("assembler-authority-2", &auth2.endpoint)))
        .collect();
    ship::arm_ownership(OwnerMap::for_volumes(&client_be, foreign2).expect("owner map"));
    let asm2 = Arc::new(MockAssembler::default());
    asm2.install();
    asm2.install_flush(0xC2);
    let client2 = WriteCustodyClient::connect(&auth2.endpoint, SECRET, NODE_A)
        .await
        .expect("the co-writer re-joins the successor");
    data_grant::install_custody_client(Arc::clone(&client2));
    let reshipped = extent_ship::reship_all(&client_be)
        .await
        .expect("retained extents re-ship idempotently");
    assert_eq!(reshipped, 1, "every retained extent re-shipped");
    assert_eq!(
        asm2.merges.lock().len(),
        1,
        "the successor's assembler holds the bytes again"
    );
    assert_eq!(
        asm2.images.lock().get(&(ino, 0)).unwrap()[4096..4096 + 21].to_vec(),
        b"acked-but-unpublished".to_vec(),
        "zero acked loss: the successor assembled the exact bytes"
    );

    // The fsync that was chained through the publish barrier completes
    // only NOW — after the successor's covering publish.
    let covering = extent_ship::flush_ino(&client_be, ino)
        .await
        .expect("the fsync force completes on the successor");
    assert_eq!(covering, 0xC2);
    assert_eq!(
        extent_ship::retained_bytes(),
        before,
        "retention releases on covering-version visibility, never on ack"
    );

    auth2.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

// ===========================================================================
// 6. The composition: shipped merges chain onto the head (the standing-red
//    gate's machinery)
// ===========================================================================

/// Contract (design-mw-layout-versions §6, owned by this rung): two
/// shipped publishers of ONE ino compose — a shipped merge whose claim
/// is stale (or 0) CHAINS onto the durable head instead of refusing or
/// re-basing with the co-writer's private full layout (the clobber
/// mint); the reply carries the staged link's version so the co-writer
/// chains without a refetch; and the final durable layout carries BOTH
/// writers' blocks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shipped_merges_chain_onto_the_head_and_never_clobber_a_peer() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli", false).await;
    let auth = start_authority(Arc::clone(&owner_be), "assembler-authority");
    let (client, pc) = arm_client(&auth, &client_be).await;
    let epoch = client.lease_epoch();
    let ino = shipped_create(&client_be, "compose.bin").await;
    let base = base_layout_bytes(4 * BLOCK, &[]);
    publish::set_layout_and_size(&client_be, ino, &base, 4 * BLOCK, &[])
        .await
        .expect("the base Put lands");

    // Writer 1 publishes ITS block (a delta claiming 0 — the first
    // link, carrying a client-minted version the owner is free to
    // supersede). The reply carries the STAGED version.
    let p1 = publish::PublishCall::MergeLayoutAndSize {
        ino,
        delta: delta(
            4 * BLOCK,
            &[(0, "be://data:w1b0")],
            Some((0, squeezefs::dlm::mint_layout_version())),
        )
        .encode(),
        full_layout: base_layout_bytes(4 * BLOCK, &[(0, "be://data:w1b0")]),
        size: 4 * BLOCK,
        refs: Vec::new(),
        lease_epoch: epoch,
        request_id: 0xC1,
    };
    let r1 = pc.ship(&auth.endpoint, p1).await.expect("W1's merge lands");
    let publish::PublishReply::DeltaUsed { used, version: v1 } = r1 else {
        panic!("the merge reply carries the staged version: {r1:?}");
    };
    assert!(
        used,
        "the shipped merge STAGED a delta (never a full-Put re-base)"
    );
    assert_ne!(v1, 0, "the staged link's version travels on the reply");

    // Writer 2 (a second holder's frame): its private view NEVER SAW
    // W1's block — its claim is 0 and its fallback full layout LACKS
    // W1's block. Pre-fix this re-based with W2's full layout and
    // silently erased W1's block (the C8 mint). Post-fix it CHAINS.
    let p2 = publish::PublishCall::MergeLayoutAndSize {
        ino,
        delta: delta(
            4 * BLOCK,
            &[(1, "be://data:w2b1")],
            Some((0, squeezefs::dlm::mint_layout_version())),
        )
        .encode(),
        full_layout: base_layout_bytes(4 * BLOCK, &[(1, "be://data:w2b1")]),
        size: 4 * BLOCK,
        refs: Vec::new(),
        lease_epoch: epoch,
        request_id: 0xC2,
    };
    let r2 = pc.ship(&auth.endpoint, p2).await.expect("W2's merge lands");
    let publish::PublishReply::DeltaUsed { used, version: v2 } = r2 else {
        panic!("reply shape: {r2:?}");
    };
    assert!(used, "W2's merge chained as a delta too");
    assert_ne!(v2, 0);
    assert_ne!(v1, v2, "owner-minted link versions never collide");

    // A stale NONZERO claim (W1 chaining from its last reply while W2
    // moved the head) also chains — no refusal, no refetch loop.
    let p3 = publish::PublishCall::MergeLayoutAndSize {
        ino,
        delta: delta(
            4 * BLOCK,
            &[(2, "be://data:w1b2")],
            Some((v1, squeezefs::dlm::mint_layout_version())),
        )
        .encode(),
        full_layout: base_layout_bytes(4 * BLOCK, &[(0, "be://data:w1b0"), (2, "be://data:w1b2")]),
        size: 4 * BLOCK,
        refs: Vec::new(),
        lease_epoch: epoch,
        request_id: 0xC3,
    };
    let r3 = pc
        .ship(&auth.endpoint, p3)
        .await
        .expect("the stale claim chains");
    let publish::PublishReply::DeltaUsed { used, .. } = r3 else {
        panic!("reply shape: {r3:?}");
    };
    assert!(used);

    // THE COMPOSITION: the durable layout carries EVERY writer's blocks.
    let final_layout = owner_layout(&owner_be, ino).await;
    let map = final_layout.block_map.expect("striped map");
    assert_eq!(map.get(&0).map(String::as_str), Some("be://data:w1b0"));
    assert_eq!(map.get(&1).map(String::as_str), Some("be://data:w2b1"));
    assert_eq!(map.get(&2).map(String::as_str), Some("be://data:w1b2"));

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

/// Contract (the s11-range leg's second conviction, found live on the
/// rung's own from-zero pass — C8 drift 8742 with clean bytes): TWO
/// same-ino chained merges landing in ONE aggregated conveyor pass, the
/// second at the CHAIN CAP, must never take the owner-compaction arm
/// against the COMMITTED fold — that full `Put` erases the earlier
/// member's just-staged entries from the layout while their ledger refs
/// land (the C8 mint: "1 durable record vs 0 layout references"). The
/// law: compaction only when the ino has no earlier member in THIS pass;
/// a batch-prior ino stays on the chained delta past the cap (the next
/// pass compacts).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_prior_ino_never_compacts_over_its_own_pass_mates() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli", false).await;
    let auth = start_authority(Arc::clone(&owner_be), "assembler-authority");
    let (client, pc) = arm_client(&auth, &client_be).await;
    let epoch = client.lease_epoch();
    let ino = shipped_create(&client_be, "batchmate.bin").await;
    let base = base_layout_bytes(4 * BLOCK, &[]);
    publish::set_layout_and_size(&client_be, ino, &base, 4 * BLOCK, &[])
        .await
        .expect("the base Put lands");

    // Drive the durable chain to ONE BELOW the cap: cap = 2, one staged
    // link — the leg's mid-stream shape.
    squeezefs::routing::set_layout_delta_chain_override(Some(2));
    let r = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::MergeLayoutAndSize {
                ino,
                delta: delta(
                    4 * BLOCK,
                    &[(0, "be://data:pre0")],
                    Some((0, squeezefs::dlm::mint_layout_version())),
                )
                .encode(),
                full_layout: base_layout_bytes(4 * BLOCK, &[(0, "be://data:pre0")]),
                size: 4 * BLOCK,
                refs: Vec::new(),
                lease_epoch: epoch,
                request_id: 0xD0,
            },
        )
        .await
        .expect("pre-cap link lands");
    assert!(matches!(
        r,
        publish::PublishReply::DeltaUsed { used: true, .. }
    ));

    // Two members of ONE pass for this ino, from TWO clients (two wire
    // lanes — the leg's two co-writers; one client's lane mutex would
    // serialize them into separate passes). The conveyor hold seam makes
    // the single-pass accumulation deterministic. Mate 1 stages the
    // cap-th delta; mate 2's batch-head read sits AT the cap — pre-fix
    // it took the owner-compaction arm against the COMMITTED fold and
    // ERASED mate 1's just-staged entry (the leg's C8 mint: ledger ref
    // landed, layout entry gone).
    let pc2 = publish::PublishClient::new("node-assembler-b2", SECRET.to_vec());
    squeezefs::meta_backend::kv::backend::TEST_LAYOUT_MERGE_HOLD_MS.store(150, Ordering::Relaxed);
    let (r1, r2) = tokio::join!(
        pc.ship(
            &auth.endpoint,
            publish::PublishCall::MergeLayoutAndSize {
                ino,
                delta: delta(
                    4 * BLOCK,
                    &[(2, "be://data:mate2")],
                    Some((0, squeezefs::dlm::mint_layout_version())),
                )
                .encode(),
                full_layout: base_layout_bytes(4 * BLOCK, &[(2, "be://data:mate2")]),
                size: 4 * BLOCK,
                refs: Vec::new(),
                lease_epoch: epoch,
                request_id: 0xD8,
            },
        ),
        pc2.ship(
            &auth.endpoint,
            publish::PublishCall::MergeLayoutAndSize {
                ino,
                delta: delta(
                    4 * BLOCK,
                    &[(3, "be://data:mate3")],
                    Some((0, squeezefs::dlm::mint_layout_version())),
                )
                .encode(),
                full_layout: base_layout_bytes(4 * BLOCK, &[(3, "be://data:mate3")]),
                size: 4 * BLOCK,
                refs: Vec::new(),
                lease_epoch: epoch,
                request_id: 0xD9,
            },
        ),
    );
    squeezefs::meta_backend::kv::backend::TEST_LAYOUT_MERGE_HOLD_MS.store(0, Ordering::Relaxed);
    squeezefs::routing::set_layout_delta_chain_override(None);
    r1.expect("pass mate 1 lands");
    r2.expect("pass mate 2 lands");

    // The composition: EVERY entry survives — the pre-cap link AND both
    // pass mates (pre-fix one mate's entry was erased by the other's
    // committed-fold compaction Put).
    let final_layout = owner_layout(&owner_be, ino).await;
    let map = final_layout.block_map.expect("striped map");
    assert_eq!(map.get(&0).map(String::as_str), Some("be://data:pre0"));
    assert_eq!(map.get(&2).map(String::as_str), Some("be://data:mate2"));
    assert_eq!(map.get(&3).map(String::as_str), Some("be://data:mate3"));

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

/// Contract (the s11-range leg's THIRD conviction — the zeros/remove
/// class): a shipped **full Put** (`SetLayoutAndSize` — the vehicle of
/// every save the delta wire cannot express, hole-punch removals above
/// all) is authoritative EXACTLY FOR THE SHIPPER'S CUSTODY SPANS. The
/// owner applies it CUSTODY-SCOPED: durable entries for blocks the
/// shipper's live grants do not overlap are PRESERVED (a concurrent
/// peer's publish between the shipper's base read and this apply must
/// never be erased by an unconditional Put — the C8 dangler + double-
/// release mint the leg convicted); entries INSIDE its custody follow
/// the Put verbatim (absence there IS the removal intent). A whole-file
/// holder's Put stays fully authoritative (unscoped — solo/whole-file
/// byte-identical).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shipped_full_put_is_custody_scoped_and_never_erases_a_peer() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli", false).await;
    let auth = start_authority(Arc::clone(&owner_be), "assembler-authority");
    let (client, pc) = arm_client(&auth, &client_be).await;
    let epoch = client.lease_epoch();
    let ino = shipped_create(&client_be, "scoped.bin").await;
    let base = base_layout_bytes(4 * BLOCK, &[]);
    publish::set_layout_and_size(&client_be, ino, &base, 4 * BLOCK, &[])
        .await
        .expect("the base Put lands");

    // This client (NODE_A) takes RANGE custody of block 0 only.
    let _g = client
        .acquire_range(ino, (0, BLOCK), (0, BLOCK), Duration::from_secs(1))
        .await
        .expect("block-0 range custody");

    // The PEER's publish (a second holder's frame): block 1's entry.
    let peer_lease = auth
        .owner
        .join(&data_grant::JoinFrame {
            schema: data_grant::CUSTODY_SCHEMA,
            client: "node-assembler-b".to_string(),
            pr_key: 0,
            prior_epoch: None,
        })
        .expect("peer joins");
    let _pg = auth
        .owner
        .grant_ranged(
            &data_grant::AcquireFrame {
                schema: data_grant::CUSTODY_SCHEMA,
                client: "node-assembler-b".to_string(),
                lease_epoch: peer_lease.epoch,
                ino,
                span: Some((BLOCK, 2 * BLOCK)),
                concurrent_write: false,
                wait_ms: 5_000,
                desired: Some((BLOCK, 2 * BLOCK)),
            },
            (BLOCK, 2 * BLOCK),
        )
        .await
        .expect("peer's block-1 custody");
    let pc2 = publish::PublishClient::new("node-assembler-b", SECRET.to_vec());
    let r = pc2
        .ship(
            &auth.endpoint,
            publish::PublishCall::MergeLayoutAndSize {
                ino,
                delta: delta(
                    4 * BLOCK,
                    &[(1, "be://data:peer1")],
                    Some((0, squeezefs::dlm::mint_layout_version())),
                )
                .encode(),
                full_layout: base_layout_bytes(4 * BLOCK, &[(1, "be://data:peer1")]),
                size: 4 * BLOCK,
                refs: Vec::new(),
                lease_epoch: peer_lease.epoch,
                request_id: 0xF1,
            },
        )
        .await
        .expect("the peer's block-1 publish lands");
    assert!(matches!(
        r,
        publish::PublishReply::DeltaUsed { used: true, .. }
    ));

    // NODE_A ships a FULL PUT computed from a base that NEVER SAW the
    // peer's block 1 (the zeros/remove-class save's exact shape): block 0
    // present, block 1 ABSENT. Pre-fix the owner applied it verbatim and
    // erased the peer's entry (its ledger take dangling — the C8 mint).
    pc.ship(
        &auth.endpoint,
        publish::PublishCall::SetLayoutAndSize {
            ino,
            layout: base_layout_bytes(4 * BLOCK, &[(0, "be://data:mine0")]),
            size: 4 * BLOCK,
            refs: Vec::new(),
            lease_epoch: epoch,
            request_id: 0xF2,
        },
    )
    .await
    .expect("the scoped full Put lands");

    let map = owner_layout(&owner_be, ino).await.block_map.expect("map");
    assert_eq!(
        map.get(&0).map(String::as_str),
        Some("be://data:mine0"),
        "the shipper's own-custody entry follows the Put"
    );
    assert_eq!(
        map.get(&1).map(String::as_str),
        Some("be://data:peer1"),
        "the PEER's entry survives — a shipped full Put is custody-scoped"
    );

    // The removal intent INSIDE custody still works: a Put absent block 0
    // removes it while the peer's entry keeps surviving.
    pc.ship(
        &auth.endpoint,
        publish::PublishCall::SetLayoutAndSize {
            ino,
            layout: base_layout_bytes(4 * BLOCK, &[]),
            size: 4 * BLOCK,
            refs: Vec::new(),
            lease_epoch: epoch,
            request_id: 0xF3,
        },
    )
    .await
    .expect("the removal-class Put lands");
    let map = owner_layout(&owner_be, ino).await.block_map.expect("map");
    assert_eq!(map.get(&0), None, "absence inside custody IS the removal");
    assert_eq!(
        map.get(&1).map(String::as_str),
        Some("be://data:peer1"),
        "the peer's entry still survives the removal-class Put"
    );

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

/// Contract (solo untouched): the LOCAL publish path keeps the §6.2-item-9
/// version gate byte-identical — a divergent NONZERO claim from the local
/// path still refuses loud; the chained composition is the SHIPPED arm's
/// law only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_local_publish_path_keeps_the_version_gate() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (be, _p) = sandbox(dir.path(), "solo", true).await;
    use squeezefs::meta_backend::Metadata;
    let ino = be
        .create(1, "solo.bin", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("local create")
        .ino;
    let base = base_layout_bytes(BLOCK, &[]);
    be.set_layout_and_size(ino, &base, BLOCK, &[])
        .await
        .expect("base Put");
    let v1 = squeezefs::dlm::mint_layout_version();
    let d1 = delta(BLOCK, &[(0, "be://data:solo0")], Some((0, v1)));
    assert!(be
        .merge_layout_and_size(
            ino,
            &d1,
            bytes::Bytes::from(base_layout_bytes(BLOCK, &[(0, "be://data:solo0")])),
            BLOCK,
            Vec::new(),
        )
        .await
        .expect("the first link stages"));
    // A DIVERGENT nonzero claim refuses loud on the local path.
    let bogus = squeezefs::dlm::mint_layout_version();
    let v2 = squeezefs::dlm::mint_layout_version();
    let d2 = delta(BLOCK, &[(1, "be://data:solo1")], Some((bogus, v2)));
    let err = be
        .merge_layout_and_size(
            ino,
            &d2,
            bytes::Bytes::from(base_layout_bytes(BLOCK, &[(1, "be://data:solo1")])),
            BLOCK,
            Vec::new(),
        )
        .await
        .expect_err("the local version gate refuses a divergent claim");
    assert!(
        format!("{err}").contains("item 9"),
        "the refusal names the law: {err}"
    );
    shutdown(&be).await;
}

// ===========================================================================
// The ZEROS-REWRITE INTERLEAVE conviction (rung 17's open residual —
// finding #4, `.benchmarks/2026-08-17-s11-zeros-interleave-fix.md`): the
// custody-scoped full Put (finding #3's fix) was structurally DISARMED on
// every production mount, because `arm_multi_writer` never installed a
// `RangeGeometry` source on the custody owner — `install_range_geometry`
// had exactly two callers, both test fixtures. `custody_scoped_layout`'s
// no-geometry arm then applied every range holder's full Put VERBATIM
// ("peer entries at risk" — its own warn text), which is finding #3's
// clobber unfixed: two co-writers' epoch-close full Puts of one ino
// revert each other's halves to their private stale bases while the ref
// ops land — the exact 48-finding C8 mint (dangling takes on every
// superseded mint + released takes under still-referenced pass-1 keys +
// the paired double-release free refusals). The live discriminator
// re-run proved the mint CONTENT-BLIND (iso3 both-urandom and iso6
// both-zeros mint identically under a uniform barriered harness; zeros
// was the original instrument's timing accident and its read-back MASK —
// a reverted map references freed/discarded offsets, which read zeros).
// The same missing source also kept `block_size = None` at every
// production ranged acquire, silently disarming the §9.3 demotion
// barrier ("no geometry, no barrier").
// ===========================================================================

/// Contract (the conviction's REFUSAL half): a RANGE holder's full Put on
/// an authority with NO geometry source must never apply verbatim — the
/// peer's entries survive because the serve REFUSES loud (the scoped
/// apply is impossible without block walls, and a silent verbatim apply
/// is the C8 mint). RED at f609f984: the Put applied verbatim and erased
/// the peer's entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_range_holders_put_without_geometry_refuses_rather_than_reverting_a_peer() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli", false).await;
    // The PRODUCTION shape at the conviction: no geometry source at all.
    let auth = start_authority_geo(Arc::clone(&owner_be), "assembler-authority", None);
    let (client, pc) = arm_client(&auth, &client_be).await;
    let epoch = client.lease_epoch();
    let ino = shipped_create(&client_be, "nogeo.bin").await;
    let base = base_layout_bytes(4 * BLOCK, &[(0, "be://data:mine0")]);
    publish::set_layout_and_size(&client_be, ino, &base, 4 * BLOCK, &[])
        .await
        .expect("the base Put lands");

    // This client (NODE_A) takes RANGE custody of block 0 only.
    let _g = client
        .acquire_range(ino, (0, BLOCK), (0, BLOCK), Duration::from_secs(1))
        .await
        .expect("block-0 range custody");

    // The PEER's newer publish: block 1's entry (the half a stale Put
    // must never revert).
    let peer_lease = auth
        .owner
        .join(&data_grant::JoinFrame {
            schema: data_grant::CUSTODY_SCHEMA,
            client: "node-assembler-b".to_string(),
            pr_key: 0,
            prior_epoch: None,
        })
        .expect("peer joins");
    let _pg = auth
        .owner
        .grant_ranged(
            &data_grant::AcquireFrame {
                schema: data_grant::CUSTODY_SCHEMA,
                client: "node-assembler-b".to_string(),
                lease_epoch: peer_lease.epoch,
                ino,
                span: Some((BLOCK, 2 * BLOCK)),
                concurrent_write: false,
                wait_ms: 5_000,
                desired: Some((BLOCK, 2 * BLOCK)),
            },
            (BLOCK, 2 * BLOCK),
        )
        .await
        .expect("peer's block-1 custody");
    let pc2 = publish::PublishClient::new("node-assembler-b", SECRET.to_vec());
    let r = pc2
        .ship(
            &auth.endpoint,
            publish::PublishCall::MergeLayoutAndSize {
                ino,
                delta: delta(
                    4 * BLOCK,
                    &[(1, "be://data:peer1")],
                    Some((0, squeezefs::dlm::mint_layout_version())),
                )
                .encode(),
                full_layout: base_layout_bytes(
                    4 * BLOCK,
                    &[(0, "be://data:mine0"), (1, "be://data:peer1")],
                ),
                size: 4 * BLOCK,
                refs: Vec::new(),
                lease_epoch: peer_lease.epoch,
                request_id: 0xE1,
            },
        )
        .await
        .expect("the peer's block-1 publish lands");
    assert!(matches!(
        r,
        publish::PublishReply::DeltaUsed { used: true, .. }
    ));

    // NODE_A's stale full Put (its base never saw the peer's block 1).
    // With no geometry the owner CANNOT scope it — the serve must refuse
    // loud (naming the arm), never apply it verbatim.
    let err = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::SetLayoutAndSize {
                ino,
                layout: base_layout_bytes(4 * BLOCK, &[(0, "be://data:mine0v2")]),
                size: 4 * BLOCK,
                refs: Vec::new(),
                lease_epoch: epoch,
                request_id: 0xE2,
            },
        )
        .await
        .expect_err("a range holder's Put without geometry refuses instead of applying verbatim");
    assert!(
        format!("{err}").contains("cannot be custody-scoped"),
        "the refusal names the disarmed scoping: {err}"
    );

    // Nothing moved: the peer's entry AND the holder's own pre-Put entry
    // both survive (the refusal applied nothing).
    let map = owner_layout(&owner_be, ino).await.block_map.expect("map");
    assert_eq!(
        map.get(&1).map(String::as_str),
        Some("be://data:peer1"),
        "the peer's entry survives — the un-scopable Put was refused, not applied"
    );
    assert_eq!(
        map.get(&0).map(String::as_str),
        Some("be://data:mine0"),
        "the holder's durable entry is untouched by the refused Put"
    );

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

/// Contract (the conviction's ARMING half): the PRODUCTION geometry
/// source — `multi_writer::router_range_geometry`, the one
/// `arm_multi_writer` now installs — answers `(size, block)` from the
/// authority's own planes (layout head size; data-plane block size), and
/// through it the iso2 interleave scopes: the peer's newer entry
/// survives a range holder's stale full Put while the holder's
/// own-custody entry follows it. Compile-RED at f609f984 (the source did
/// not exist; nothing production-shaped could scope a Put).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_production_range_geometry_arms_the_scoped_put() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli", false).await;

    // The production data plane the source reads its block size from.
    let alloc = Arc::new(
        squeezefs::block_allocator::BlockAllocator::new("nogeo-data")
            .await
            .expect("allocator"),
    );
    let chunk = alloc.chunk_size();
    assert_eq!(chunk, BLOCK, "the suite's shapes assume the default block");
    let backend = Arc::new(squeezefs::routing::BackendRouter::new(
        Arc::clone(&alloc),
        Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
            dir.path().join("nogeo-dev").to_str().unwrap(),
        )),
        Arc::new(AtomicU64::new(chunk)),
    ));
    let geometry = squeezefs::multi_writer::router_range_geometry(Arc::clone(&owner_be), backend);

    let auth = start_authority_geo(
        Arc::clone(&owner_be),
        "assembler-authority",
        Some(Arc::clone(&geometry)),
    );
    let (client, pc) = arm_client(&auth, &client_be).await;
    let epoch = client.lease_epoch();
    let ino = shipped_create(&client_be, "prodgeo.bin").await;

    // The source's own contract: absent layout -> (0, block); a
    // layout-bearing ino -> (its size, block).
    assert_eq!(
        geometry.geometry(ino).await,
        Some((0, BLOCK)),
        "absent layout answers size 0 with the data plane's block size"
    );
    let base = base_layout_bytes(4 * BLOCK, &[(0, "be://data:mine0")]);
    publish::set_layout_and_size(&client_be, ino, &base, 4 * BLOCK, &[])
        .await
        .expect("the base Put lands");
    assert_eq!(
        geometry.geometry(ino).await,
        Some((4 * BLOCK, BLOCK)),
        "a layout-bearing ino answers its durable head's size"
    );

    // The iso2 interleave, production-armed: holder takes block 0, the
    // peer publishes block 1, the holder ships a STALE full Put.
    let _g = client
        .acquire_range(ino, (0, BLOCK), (0, BLOCK), Duration::from_secs(1))
        .await
        .expect("block-0 range custody");
    let peer_lease = auth
        .owner
        .join(&data_grant::JoinFrame {
            schema: data_grant::CUSTODY_SCHEMA,
            client: "node-assembler-b".to_string(),
            pr_key: 0,
            prior_epoch: None,
        })
        .expect("peer joins");
    let _pg = auth
        .owner
        .grant_ranged(
            &data_grant::AcquireFrame {
                schema: data_grant::CUSTODY_SCHEMA,
                client: "node-assembler-b".to_string(),
                lease_epoch: peer_lease.epoch,
                ino,
                span: Some((BLOCK, 2 * BLOCK)),
                concurrent_write: false,
                wait_ms: 5_000,
                desired: Some((BLOCK, 2 * BLOCK)),
            },
            (BLOCK, 2 * BLOCK),
        )
        .await
        .expect("peer's block-1 custody");
    let pc2 = publish::PublishClient::new("node-assembler-b", SECRET.to_vec());
    pc2.ship(
        &auth.endpoint,
        publish::PublishCall::MergeLayoutAndSize {
            ino,
            delta: delta(
                4 * BLOCK,
                &[(1, "be://data:peer1")],
                Some((0, squeezefs::dlm::mint_layout_version())),
            )
            .encode(),
            full_layout: base_layout_bytes(
                4 * BLOCK,
                &[(0, "be://data:mine0"), (1, "be://data:peer1")],
            ),
            size: 4 * BLOCK,
            refs: Vec::new(),
            lease_epoch: peer_lease.epoch,
            request_id: 0xE3,
        },
    )
    .await
    .expect("the peer's block-1 publish lands");

    pc.ship(
        &auth.endpoint,
        publish::PublishCall::SetLayoutAndSize {
            ino,
            layout: base_layout_bytes(4 * BLOCK, &[(0, "be://data:mine0v2")]),
            size: 4 * BLOCK,
            refs: Vec::new(),
            lease_epoch: epoch,
            request_id: 0xE4,
        },
    )
    .await
    .expect("the production-armed scoped Put lands");

    let map = owner_layout(&owner_be, ino).await.block_map.expect("map");
    assert_eq!(
        map.get(&0).map(String::as_str),
        Some("be://data:mine0v2"),
        "the holder's own-custody entry follows the Put"
    );
    assert_eq!(
        map.get(&1).map(String::as_str),
        Some("be://data:peer1"),
        "the peer's newer entry survives the stale full Put — scoping engaged \
         through the PRODUCTION geometry source"
    );

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

// ===========================================================================
// Rung 18: the kernel-coalesced oversized extent ships CHUNKED
// ===========================================================================

/// Contract (CONVICTED LIVE on the s11-subblock leg, 2026-08-18): a
/// BUFFERED writer's 4 KiB records coalesce in the page cache and arrive
/// as ONE 1 MiB FUSE WRITE (`max_write`) — a legal, ordinary shape — and
/// the extent-ship of that slice built a 1,048,633 B publish frame, which
/// the cluster wire's CONTROL class cap (1 MiB) refused: the app saw
/// EINVAL on a healthy fleet ("S9 publish write_extent of 1048633 B
/// exceeds the cluster wire's CONTROL class cap").
///
/// The law: [`extent_ship::ship_extent`] CHUNKS a payload larger than the
/// wire's admissible envelope into byte-adjacent sub-extents, each its own
/// witnessed frame (own `request_id`, own retention record — retention
/// granularity == wire granularity, so every coverage-release path keeps
/// its meaning), and the assembler's merged image is byte-exact across
/// the chunk boundary. The wire cap itself stays (a loud guard against
/// any OTHER oversized frame class).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_extent_larger_than_the_wire_cap_ships_chunked_and_assembles_exact() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own-chunk", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli-chunk", false).await;
    let auth = start_authority(Arc::clone(&owner_be), "assembler-authority");
    let (_client, _pc) = arm_client(&auth, &client_be).await;
    let ino = shipped_create(&client_be, "chunked.bin").await;

    let asm = Arc::new(MockAssembler::default());
    asm.install();

    // 1 MiB + 8 KiB: strictly larger than the control frame cap, so the
    // pre-fix single-frame ship REFUSES (the live EINVAL signature).
    let len = 1024 * 1024 + 8192;
    let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    let s0 = extent_stats();
    extent_ship::ship_extent(
        &client_be,
        ino,
        0,
        4096,
        bytes::Bytes::from(data.clone()),
        0,
    )
    .await
    .unwrap_or_else(|e| {
        panic!(
            "a kernel-coalesced slice larger than the wire cap must ship \
             CHUNKED, never refuse (the s11-subblock live EINVAL): {e:?}"
        )
    });
    let s1 = extent_stats();
    assert!(
        s1.extent_shipped - s0.extent_shipped >= 2,
        "an over-cap payload is at least two witnessed frames (shipped d={})",
        s1.extent_shipped - s0.extent_shipped
    );
    assert_eq!(
        s1.extent_served - s0.extent_served,
        s1.extent_shipped - s0.extent_shipped,
        "shipped ≡ served (the engagement law holds per chunk)"
    );

    // Byte-exact assembly across the chunk boundary.
    let images = asm.images.lock();
    let img = images
        .get(&(ino, 0))
        .expect("the assembler holds block 0's image");
    assert!(
        img.len() >= 4096 + len,
        "the merged image covers the whole span"
    );
    assert_eq!(
        &img[4096..4096 + len],
        &data[..],
        "the assembled bytes are exact across the chunk boundary"
    );

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

// ===========================================================================
// Rung 18: a scoped Put's truth-span EXCLUDES demoted regions
// ===========================================================================

/// Contract (CONVICTED LIVE on the s11-subblock priced row, 2026-08-18 —
/// fsck `[C2] leaked block` + `[C8] 1 durable record vs 0 layout
/// references`, drift 2): the custody-scoped Put's truth-span was the
/// holder's SPANS — including bytes inside a DEMOTED region, where the
/// holder never DMAs (KD-MW-8: the AUTHORITY is the single publisher of
/// a demoted block; every holder ships extents) and its RAM map is
/// legitimately stale. The holder's epoch-close Put then re-inserted its
/// STALE key for the demoted block, REVERTING the authority's assembly —
/// the assembly's key lost its map reference while its take stood (the
/// C8 dangling-take + C2 leak pair), and until the next fold re-covered
/// it the map named superseded bytes.
///
/// The law: a demoted region is NOBODY's Put-truth — the scoped apply
/// preserves the DURABLE (authority-assembled) entry for any block
/// overlapping a demoted region, both directions (the holder's stale
/// presence never overwrites; its absence never removes).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scoped_put_never_reverts_a_demoted_blocks_assembly() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own-dem", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli-dem", false).await;
    let auth = start_authority(Arc::clone(&owner_be), "assembler-authority");
    let (client, pc) = arm_client(&auth, &client_be).await;
    let epoch = client.lease_epoch();
    let ino = shipped_create(&client_be, "demoted.bin").await;

    // The holder's custody: blocks 0..2 (its stream's stripes).
    let _g = client
        .acquire_range(ino, (0, 2 * BLOCK), (0, 2 * BLOCK), Duration::from_secs(1))
        .await
        .expect("blocks 0-1 range custody");

    // The AUTHORITY's assembly published block 0 (the fold's merge) —
    // and block 0 is DEMOTED (the barrier acked: both holders ship
    // extents there, the authority assembles).
    let base = base_layout_bytes(
        2 * BLOCK,
        &[(0, "be://data/K_assembly"), (1, "be://data/K_holder_old")],
    );
    use squeezefs::meta_backend::Metadata as _;
    owner_be
        .set_layout_and_size(ino, &base, 2 * BLOCK, &[])
        .await
        .expect("the authority's assembled layout");
    assert!(
        squeezefs::dlm::adopt_demoted_region(ino, (0, BLOCK)),
        "block 0's region marks demoted on the authority's own table"
    );

    // The holder's epoch-close full Put: its RAM view of block 0 is
    // STALE (it never DMAs a demoted block); block 1 is its live truth.
    let put = base_layout_bytes(
        2 * BLOCK,
        &[(0, "be://data/K_stale"), (1, "be://data/K_holder_new")],
    );
    let reply = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::SetLayoutAndSize {
                ino,
                layout: put,
                size: 2 * BLOCK,
                refs: vec![],
                lease_epoch: epoch,
                request_id: 0xD3,
            },
        )
        .await
        .expect("the scoped Put lands");
    assert!(matches!(reply, publish::PublishReply::Unit), "{reply:?}");

    let final_layout = owner_layout(&owner_be, ino).await;
    let map = final_layout.block_map.expect("striped map");
    assert_eq!(
        map.get(&0).map(String::as_str),
        Some("be://data/K_assembly"),
        "a DEMOTED block's durable entry is the AUTHORITY's assembly — a \
         holder's stale Put must never revert it (the s11-subblock C8/C2 \
         mint)"
    );
    assert_eq!(
        map.get(&1).map(String::as_str),
        Some("be://data/K_holder_new"),
        "the holder's NON-demoted custody still applies (the scoping law \
         unchanged outside demoted regions)"
    );

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

/// Contract (the dangling-take mint's SECOND face, same live row): a
/// served scoped Put reads the durable head, composes, and commits under
/// the per-ino serve stripe — but the AUTHORITY's OWN layout publishes
/// of a range-granted ino (the assembler's fold, its writeback) ran
/// OUTSIDE it, so a fold's merge landing inside a Put's read→commit
/// window was erased by the Put's composed full map: the fold's
/// freshly-taken key lost its map reference while its take stood (fsck
/// [C8] '1 durable vs 0 layout references [ino 2 idx 0]' + [C2] leak on
/// the re-run). The law, structurally: a LOCAL layout publish of a
/// RANGE-GRANTED ino serializes under the SAME serve stripe (and a
/// custody-free ino pays nothing — the guard is None).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_publish_of_a_range_granted_ino_parks_on_the_serve_window() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own-window", true).await;
    let auth = start_authority(Arc::clone(&owner_be), "assembler-authority");

    // A range-granted ino on the authority's own table (the arbiter's
    // LOCK_MAP is process-global — the owner's grant IS the state). Both
    // inos are REAL records (the local publish path reads the inode).
    let ino = publish::create_with_rdev_size(&owner_be, 1, "window.bin", libc::S_IFREG | 0o644, 0, 0, 0, 0)
        .await
        .expect("local create")
        .ino;
    let free_ino = publish::create_with_rdev_size(&owner_be, 1, "solo.bin", libc::S_IFREG | 0o644, 0, 0, 0, 0)
        .await
        .expect("local create")
        .ino;
    let lease = auth
        .owner
        .join(&data_grant::JoinFrame {
            schema: data_grant::CUSTODY_SCHEMA,
            client: "node-window-a".to_string(),
            pr_key: 0,
            prior_epoch: None,
        })
        .expect("holder joins");
    let _grant = auth
        .owner
        .grant_ranged(
            &data_grant::AcquireFrame {
                schema: data_grant::CUSTODY_SCHEMA,
                client: "node-window-a".to_string(),
                lease_epoch: lease.epoch,
                ino,
                span: Some((0, BLOCK)),
                concurrent_write: false,
                wait_ms: 1_000,
                desired: Some((0, BLOCK)),
            },
            (0, BLOCK),
        )
        .await
        .expect("the range grant");

    // Hold the serve window open (the scoped Put's read→commit span),
    // then drive the authority's own LOCAL publish: it must PARK until
    // the window closes — never land inside it.
    let window = publish::test_lock_serve_ino(ino).await;
    let be = Arc::clone(&owner_be);
    let base = base_layout_bytes(BLOCK, &[(0, "be://data/K_fold")]);
    let publish_task = tokio::spawn(async move {
        publish::set_layout_and_size(&be, ino, &base, BLOCK, &[]).await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !publish_task.is_finished(),
        "a LOCAL layout publish of a RANGE-GRANTED ino landed INSIDE a held \
         serve window — the fold-vs-scoped-Put erasure race is open (the \
         s11-subblock dangling-take mint)"
    );
    drop(window);
    publish_task
        .await
        .expect("publish task")
        .expect("the parked publish lands after the window closes");

    // The custody-free control: no grant, no guard — a local publish of
    // an un-granted ino never pays the stripe (KD-MW-12's shape).
    let window2 = publish::test_lock_serve_ino(free_ino).await;
    let be2 = Arc::clone(&owner_be);
    let base2 = base_layout_bytes(BLOCK, &[(0, "be://data/K_solo")]);
    let free_task = tokio::spawn(async move {
        publish::set_layout_and_size(&be2, free_ino, &base2, BLOCK, &[]).await
    });
    tokio::time::timeout(Duration::from_secs(5), free_task)
        .await
        .expect("a custody-free ino's local publish never parks on the serve stripe")
        .expect("publish task")
        .expect("publish ok");
    drop(window2);

    auth.listener.shutdown();
    shutdown(&owner_be).await;
}

/// Contract (the mint's THIRD face, same live row): a SERVED
/// layout-class publish commits on the backend directly — the authority
/// fs's RAM view of the ino must be INVALIDATED at that commit, or its
/// own fold's displaced-release set is computed from a view that never
/// saw the peer's last direct merge (the dangling-take + leak pair,
/// reproduced at drift 2 on the run AFTER the first two faces were
/// fixed). The pin: the installed sink fires with the served verb's ino
/// on the committed path and stays silent on a refused one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_served_layout_publish_invalidates_the_authoritys_ram_view() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own-inval", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli-inval", false).await;
    let auth = start_authority(Arc::clone(&owner_be), "assembler-authority");
    let (client, pc) = arm_client(&auth, &client_be).await;
    let epoch = client.lease_epoch();
    let ino = shipped_create(&client_be, "inval.bin").await;

    let seen: Arc<parking_lot::Mutex<Vec<u64>>> = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    publish::install_served_layout_invalidation(Arc::new(move |i: u64| {
        sink.lock().push(i);
    }));

    let base = base_layout_bytes(BLOCK, &[(0, "be://data/K_direct")]);
    pc.ship(
        &auth.endpoint,
        publish::PublishCall::SetLayoutAndSize {
            ino,
            layout: base,
            size: BLOCK,
            refs: vec![],
            lease_epoch: epoch,
            request_id: 0xF1,
        },
    )
    .await
    .expect("the served Put lands");
    assert!(
        seen.lock().contains(&ino),
        "a COMMITTED served layout publish must invalidate the authority \
         fs's RAM view of ino {ino} (the dangling-take mint's third face) \
         — sink saw {:?}",
        seen.lock()
    );

    // The refused path stays silent (nothing was applied).
    seen.lock().clear();
    let stale = publish::PublishCall::SetLayoutAndSize {
        ino,
        layout: base_layout_bytes(BLOCK, &[(0, "be://data/K_zombie")]),
        size: BLOCK,
        refs: vec![],
        lease_epoch: epoch + 999,
        request_id: 0xF2,
    };
    let _ = pc
        .ship(&auth.endpoint, stale)
        .await
        .expect_err("a dead era's Put refuses");
    assert!(
        seen.lock().is_empty(),
        "a REFUSED serve applied nothing and must invalidate nothing"
    );

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}
