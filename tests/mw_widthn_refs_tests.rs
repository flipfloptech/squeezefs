//! **The WIDTH-N same-ino publish REFS composition** — DLM S11 rung 19
//! (rung 18's standing-red residual #1,
//! `.benchmarks/2026-08-18-s11-mpiio-row.md`: "caller-computed block-ref
//! deltas are not atomic with the chain-composed map").
//!
//! # The law these pins encode (each red-first against the rung-18 tip)
//!
//! On every AUTHORITY-COMPOSED layout commit the staged durable
//! accounting is computed FROM THE COMPOSITION ITSELF — the folded
//! durable head this very commit composes onto — never taken from the
//! caller's frame, which at width N legitimately lags its peers (the
//! caller computed it against its private RAM base under its own
//! `INODE_META_LOCKS`, an ocean away from the owner's chain):
//!
//! 1. **The chain-onto-head merge** (`merge_layout_and_size_chained`,
//!    both the aggregated conveyor member and the direct path): the
//!    delta's entries displace whatever the DURABLE HEAD holds at those
//!    indices — a caller frame that never saw the head's binding strands
//!    the displaced record ("1 durable vs 0 layout references", the bc
//!    row's C8 face), and a caller frame releasing a key the head never
//!    held deletes nothing today but deletes a LIVE record the moment
//!    the frames interleave (the swapped pair's other half).
//! 2. **The custody-scoped Put** (`custody_scoped_layout`): an entry the
//!    scope DROPS (out-of-custody / demoted) must contribute NO
//!    accounting — staging the caller's frame verbatim mints the exact
//!    swapped pair (a durable-without-map take + a map-without-durable
//!    entry) the rung-18 note adjudicated.
//!
//! The resolver seam (`block_refs::install_block_ref_resolver`) is the
//! production `BackendRouter::block_ref_for` behind a hook, installed at
//! `multi_writer::arm_multi_writer` — these pins install a synthetic one
//! (key → its `volume_tag` hash) so the ledger oracle is exact without a
//! data plane.
//!
//! **No numbers here — ruling D11.** The live acceptance is the
//! `s11-blockcyclic` row's fsck/C8 half ×3 from zero
//! (`tests/run_mw_matrix.sh`); this file is the deterministic cargo port
//! per the repro-port mandate.

use squeezefs::data_grant::{self, WriteCustodyClient, WriteCustodyOwner};
use squeezefs::layout_wire::{LayoutDelta, LayoutMetadata};
use squeezefs::membership::{LeaseClock, LeaseClocks};
use squeezefs::meta_backend::kv::block_refs::{self, BlockRef, BlockRefOp};
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::meta_backend::{open_routed_meta_set, plan_meta_slot_set, RoutedMetaBackend};
use squeezefs::meta_ship::{self as ship, publish, OwnerMap, PeerOwner};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

const SECRET: &[u8] = b"s11-widthn-refs-storage-trust-secret";
const VOL_LEN: u64 = 256 * 1024 * 1024;
const NODE_A: &str = "node-widthn-a";
const BLOCK: u64 = 4 * 1024 * 1024;
/// The synthetic data volume every test reference lands on.
const TEST_TAG: u64 = 0x5157_4944_5448_4e01; // "QWIDTHN"-ish, stable

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
        publish::uninstall_served_layout_invalidation();
        ship::disarm_ownership();
        block_refs::uninstall_block_ref_resolver();
        squeezefs::data_custody::test_reset_custody_generation();
        squeezefs::data_custody::test_clear_poison();
    }
}

fn restore() -> Restore {
    Restore
}

// ---------------------------------------------------------------------------
// Fixtures (the mw_authority_assembler_tests shapes + the bit-9 ledger)
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

/// One opened single-volume set. `stamp` engages bit 15 (the version
/// gate) AND bit 9 (the durable block-reference ledger — what makes the
/// accounting oracle real) between format and open, the KD-MW-1 fleet
/// format shape.
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
        sb::set_block_refcounts_bit(&p)
            .await
            .expect("stamp the durable block-reference ledger bit");
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
    endpoint: String,
}

fn start_authority(inner: Arc<RoutedMetaBackend>, tag: &str) -> Authority {
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
    owner.install_range_geometry(data_grant::fixed_range_geometry(16 * BLOCK, BLOCK));
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
    Authority { listener, endpoint }
}

async fn arm_client(
    auth: &Authority,
    client_be: &Arc<RoutedMetaBackend>,
) -> (Arc<WriteCustodyClient>, Arc<publish::PublishClient>) {
    let foreign: Vec<(usize, PeerOwner)> = (0..client_be.volumes.len())
        .map(|v| (v, PeerOwner::new("widthn-authority", &auth.endpoint)))
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

/// The synthetic resolver: every key string resolves onto [`TEST_TAG`]
/// at a stable per-key block index (`volume_tag` doubles as the hash) —
/// the same arithmetic the caller-frame builders below use, so the
/// ledger oracle compares like with like.
fn install_test_resolver() {
    block_refs::install_block_ref_resolver(Arc::new(|key: &str, ino: u64, idx: u32| {
        Some(BlockRef {
            vol_tag: TEST_TAG,
            block_idx: block_refs::volume_tag(key),
            owner_ino: ino,
            block_index: idx,
        })
    }));
}

/// A caller-frame reference op, through the SAME arithmetic as the
/// resolver above.
fn frame_op(key: &str, ino: u64, idx: u32, take: bool) -> publish::WireBlockRefOp {
    let r = BlockRef {
        vol_tag: TEST_TAG,
        block_idx: block_refs::volume_tag(key),
        owner_ino: ino,
        block_index: idx,
    };
    publish::WireBlockRefOp::from(&if take {
        BlockRefOp::taken(r)
    } else {
        BlockRefOp::released(r)
    })
}

fn kid(key: &str) -> u64 {
    block_refs::volume_tag(key)
}

/// The durable ledger, as `(block_idx, owner_ino, block_index)` sorted —
/// the oracle every pin compares against the composed map.
async fn ledger(be: &Arc<RoutedMetaBackend>) -> Vec<(u64, u64, u32)> {
    let mut out = Vec::new();
    for kv in &be.volumes {
        for r in kv.block_ref_scan(TEST_TAG).await.expect("ledger scan") {
            out.push((r.block_idx, r.owner_ino, r.block_index));
        }
    }
    out.sort_unstable();
    out
}

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

fn delta(size: u64, entries: &[(u32, &str)], versions: (u64, u64)) -> LayoutDelta {
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
    d.set_versions(versions.0, versions.1);
    d
}

async fn shipped_create(client_be: &Arc<RoutedMetaBackend>, name: &str) -> u64 {
    publish::create_with_rdev_size(client_be, 1, name, libc::S_IFREG | 0o644, 0, 0, 0, 0)
        .await
        .expect("a live-era shipped create lands")
        .ino
}

async fn owner_map(
    be: &Arc<RoutedMetaBackend>,
    ino: u64,
) -> std::collections::HashMap<u32, String> {
    use squeezefs::meta_backend::Metadata;
    let raw = be
        .getxattr(ino, "layout")
        .await
        .expect("owner layout read")
        .expect("layout present");
    squeezefs::layout_wire::decode_base_layout(&raw)
        .expect("decodable layout")
        .block_map
        .expect("striped map")
}

// ===========================================================================
// 1. The chain-onto-head merge recomputes its accounting against the head
// ===========================================================================

/// Contract (rung 19, the bc row's C8 face): a shipped chained merge's
/// staged accounting is the delta-entries-onto-the-FOLDED-HEAD
/// transition — the head's displaced binding is RELEASED even when the
/// caller's frame never saw it, and a caller-frame release naming a key
/// the head does not hold at that index is DISCARDED (staged verbatim it
/// deletes a live record the moment frames interleave — the swapped
/// pair's loss half).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chained_merge_recomputes_refs_against_the_live_head() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own-refs", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli-refs", false).await;
    let auth = start_authority(Arc::clone(&owner_be), "widthn-authority");
    let (client, pc) = arm_client(&auth, &client_be).await;
    install_test_resolver();
    let epoch = client.lease_epoch();
    let ino = shipped_create(&client_be, "refs.bin").await;

    // The head: a pre-custody verbatim Put {0→A}, its take staged by the
    // caller's own (correct) frame.
    let reply = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::SetLayoutAndSize {
                ino,
                layout: base_layout_bytes(4 * BLOCK, &[(0, "be://data:A")]),
                size: 4 * BLOCK,
                refs: vec![frame_op("be://data:A", ino, 0, true)],
                lease_epoch: epoch,
                request_id: 0xE1,
            },
        )
        .await
        .expect("the base Put lands");
    assert!(matches!(reply, publish::PublishReply::Unit), "{reply:?}");
    assert_eq!(
        ledger(&owner_be).await,
        vec![(kid("be://data:A"), ino, 0)],
        "the base take lands (sanity)"
    );

    // W2's chained merge: entries {0→B}. Its PRIVATE frame never saw A
    // (width-N staleness) — it ships take(B) alone. The composition
    // displaces A; the accounting must say so.
    let reply = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::MergeLayoutAndSize {
                ino,
                delta: delta(
                    4 * BLOCK,
                    &[(0, "be://data:B")],
                    (0, squeezefs::dlm::mint_layout_version()),
                )
                .encode(),
                full_layout: base_layout_bytes(4 * BLOCK, &[(0, "be://data:B")]),
                size: 4 * BLOCK,
                refs: vec![frame_op("be://data:B", ino, 0, true)],
                lease_epoch: epoch,
                request_id: 0xE2,
            },
        )
        .await
        .expect("W2's merge lands");
    let publish::PublishReply::DeltaUsed { .. } = reply else {
        panic!("reply shape: {reply:?}");
    };
    let map = owner_map(&owner_be, ino).await;
    assert_eq!(map.get(&0).map(String::as_str), Some("be://data:B"));
    assert_eq!(
        ledger(&owner_be).await,
        vec![(kid("be://data:B"), ino, 0)],
        "the composition displaced A at index 0 — its record is RELEASED \
         by the merge itself, never left dangling on the caller's blind \
         frame (the bc row's '1 durable vs 0 layout references' face)"
    );

    // W3's chained merge: entries {0→C}, but its stale frame believes X
    // was displaced — the caller-frame release names a key the head does
    // not hold. The composition's truth is release(B), take(C); X's
    // release is discarded.
    let reply = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::MergeLayoutAndSize {
                ino,
                delta: delta(
                    4 * BLOCK,
                    &[(0, "be://data:C")],
                    (0, squeezefs::dlm::mint_layout_version()),
                )
                .encode(),
                full_layout: base_layout_bytes(4 * BLOCK, &[(0, "be://data:C")]),
                size: 4 * BLOCK,
                refs: vec![
                    frame_op("be://data:X", ino, 0, false),
                    frame_op("be://data:C", ino, 0, true),
                ],
                lease_epoch: epoch,
                request_id: 0xE3,
            },
        )
        .await
        .expect("W3's merge lands");
    let publish::PublishReply::DeltaUsed { .. } = reply else {
        panic!("reply shape: {reply:?}");
    };
    let map = owner_map(&owner_be, ino).await;
    assert_eq!(map.get(&0).map(String::as_str), Some("be://data:C"));
    assert_eq!(
        ledger(&owner_be).await,
        vec![(kid("be://data:C"), ino, 0)],
        "the composition's transition is B→C: B released, C taken, the \
         stale frame's X-release discarded"
    );

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

// ===========================================================================
// 1b. The recompute keys records by the GLOBAL ino (the armD conviction)
// ===========================================================================

/// Contract (the live conviction's identity face): the block-reference
/// key law is `owner_ino = the GLOBAL ino` (`block_refs.rs`'s own words).
/// The chained recompute runs INSIDE the KV volume, whose `ino` is the
/// ROUTED-LOCAL identity — on a hosted slot that reads
/// `((slot+1) << 40) | local`, and a record keyed by it is a PHANTOM no
/// release, no delete-path teardown and no oracle walk can ever match
/// (the armD tape: `owner_ino = 0xE754_0000_0000_02` beside live global
/// inos). The routed layer therefore threads the GLOBAL ino down as the
/// accounting owner, and the staged records carry IT.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_chained_recompute_keys_records_by_the_global_ino() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (be, _p) = sandbox(dir.path(), "own-ident", true).await;
    install_test_resolver();

    // The hosted-slot SHAPE without the slot machinery: the KV volume is
    // handed one (local) ino while the accounting owner is a DIFFERENT
    // (global) identity — distinct on purpose, exactly what a hosted
    // slot's `((slot+1) << 40) | local` routing produces live.
    let vol = &be.volumes[0];
    let local_rec = {
        use squeezefs::meta_backend::Metadata as _;
        vol.create(1, "local.bin", libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("local inode")
    };
    let local_ino = local_rec.ino;
    let global_ino: u64 = local_ino + 59220;
    // A live base at the local slot key (the chained arm needs an
    // existing decodable head).
    vol.set_layout_and_size(
        local_ino,
        &base_layout_bytes(BLOCK, &[(0, "be://data:A")]),
        BLOCK,
        &[],
    )
    .await
    .expect("base Put lands");

    let d = delta(
        BLOCK,
        &[(0, "be://data:B")],
        (0, squeezefs::dlm::mint_layout_version()),
    );
    let (_used, _v) = vol
        .merge_layout_and_size_chained(
            local_ino,
            global_ino,
            &d,
            bytes::Bytes::from(base_layout_bytes(BLOCK, &[(0, "be://data:B")])),
            BLOCK,
            vec![],
        )
        .await
        .expect("chained merge lands");

    let mut owners: Vec<u64> = Vec::new();
    for kv in &be.volumes {
        for r in kv.block_ref_scan(TEST_TAG).await.expect("scan") {
            owners.push(r.owner_ino);
        }
    }
    assert_eq!(
        owners,
        vec![global_ino],
        "the recomputed record keys on the GLOBAL ino — a local/guest-\
         namespaced owner is a phantom record nothing can ever release"
    );

    for vol in &be.volumes {
        vol.shutdown().await.expect("shutdown");
    }
}

// ===========================================================================
// 2. The custody-scoped Put's accounting follows the composition
// ===========================================================================

/// Contract (rung 19, the swapped-pair face verbatim): a range holder's
/// Put entry the scope DROPS (out-of-custody) contributes NO accounting —
/// staged verbatim, its take dangles ("1 durable vs 0 layout references")
/// while its release deletes the record of the binding the composition
/// KEPT ("0 durable vs 1" — the loss half). One event, one pair, the bc
/// row's drift-6 arithmetic. And inside custody the accounting is the
/// composition's own transition (the head's binding released), not the
/// caller's guess.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scoped_puts_refs_follow_the_composition_not_the_callers_frame() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own-scoped", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli-scoped", false).await;
    let auth = start_authority(Arc::clone(&owner_be), "widthn-authority");
    let (client, pc) = arm_client(&auth, &client_be).await;
    install_test_resolver();
    let epoch = client.lease_epoch();
    let ino = shipped_create(&client_be, "scoped.bin").await;

    // The pre-custody head: {0→K_old, 1→K_peer}, both takes staged.
    let reply = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::SetLayoutAndSize {
                ino,
                layout: base_layout_bytes(
                    2 * BLOCK,
                    &[(0, "be://data:K_old"), (1, "be://data:K_peer")],
                ),
                size: 2 * BLOCK,
                refs: vec![
                    frame_op("be://data:K_old", ino, 0, true),
                    frame_op("be://data:K_peer", ino, 1, true),
                ],
                lease_epoch: epoch,
                request_id: 0xF1,
            },
        )
        .await
        .expect("the base Put lands");
    assert!(matches!(reply, publish::PublishReply::Unit), "{reply:?}");

    // The holder's custody: block 0 ONLY.
    let _g = client
        .acquire_range(ino, (0, BLOCK), (0, BLOCK), Duration::from_secs(1))
        .await
        .expect("block 0 range custody");

    // The holder's full Put: in-custody {0→K_new} (a real displacement),
    // PLUS a stale out-of-custody claim {1→K_mine} — the scope drops it.
    // Its caller frame carries BOTH transitions and gets block 0's prev
    // WRONG (K_wrong): every one of those errors must die at the
    // composition, not in the ledger.
    let reply = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::SetLayoutAndSize {
                ino,
                layout: base_layout_bytes(
                    2 * BLOCK,
                    &[(0, "be://data:K_new"), (1, "be://data:K_mine")],
                ),
                size: 2 * BLOCK,
                refs: vec![
                    frame_op("be://data:K_wrong", ino, 0, false),
                    frame_op("be://data:K_new", ino, 0, true),
                    frame_op("be://data:K_peer", ino, 1, false),
                    frame_op("be://data:K_mine", ino, 1, true),
                ],
                lease_epoch: epoch,
                request_id: 0xF2,
            },
        )
        .await
        .expect("the scoped Put lands");
    assert!(matches!(reply, publish::PublishReply::Unit), "{reply:?}");

    let map = owner_map(&owner_be, ino).await;
    assert_eq!(
        map.get(&0).map(String::as_str),
        Some("be://data:K_new"),
        "in-custody entry applies"
    );
    assert_eq!(
        map.get(&1).map(String::as_str),
        Some("be://data:K_peer"),
        "out-of-custody entry preserved (the zeros-interleave law)"
    );
    let mut want = vec![
        (kid("be://data:K_new"), ino, 0),
        (kid("be://data:K_peer"), ino, 1),
    ];
    want.sort_unstable();
    assert_eq!(
        ledger(&owner_be).await,
        want,
        "the accounting IS the composition: K_old released (the real \
         displacement), K_peer's record ALIVE (the caller's stale release \
         died with its dropped entry — the map-without-durable half), and \
         K_mine never taken (the durable-without-map half). One staged \
         caller frame here = one swapped pair = the bc row's drift"
    );

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}
