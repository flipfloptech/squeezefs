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
use squeezefs::error::SqueezefsError;
use squeezefs::layout_wire::{LayoutDelta, LayoutMetadata};
use squeezefs::membership::{LeaseClock, LeaseClocks};
use squeezefs::meta_backend::kv::block_refs::{self, BlockRef, BlockRefOp};
use squeezefs::meta_backend::kv::indirect_map::{self, IndirectBlobGuard, IndirectMapIo};
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::meta_backend::{open_routed_meta_set, plan_meta_slot_set, RoutedMetaBackend};
use squeezefs::meta_ship::{self as ship, publish, OwnerMap, PeerOwner};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
        indirect_map::uninstall_indirect_map_io();
        // The 1d section's process-global seams (direct-path A/B lever +
        // the conveyor hold): restored even on an assertion unwind, so a
        // panicking pin can never leak posture into its neighbors.
        squeezefs::routing::set_publish_commit_group_override(None);
        // The 1e section's chain-cap lever (the compaction-arm forcer).
        squeezefs::routing::set_layout_delta_chain_override(None);
        squeezefs::meta_backend::kv::backend::TEST_LAYOUT_MERGE_HOLD_MS.store(0, Ordering::Relaxed);
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

fn opts_sized(node_size: usize) -> squeezefs::meta_backend::kv::builder::FormatV3Options {
    squeezefs::meta_backend::kv::builder::FormatV3Options {
        node_size,
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
    sandbox_sized(
        dir,
        tag,
        stamp,
        squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
    )
    .await
}

/// [`sandbox`] with an explicit node size — the 1e crossing pins format
/// at the 64 KiB floor so the inline ceiling (`xattr_value_cap −
/// LAYOUT_INLINE_HEADROOM` = 16,384 − 4,096) is crossable with a few
/// hundred map entries instead of the default cap's ~2,500 (the
/// `--meta-node-kib 64` format-knob shape).
async fn sandbox_sized(
    dir: &Path,
    tag: &str,
    stamp: bool,
    node_size: usize,
) -> (Arc<RoutedMetaBackend>, PathBuf) {
    let plan = plan_meta_slot_set(1).expect("derived plan");
    let p = make_file(dir, &format!("{tag}-meta0"), VOL_LEN);
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &p,
        VOL_LEN,
        &opts_sized(node_size),
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

/// The 1d pins' in-memory blob store (key → decoded map entries).
type BlobStore = Arc<Mutex<HashMap<String, Vec<(u32, String)>>>>;
/// One `(block index, block key)` entry list (the 1e builders' form).
type Entries = Vec<(u32, String)>;
/// A boxed hook future (the `IndirectMapIo` closure shapes).
type MapIoFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Handles into the in-memory blob store the 1d pins' map hook serves —
/// the oracle for "which blob holds which map" and "which blob keys the
/// commit freed".
struct MapIoHandles {
    blobs: BlobStore,
    freed: Arc<Mutex<Vec<String>>>,
}

/// The synthetic indirect-map I/O hook (the 1d section's data plane):
/// blobs live in a shared in-memory store, `write` mints monotone
/// `be://data:fresh-{n}` keys, `free` records instead of destroying —
/// so every pin reads the compose's device-side lifecycle exactly. The
/// production hook (`multi_writer::arm_multi_writer`) is the router's
/// read/spill/free ladder behind the same seam.
fn install_test_map_io() -> MapIoHandles {
    let blobs: BlobStore = Arc::new(Mutex::new(HashMap::new()));
    let freed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seq = Arc::new(AtomicU64::new(0));

    let b = Arc::clone(&blobs);
    let read = Arc::new(
        move |key: String| -> MapIoFut<Result<Vec<(u32, String)>, SqueezefsError>> {
            let b = Arc::clone(&b);
            Box::pin(async move {
                b.lock().unwrap().get(&key).cloned().ok_or_else(|| {
                    SqueezefsError::InvalidOperation(format!("test map io: no blob '{key}'"))
                })
            })
        },
    );
    let b = Arc::clone(&blobs);
    let s = Arc::clone(&seq);
    let write = Arc::new(
        move |_ino: u64,
              entries: Vec<(u32, String)>|
              -> MapIoFut<Result<(String, IndirectBlobGuard), SqueezefsError>> {
            let b = Arc::clone(&b);
            let s = Arc::clone(&s);
            Box::pin(async move {
                let n = s.fetch_add(1, Ordering::SeqCst) + 1;
                let key = format!("be://data:fresh-{n}");
                b.lock().unwrap().insert(key.clone(), entries);
                Ok((key, IndirectBlobGuard::unguarded()))
            })
        },
    );
    let f = Arc::clone(&freed);
    let free = Arc::new(move |key: String| -> MapIoFut<()> {
        let f = Arc::clone(&f);
        Box::pin(async move {
            f.lock().unwrap().push(key);
        })
    });
    indirect_map::install_indirect_map_io(IndirectMapIo { read, write, free });
    MapIoHandles { blobs, freed }
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
    let owned: Vec<(u32, String)> = map.iter().map(|(b, k)| (*b, k.to_string())).collect();
    base_layout_bytes_owned(size, &owned)
}

fn base_layout_bytes_owned(size: u64, map: &[(u32, String)]) -> Vec<u8> {
    bincode::serialize(&LayoutMetadata {
        file_type: "striped".into(),
        size,
        block_map_id: None,
        block_prefix: Some("be://data".into()),
        file_id: None,
        data_key: None,
        block_map: Some(map.iter().cloned().collect()),
    })
    .expect("serialize base layout")
}

fn delta(size: u64, entries: &[(u32, &str)], versions: (u64, u64)) -> LayoutDelta {
    let owned: Vec<(u32, String)> = entries.iter().map(|(b, k)| (*b, k.to_string())).collect();
    delta_owned(size, &owned, versions)
}

fn delta_owned(size: u64, entries: &[(u32, String)], versions: (u64, u64)) -> LayoutDelta {
    let mut d = LayoutDelta::from_final_state(
        "striped",
        size,
        None,
        Some("be://data"),
        None,
        None,
        entries.to_vec(),
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
// 1c. The indirect head: never stage a delta, never clobber (the MPI-IO
//     row's live conviction — "layout delta base unusable: indirect base")
// ===========================================================================

fn indirect_layout_bytes(size: u64) -> Vec<u8> {
    bincode::serialize(&LayoutMetadata {
        file_type: "striped".into(),
        size,
        block_map_id: Some("indirect:be://data:999".into()),
        block_prefix: Some("be://data".into()),
        file_id: None,
        data_key: None,
        block_map: None,
    })
    .expect("serialize indirect layout")
}

/// Contract (the MPI-IO row's live conviction, this branch: at 10 GiB the
/// head legally goes `indirect:` under a co-writer's spill, and the chain
/// gate's `{`-peek admitted deltas onto it — the fold's own law calls
/// that CORRUPTION, every subsequent lookup/checkpoint of the key refuses
/// forever ("layout delta base unusable: indirect base", 5,000+ failed
/// ticks, fsync terminal-EIO). The law: a CHAINED merge onto an indirect
/// head REFUSES with the retried-class marker — it can neither fold onto
/// the blob (the meta plane cannot read it) nor full-Put the caller's
/// partial map over it (the zeros-interleave clobber) — and stages
/// NOTHING.
///
/// **ARM-DEPENDENT since rung 20 residual 1**: this is the UNARMED shape
/// (no `indirect_map_io` hook installed — every mount without the
/// multi-writer authority). An ARMED owner has a data router, so the
/// same predicate takes the blob-aware compose instead (section 1d).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chained_merge_onto_an_indirect_head_refuses_and_stages_nothing() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (be, _p) = sandbox(dir.path(), "own-indirect", true).await;
    install_test_resolver();
    let vol = &be.volumes[0];
    let ino = {
        use squeezefs::meta_backend::Metadata as _;
        vol.create(1, "big.bin", libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("inode")
            .ino
    };
    vol.set_layout_and_size(ino, &indirect_layout_bytes(16 * BLOCK), 16 * BLOCK, &[])
        .await
        .expect("the indirect head lands");

    let d = delta(
        16 * BLOCK,
        &[(3, "be://data:D3")],
        (0, squeezefs::dlm::mint_layout_version()),
    );
    let err = vol
        .merge_layout_and_size_chained(
            ino,
            ino,
            &d,
            bytes::Bytes::from(base_layout_bytes(16 * BLOCK, &[(3, "be://data:D3")])),
            16 * BLOCK,
            vec![],
        )
        .await
        .expect_err(
            "a chained merge onto an indirect head must REFUSE — staged, the \
             delta poisons every subsequent fold of the key",
        );
    let text = format!("{err}");
    assert!(
        text.contains("indirect base"),
        "the refusal carries the retried-class marker (the writeback \
         ladder's classifier keys on it): {text}"
    );
    // Nothing staged: the head still folds clean to the indirect layout
    // (raw bincode decode — `decode_base_layout` refuses indirect bases
    // by design, which is the very law the gate now enforces).
    use squeezefs::meta_backend::Metadata as _;
    let raw = be
        .getxattr(ino, "layout")
        .await
        .expect("head still readable — nothing corrupted the chain")
        .expect("layout present");
    let l: LayoutMetadata = bincode::deserialize(&raw).expect("still the indirect head");
    assert_eq!(l.block_map_id.as_deref(), Some("indirect:be://data:999"));

    for vol in &be.volumes {
        vol.shutdown().await.expect("shutdown");
    }
}

/// Contract (the same face's Put half): a RANGE holder's full Put onto an
/// INDIRECT durable head must REFUSE — the scoped compose cannot read
/// either blob at the meta plane, and the retired verbatim arm replaces
/// the whole-file map with the shipper's partial view (the
/// zeros-interleave clobber at 10 GiB scale). "Scoped or not at all",
/// rung 18's own law, extended to the base the scope cannot decode.
///
/// **ARM-DEPENDENT since rung 20 residual 1**: this is the UNARMED shape
/// (no `indirect_map_io` hook installed). An ARMED owner rehydrates the
/// blob and composes the scoped Put over the full map (section 1d).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_range_holders_put_onto_an_indirect_head_refuses_not_clobbers() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own-indput", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli-indput", false).await;
    let auth = start_authority(Arc::clone(&owner_be), "widthn-authority");
    let (client, pc) = arm_client(&auth, &client_be).await;
    install_test_resolver();
    let epoch = client.lease_epoch();
    let ino = shipped_create(&client_be, "indput.bin").await;

    // The indirect durable head (a peer's spill).
    let reply = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::SetLayoutAndSize {
                ino,
                layout: indirect_layout_bytes(16 * BLOCK),
                size: 16 * BLOCK,
                refs: vec![],
                lease_epoch: epoch,
                request_id: 0xA1,
            },
        )
        .await
        .expect("the indirect head lands (pre-custody verbatim)");
    assert!(matches!(reply, publish::PublishReply::Unit), "{reply:?}");

    // The holder's custody + its partial-view full Put.
    let _g = client
        .acquire_range(
            ino,
            (0, BLOCK),
            (0, BLOCK),
            std::time::Duration::from_secs(1),
        )
        .await
        .expect("block 0 range custody");
    let out = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::SetLayoutAndSize {
                ino,
                layout: base_layout_bytes(16 * BLOCK, &[(0, "be://data:K0")]),
                size: 16 * BLOCK,
                refs: vec![frame_op("be://data:K0", ino, 0, true)],
                lease_epoch: epoch,
                request_id: 0xA2,
            },
        )
        .await;
    match out {
        Err(e) => {
            let text = format!("{e}");
            assert!(
                text.contains("indirect"),
                "the refusal names the indirect head: {text}"
            );
        }
        Ok(reply) => panic!(
            "a range holder's Put onto an indirect head applied ({reply:?}) — \
             the verbatim arm just replaced a whole-file map with one block"
        ),
    }
    // The head survives untouched (raw decode — see the gate pin's note).
    use squeezefs::meta_backend::Metadata as _;
    let raw = owner_be
        .getxattr(ino, "layout")
        .await
        .expect("head readable")
        .expect("layout present");
    let l: LayoutMetadata = bincode::deserialize(&raw).expect("still the indirect head");
    assert_eq!(l.block_map_id.as_deref(), Some("indirect:be://data:999"));

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

// ===========================================================================
// 1d. The blob-aware owner-side compose (rung 20 residual 1)
// ===========================================================================
//
// The 1c refusals above are the UNARMED shape. When the multi-writer
// authority is armed it HAS a data router — `multi_writer::arm_multi_writer`
// installs the `indirect_map_io` hook beside the block-ref resolver — so
// the three owner-side sites (the aggregated conveyor member, the direct
// merge, the custody-scoped Put) REHYDRATE the blob, compose onto the FULL
// map, recompute the durable accounting against the full head, write a
// fresh CoW blob (DUR-6: flushed before the naming commit), and stage a
// full Put naming it. The refusal that never converged (retried-class
// fsync EIO forever at 10 GiB scale) becomes a composition.

/// Plant `indirect:be://data:999` as `name`'s durable head: `map` stored
/// at the blob key in the test hook's store, every reference (entries +
/// the blob's own MAP_BLOB record) staged — so the ledger oracle nets
/// exactly against the compose's recomputed accounting.
async fn plant_indirect_head(
    be: &Arc<RoutedMetaBackend>,
    io: &MapIoHandles,
    name: &str,
    map: &[(u32, &str)],
    size: u64,
) -> u64 {
    io.blobs.lock().unwrap().insert(
        "be://data:999".to_string(),
        map.iter().map(|(b, k)| (*b, k.to_string())).collect(),
    );
    let vol = &be.volumes[0];
    let ino = {
        use squeezefs::meta_backend::Metadata as _;
        vol.create(1, name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("inode")
            .ino
    };
    let mut refs: Vec<BlockRefOp> = map
        .iter()
        .map(|(b, k)| {
            BlockRefOp::taken(BlockRef {
                vol_tag: TEST_TAG,
                block_idx: kid(k),
                owner_ino: ino,
                block_index: *b,
            })
        })
        .collect();
    refs.push(BlockRefOp::taken(BlockRef {
        vol_tag: TEST_TAG,
        block_idx: kid("be://data:999"),
        owner_ino: ino,
        block_index: block_refs::BLOCK_INDEX_MAP_BLOB,
    }));
    vol.set_layout_and_size(ino, &indirect_layout_bytes(size), size, &refs)
        .await
        .expect("the indirect head lands");
    ino
}

/// The durable head, RAW (bincode — `decode_base_layout` refuses indirect
/// heads by design, so the indirect pins read past the fold-layer law).
async fn raw_head(be: &Arc<RoutedMetaBackend>, ino: u64) -> LayoutMetadata {
    use squeezefs::meta_backend::Metadata as _;
    let raw = be
        .getxattr(ino, "layout")
        .await
        .expect("layout read")
        .expect("layout present");
    bincode::deserialize(&raw).expect("bincode layout head")
}

/// Contract (rung 20 residual 1, the direct-path arm — site (b)): with
/// the hook armed, a chained merge onto an indirect head COMPOSES —
/// rehydrate the blob, apply the delta onto the FULL map, write a fresh
/// CoW blob, stage a full Put naming it, recompute the accounting against
/// the full head (entry displacement + the blob custody transfer), and
/// free the displaced blob only AFTER the commit stopped naming it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_armed_chained_merge_composes_onto_the_indirect_head() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (be, _p) = sandbox(dir.path(), "own-compose-b", true).await;
    install_test_resolver();
    let io = install_test_map_io();
    // Site (b): the direct path (`SQUEEZEFS_PUBLISH_COMMIT_GROUP_MAX=1`
    // A/B lever's programmatic form; Restore resets it).
    squeezefs::routing::set_publish_commit_group_override(Some(1));

    let planted: Vec<(u32, String)> = (0..8u32).map(|i| (i, format!("be://data:P{i}"))).collect();
    let planted_refs: Vec<(u32, &str)> = planted.iter().map(|(b, k)| (*b, k.as_str())).collect();
    let ino = plant_indirect_head(&be, &io, "compose.bin", &planted_refs, 16 * BLOCK).await;
    let composes0 = squeezefs::fuse_client::METRICS
        .publish_blob_composes
        .load(Ordering::Relaxed);

    let vol = &be.volumes[0];
    let d = delta(
        16 * BLOCK,
        &[(3, "be://data:N3")],
        (0, squeezefs::dlm::mint_layout_version()),
    );
    let (used, version) = vol
        .merge_layout_and_size_chained(
            ino,
            ino,
            &d,
            bytes::Bytes::from(base_layout_bytes(16 * BLOCK, &[(3, "be://data:N3")])),
            16 * BLOCK,
            // The caller's (blind) frame — replaced by the recompute.
            vec![BlockRefOp::taken(BlockRef {
                vol_tag: TEST_TAG,
                block_idx: kid("be://data:N3"),
                owner_ino: ino,
                block_index: 3,
            })],
        )
        .await
        .expect("the armed compose lands instead of refusing");
    assert!(!used, "a compose stages a full Put, never a delta");
    assert_eq!(version, 0, "a full Put carries no staged link version");

    // The head names the fresh CoW blob.
    let head = raw_head(&be, ino).await;
    assert_eq!(
        head.block_map_id.as_deref(),
        Some("indirect:be://data:fresh-1"),
        "the composed head names the FRESH blob (CoW — never the \
         predecessor rewritten in place)"
    );
    assert!(
        head.block_map.is_none(),
        "an indirect head carries no inline map"
    );

    // The fresh blob holds the FULL composed map: planted with block 3
    // replaced.
    let mut want_map = planted.clone();
    want_map[3].1 = "be://data:N3".to_string();
    let got = io
        .blobs
        .lock()
        .unwrap()
        .get("be://data:fresh-1")
        .cloned()
        .expect("the fresh blob was written");
    assert_eq!(got, want_map, "the blob is the FULL composed map");

    // The accounting is the full-head transition + the blob custody
    // transfer: P3 released, N3 taken, old blob released, fresh taken.
    let mut want: Vec<(u64, u64, u32)> = planted
        .iter()
        .filter(|(b, _)| *b != 3)
        .map(|(b, k)| (kid(k), ino, *b))
        .collect();
    want.push((kid("be://data:N3"), ino, 3));
    want.push((
        kid("be://data:fresh-1"),
        ino,
        block_refs::BLOCK_INDEX_MAP_BLOB,
    ));
    want.sort_unstable();
    assert_eq!(
        ledger(&be).await,
        want,
        "the recomputed accounting: the full head's displaced binding \
         released, the entry taken, and the blob custody transferred — \
         all in the ONE naming commit"
    );

    // The displaced blob is freed AFTER the commit (never before).
    assert_eq!(
        io.freed.lock().unwrap().clone(),
        vec!["be://data:999".to_string()],
        "exactly the displaced predecessor blob freed"
    );
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .publish_blob_composes
            .load(Ordering::Relaxed)
            - composes0,
        1,
        "the engagement gauge counts the compose"
    );

    for vol in &be.volumes {
        vol.shutdown().await.expect("shutdown");
    }
}

/// Contract (rung 20 residual 1, the scoped-Put arm — site (c), durable
/// side): a range holder's full Put onto an INDIRECT durable head
/// composes with the hook armed — the blob rehydrates, the scoped
/// retain/insert runs over the FULL map, and a small composed result
/// COLLAPSES BACK INLINE (the durable old blob released + freed). The
/// in-custody entry lands, the out-of-custody entry dies at the scope,
/// and the planted out-of-custody binding survives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_armed_scoped_put_composes_over_the_indirect_durable_head() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own-scomp", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli-scomp", false).await;
    let auth = start_authority(Arc::clone(&owner_be), "widthn-authority");
    let (client, pc) = arm_client(&auth, &client_be).await;
    install_test_resolver();
    let io = install_test_map_io();
    let epoch = client.lease_epoch();
    let ino = shipped_create(&client_be, "scomp.bin").await;

    // The indirect durable head over a SMALL planted map (so the
    // composed result exercises the collapse-inline arm).
    io.blobs.lock().unwrap().insert(
        "be://data:999".to_string(),
        vec![
            (0, "be://data:K_old".to_string()),
            (5, "be://data:K_peer".to_string()),
        ],
    );
    let reply = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::SetLayoutAndSize {
                ino,
                layout: indirect_layout_bytes(16 * BLOCK),
                size: 16 * BLOCK,
                refs: vec![
                    frame_op("be://data:K_old", ino, 0, true),
                    frame_op("be://data:K_peer", ino, 5, true),
                    frame_op("be://data:999", ino, block_refs::BLOCK_INDEX_MAP_BLOB, true),
                ],
                lease_epoch: epoch,
                request_id: 0xB1,
            },
        )
        .await
        .expect("the indirect head lands (pre-custody verbatim)");
    assert!(matches!(reply, publish::PublishReply::Unit), "{reply:?}");

    // The holder's custody: block 0 ONLY. Its Put carries an in-custody
    // displacement (0→K_new) and a stale out-of-custody claim (5→K_mine).
    let _g = client
        .acquire_range(ino, (0, BLOCK), (0, BLOCK), Duration::from_secs(1))
        .await
        .expect("block 0 range custody");
    let reply = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::SetLayoutAndSize {
                ino,
                layout: base_layout_bytes(
                    16 * BLOCK,
                    &[(0, "be://data:K_new"), (5, "be://data:K_mine")],
                ),
                size: 16 * BLOCK,
                refs: vec![
                    frame_op("be://data:K_new", ino, 0, true),
                    frame_op("be://data:K_mine", ino, 5, true),
                ],
                lease_epoch: epoch,
                request_id: 0xB2,
            },
        )
        .await
        .expect("the armed scoped Put composes instead of refusing");
    assert!(matches!(reply, publish::PublishReply::Unit), "{reply:?}");

    // The composed head is INLINE again (small map — the collapse arm):
    // the fold layer's own decoder accepts it.
    let map = owner_map(&owner_be, ino).await;
    assert_eq!(
        map.get(&0).map(String::as_str),
        Some("be://data:K_new"),
        "the in-custody entry landed"
    );
    assert_eq!(
        map.get(&5).map(String::as_str),
        Some("be://data:K_peer"),
        "the out-of-custody entry died at the scope; the planted binding \
         survives (the zeros-interleave law over a rehydrated map)"
    );
    let head = raw_head(&owner_be, ino).await;
    assert_eq!(
        head.block_map_id, None,
        "the composed inline head no longer names any blob"
    );

    // Accounting: K_old released, K_new taken, K_peer alive, K_mine never
    // taken — and the durable blob's MAP_BLOB record RELEASED with the
    // collapse.
    let mut want = vec![
        (kid("be://data:K_new"), ino, 0),
        (kid("be://data:K_peer"), ino, 5),
    ];
    want.sort_unstable();
    assert_eq!(
        ledger(&owner_be).await,
        want,
        "the composition's accounting over the FULL rehydrated map — \
         including the released blob record"
    );
    assert_eq!(
        io.freed.lock().unwrap().clone(),
        vec!["be://data:999".to_string()],
        "the durable blob freed after the collapse-inline commit"
    );

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

/// Contract (rung 20 residual 1, the scoped-Put arm — site (c), shipped
/// side): a range holder's Put whose SHIPPED layout spilled indirect is
/// rehydrated from ITS blob and scoped like any inline Put — and the
/// caller's map-blob frame ops are DROPPED (the owner recomputes blob
/// custody entirely; the shipped blob's device block stays the shipper's
/// own lifecycle, never freed here).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_armed_scoped_put_reads_the_shipped_side_blob() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own-sship", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli-sship", false).await;
    let auth = start_authority(Arc::clone(&owner_be), "widthn-authority");
    let (client, pc) = arm_client(&auth, &client_be).await;
    install_test_resolver();
    let io = install_test_map_io();
    let epoch = client.lease_epoch();
    let ino = shipped_create(&client_be, "sship.bin").await;

    // The durable head is INLINE: {0→K_old, 5→K_peer}.
    let reply = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::SetLayoutAndSize {
                ino,
                layout: base_layout_bytes(
                    2 * BLOCK,
                    &[(0, "be://data:K_old"), (5, "be://data:K_peer")],
                ),
                size: 2 * BLOCK,
                refs: vec![
                    frame_op("be://data:K_old", ino, 0, true),
                    frame_op("be://data:K_peer", ino, 5, true),
                ],
                lease_epoch: epoch,
                request_id: 0xB3,
            },
        )
        .await
        .expect("the inline base Put lands");
    assert!(matches!(reply, publish::PublishReply::Unit), "{reply:?}");

    // The holder's custody: block 0. Its Put SHIPPED as an indirect
    // layout whose blob carries an in-custody entry (0→K_new) and an
    // out-of-custody one (5→K_mine) — plus the caller's own map-blob
    // frame op, which the compose arm must DROP.
    let _g = client
        .acquire_range(ino, (0, BLOCK), (0, BLOCK), Duration::from_secs(1))
        .await
        .expect("block 0 range custody");
    io.blobs.lock().unwrap().insert(
        "be://data:999".to_string(),
        vec![
            (0, "be://data:K_new".to_string()),
            (5, "be://data:K_mine".to_string()),
        ],
    );
    let reply = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::SetLayoutAndSize {
                ino,
                layout: indirect_layout_bytes(2 * BLOCK),
                size: 2 * BLOCK,
                refs: vec![
                    frame_op("be://data:K_new", ino, 0, true),
                    frame_op("be://data:K_mine", ino, 5, true),
                    frame_op("be://data:999", ino, block_refs::BLOCK_INDEX_MAP_BLOB, true),
                ],
                lease_epoch: epoch,
                request_id: 0xB4,
            },
        )
        .await
        .expect("the armed scoped Put reads the shipped blob and composes");
    assert!(matches!(reply, publish::PublishReply::Unit), "{reply:?}");

    let map = owner_map(&owner_be, ino).await;
    assert_eq!(
        map.get(&0).map(String::as_str),
        Some("be://data:K_new"),
        "the in-custody shipped entry landed"
    );
    assert_eq!(
        map.get(&5).map(String::as_str),
        Some("be://data:K_peer"),
        "the out-of-custody shipped entry died at the scope"
    );

    // The ledger holds NO record for the shipped blob: the caller's
    // MAP_BLOB frame op was dropped (the owner recomputes blob custody).
    let mut want = vec![
        (kid("be://data:K_new"), ino, 0),
        (kid("be://data:K_peer"), ino, 5),
    ];
    want.sort_unstable();
    assert_eq!(
        ledger(&owner_be).await,
        want,
        "no MAP_BLOB record for the shipped blob — the caller's blob \
         frame op is DROPPED on the compose arm"
    );
    // The shipped blob's device block is the SHIPPER's lifecycle — the
    // owner never frees it (bounded one-blob residue, reclaimed by the
    // shipper's own old_indirect_to_free tail or remount derivation).
    assert!(
        io.freed.lock().unwrap().is_empty(),
        "the owner freed nothing: durable side was inline, the shipped \
         blob belongs to the shipper"
    );

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

/// Contract (rung 20 residual 1, the aggregated conveyor — site (a), the
/// batch-prior law): two same-ino members of ONE pass compose onto the
/// ACCUMULATED view (the pass-local memo), never onto a re-read of the
/// committed blob — re-reading would erase the pass mate's just-staged
/// entries while their ledger refs land (the exact "1 durable vs 0
/// layout references" mint). One old-blob release, the intermediate
/// fresh blob released-and-freed, the final head naming the LAST fresh
/// key.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn armed_batch_mates_compose_onto_the_accumulated_view() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (be, _p) = sandbox(dir.path(), "own-batch", true).await;
    install_test_resolver();
    let io = install_test_map_io();

    let planted: Vec<(u32, String)> = (0..8u32).map(|i| (i, format!("be://data:P{i}"))).collect();
    let planted_refs: Vec<(u32, &str)> = planted.iter().map(|(b, k)| (*b, k.as_str())).collect();
    let ino = plant_indirect_head(&be, &io, "batch.bin", &planted_refs, 16 * BLOCK).await;

    // The conveyor hold seam accumulates both merges into ONE pass
    // (the mw_authority_assembler_tests precedent; Restore resets it).
    let vol = &be.volumes[0];
    let d2 = delta(
        16 * BLOCK,
        &[(2, "be://data:M2")],
        (0, squeezefs::dlm::mint_layout_version()),
    );
    let d4 = delta(
        16 * BLOCK,
        &[(4, "be://data:M4")],
        (0, squeezefs::dlm::mint_layout_version()),
    );
    squeezefs::meta_backend::kv::backend::TEST_LAYOUT_MERGE_HOLD_MS.store(150, Ordering::Relaxed);
    let (r1, r2) = tokio::join!(
        vol.merge_layout_and_size_chained(
            ino,
            ino,
            &d2,
            bytes::Bytes::from(base_layout_bytes(16 * BLOCK, &[(2, "be://data:M2")])),
            16 * BLOCK,
            vec![],
        ),
        vol.merge_layout_and_size_chained(
            ino,
            ino,
            &d4,
            bytes::Bytes::from(base_layout_bytes(16 * BLOCK, &[(4, "be://data:M4")])),
            16 * BLOCK,
            vec![],
        ),
    );
    squeezefs::meta_backend::kv::backend::TEST_LAYOUT_MERGE_HOLD_MS.store(0, Ordering::Relaxed);
    r1.expect("pass mate 1 composes");
    r2.expect("pass mate 2 composes");

    // The final head names the LAST fresh key (the accumulated view's
    // final blob), and its map carries BOTH mates' entries.
    let head = raw_head(&be, ino).await;
    assert_eq!(
        head.block_map_id.as_deref(),
        Some("indirect:be://data:fresh-2"),
        "the final head names the SECOND fresh blob (both mates in one \
         pass — the accumulated view)"
    );
    let mut want_map = planted.clone();
    want_map[2].1 = "be://data:M2".to_string();
    want_map[4].1 = "be://data:M4".to_string();
    let got = io
        .blobs
        .lock()
        .unwrap()
        .get("be://data:fresh-2")
        .cloned()
        .expect("the final blob was written");
    assert_eq!(
        got, want_map,
        "BOTH mates' entries in the final blob — the second composed onto \
         the accumulated view, never a re-read of the committed blob"
    );

    // Ledger net: planted − displaced + both entries + the FINAL blob
    // take. The old blob released once; the intermediate fresh blob's
    // take+release nets to absent.
    let mut want: Vec<(u64, u64, u32)> = planted
        .iter()
        .filter(|(b, _)| *b != 2 && *b != 4)
        .map(|(b, k)| (kid(k), ino, *b))
        .collect();
    want.push((kid("be://data:M2"), ino, 2));
    want.push((kid("be://data:M4"), ino, 4));
    want.push((
        kid("be://data:fresh-2"),
        ino,
        block_refs::BLOCK_INDEX_MAP_BLOB,
    ));
    want.sort_unstable();
    assert_eq!(
        ledger(&be).await,
        want,
        "one old-blob release, the intermediate fresh blob's take+release \
         netted out, the final blob taken"
    );

    // Freed: the old blob AND the intermediate fresh blob (superseded
    // within the pass) — never the final one.
    let mut freed = io.freed.lock().unwrap().clone();
    freed.sort_unstable();
    assert_eq!(
        freed,
        vec!["be://data:999".to_string(), "be://data:fresh-1".to_string()],
        "the displaced predecessor and the superseded intermediate blob \
         both freed after the ONE commit"
    );

    for vol in &be.volumes {
        vol.shutdown().await.expect("shutdown");
    }
}

/// Contract (the arm-dependence boundary): with NO map hook installed
/// (the resolver alone does not arm the compose), the ORIGINAL refusal
/// fires verbatim on the direct path — unarmed mounts keep the
/// fail-safe, retried-class shape the 1c pins encode.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unarmed_chained_merge_still_refuses_the_indirect_head() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (be, _p) = sandbox(dir.path(), "own-unarmed", true).await;
    install_test_resolver(); // resolver armed — the HOOK is what gates
    squeezefs::routing::set_publish_commit_group_override(Some(1)); // site (b)

    let vol = &be.volumes[0];
    let ino = {
        use squeezefs::meta_backend::Metadata as _;
        vol.create(1, "unarmed.bin", libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("inode")
            .ino
    };
    vol.set_layout_and_size(ino, &indirect_layout_bytes(16 * BLOCK), 16 * BLOCK, &[])
        .await
        .expect("the indirect head lands");

    let d = delta(
        16 * BLOCK,
        &[(3, "be://data:D3")],
        (0, squeezefs::dlm::mint_layout_version()),
    );
    let err = vol
        .merge_layout_and_size_chained(
            ino,
            ino,
            &d,
            bytes::Bytes::from(base_layout_bytes(16 * BLOCK, &[(3, "be://data:D3")])),
            16 * BLOCK,
            vec![],
        )
        .await
        .expect_err("unarmed mounts keep the fail-safe refusal verbatim");
    let text = format!("{err}");
    assert!(
        text.contains("indirect base"),
        "the retried-class marker survives on the unarmed arm: {text}"
    );
    let head = raw_head(&be, ino).await;
    assert_eq!(
        head.block_map_id.as_deref(),
        Some("indirect:be://data:999"),
        "nothing staged, nothing composed"
    );

    for vol in &be.volumes {
        vol.shutdown().await.expect("shutdown");
    }
}

// ===========================================================================
// 1e. The inline→oversize CROSSING (the 2026-08-19 AlreadyFreezing wedge's
//     trigger — the owner-side chain compaction past the record cap)
// ===========================================================================
//
// The rung-20 compose arms above handle a head that is ALREADY indirect.
// What the 2026-08-19 field wedge convicted is the crossing itself: the
// owner-side chain compaction (`chained_full = delta.apply(folded)`)
// composed a 10 GiB shared file's map onto an INLINE head with NO cap
// check — the oversize record passed admission, reached the node
// overlay, and could only be refused at FREEZE time ("record value
// length 66121 exceeds the per-volume cap 65792"), wedging the volume's
// checkpoint forever. The law: the compaction arm makes the SAME spill
// decision the router's save path and the custody-scoped Put make
// (site (c)'s arithmetic — `encoded > xattr_value_cap −
// LAYOUT_INLINE_HEADROOM`): armed it composes to a fresh indirect blob;
// unarmed it refuses retried-class and stages NOTHING. These pins run on
// 64 KiB nodes (inline ceiling 12,288 B) so a few hundred entries cross.

/// The 1e fixture: a 64 KiB-node armed sandbox with a `base_n`-entry
/// inline durable head committed (every base take staged), chain-cap
/// compaction forced (`set_layout_delta_chain_override(Some(0))` — every
/// chained merge takes the owner-side compaction arm, the field's
/// at-the-cap shape without 64 filler deltas).
async fn crossing_sandbox(
    dir: &Path,
    tag: &str,
    base_n: u32,
) -> (Arc<RoutedMetaBackend>, u64, Entries) {
    let (be, _p) = sandbox_sized(dir, tag, true, 64 * 1024).await;
    install_test_resolver();
    squeezefs::routing::set_layout_delta_chain_override(Some(0));
    let vol = &be.volumes[0];
    let ino = {
        use squeezefs::meta_backend::Metadata as _;
        vol.create(1, "crossing.bin", libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("inode")
            .ino
    };
    let base: Vec<(u32, String)> = (0..base_n)
        .map(|i| (i, format!("be://data:K{i:05}")))
        .collect();
    let refs: Vec<BlockRefOp> = base
        .iter()
        .map(|(b, k)| {
            BlockRefOp::taken(BlockRef {
                vol_tag: TEST_TAG,
                block_idx: kid(k),
                owner_ino: ino,
                block_index: *b,
            })
        })
        .collect();
    let size = u64::from(base_n) * BLOCK;
    vol.set_layout_and_size(ino, &base_layout_bytes_owned(size, &base), size, &refs)
        .await
        .expect("the inline base head lands");
    (be, ino, base)
}

/// The delta that drives the composed map past the inline ceiling, plus
/// the crossing precondition proof: base fits inline, composed does not.
fn crossing_delta(
    vol: &Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend>,
    base: &[(u32, String)],
    add_n: u32,
) -> (Entries, Entries, u64) {
    let base_n = base.len() as u32;
    let added: Vec<(u32, String)> = (base_n..base_n + add_n)
        .map(|i| (i, format!("be://data:N{i:05}")))
        .collect();
    let mut composed = base.to_vec();
    composed.extend(added.iter().cloned());
    let size = u64::from(base_n + add_n) * BLOCK;
    let ceiling = vol
        .xattr_value_cap()
        .saturating_sub(squeezefs::routing::LAYOUT_INLINE_HEADROOM);
    assert!(
        base_layout_bytes_owned(u64::from(base_n) * BLOCK, base).len() <= ceiling,
        "precondition: the BASE head fits inline"
    );
    assert!(
        base_layout_bytes_owned(size, &composed).len() > ceiling,
        "precondition: the COMPOSED map crosses the inline ceiling ({ceiling} B)"
    );
    (added, composed, size)
}

/// The expected post-crossing ledger: every base take, every added take
/// (nothing displaced — the delta only extends the map), and the fresh
/// blob's MAP_BLOB take. NO release op anywhere: the head was INLINE, so
/// the compose displaces no blob.
fn crossing_ledger_want(
    ino: u64,
    base: &[(u32, String)],
    added: &[(u32, String)],
    fresh: &str,
) -> Vec<(u64, u64, u32)> {
    let mut want: Vec<(u64, u64, u32)> = base
        .iter()
        .chain(added.iter())
        .map(|(b, k)| (kid(k), ino, *b))
        .collect();
    want.push((kid(fresh), ino, block_refs::BLOCK_INDEX_MAP_BLOB));
    want.sort_unstable();
    want
}

/// Contract (the trigger, site (b) — the direct chained merge): an ARMED
/// owner whose chain compaction composes past the inline ceiling SPILLS
/// — fresh CoW blob, full Put naming `indirect:{key}`, the MAP_BLOB take
/// staged in the same commit, NO released op (the head was inline), and
/// the engagement gauge counts the compose. The oversize inline record
/// is never staged, so the freeze-time `ValueTooLarge` (the 2026-08-19
/// checkpoint wedge) is unreachable from this arm.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_armed_chained_merge_crossing_the_inline_cap_composes_to_indirect() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let io = install_test_map_io();
    let (be, ino, base) = crossing_sandbox(dir.path(), "own-cross-b", 300).await;
    // Site (b): the direct path.
    squeezefs::routing::set_publish_commit_group_override(Some(1));
    let vol = &be.volumes[0];
    let (added, composed, size) = crossing_delta(vol, &base, 250);
    let composes0 = squeezefs::fuse_client::METRICS
        .publish_blob_composes
        .load(Ordering::Relaxed);

    let d = delta_owned(size, &added, (0, squeezefs::dlm::mint_layout_version()));
    let caller_refs: Vec<BlockRefOp> = added
        .iter()
        .map(|(b, k)| {
            BlockRefOp::taken(BlockRef {
                vol_tag: TEST_TAG,
                block_idx: kid(k),
                owner_ino: ino,
                block_index: *b,
            })
        })
        .collect();
    let (used, version) = vol
        .merge_layout_and_size_chained(
            ino,
            ino,
            &d,
            bytes::Bytes::from(base_layout_bytes_owned(size, &added)),
            size,
            caller_refs,
        )
        .await
        .expect("the armed crossing compaction composes instead of staging oversize");
    assert!(!used, "a compose stages a full Put, never a delta");
    assert_eq!(version, 0, "a full Put carries no staged link version");

    // The durable head names the fresh CoW blob; no inline map.
    let head = raw_head(&be, ino).await;
    assert_eq!(
        head.block_map_id.as_deref(),
        Some("indirect:be://data:fresh-1"),
        "the composed head crossed to INDIRECT — the oversize inline \
         record (the checkpoint-wedge mint) was never staged"
    );
    assert!(
        head.block_map.is_none(),
        "no inline map on an indirect head"
    );

    // The fresh blob holds the FULL composed map.
    let mut want_map = composed.clone();
    want_map.sort_unstable_by_key(|&(b, _)| b);
    assert_eq!(
        io.blobs
            .lock()
            .unwrap()
            .get("be://data:fresh-1")
            .cloned()
            .expect("the fresh blob was written"),
        want_map,
        "the blob is the FULL composed map (base + delta entries)"
    );

    // Accounting: base takes intact, delta entries taken, the fresh
    // blob's MAP_BLOB take — and NO released op (the head was inline).
    assert_eq!(
        ledger(&be).await,
        crossing_ledger_want(ino, &base, &added, "be://data:fresh-1"),
        "delta entries present + MAP_BLOB take, no released op"
    );
    assert!(
        io.freed.lock().unwrap().is_empty(),
        "nothing freed: an inline head displaces no blob"
    );
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .publish_blob_composes
            .load(Ordering::Relaxed)
            - composes0,
        1,
        "the engagement gauge counts the crossing compose"
    );

    for vol in &be.volumes {
        vol.shutdown().await.expect("shutdown");
    }
}

/// Contract (finding 21 — PR 5 acceptance attempt 2's A1 killer,
/// `.benchmarks/2026-08-25-s11-freeloop-stall.md`): a range holder's
/// SCOPED PUT over an INLINE head whose COMPOSED map crosses the value
/// cap SPILLS to a fresh CoW blob exactly like the rehydrated and
/// chained-merge arms — never the verbatim inline encode whose Put the
/// KV refuses ("record value length 66,109 exceeds the per-volume cap
/// 65,792"). Pre-fix the non-rehydrated compose tail had NO spill arm:
/// both inputs individually respected the cap, the COMPOSITION did not,
/// and the ack-early publish's never-lossy retry recomposed the same
/// over-cap value forever — a permanent fsync failure (errno 7 latched,
/// ior abort at 32-rank block-cyclic scale, where every co-writer's
/// scoped Put composes with 31 peers' entries).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_armed_scoped_put_crossing_the_inline_cap_composes_to_indirect() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox_sized(dir.path(), "own-scross", true, 64 * 1024).await;
    let (client_be, _p2) = sandbox_sized(dir.path(), "cli-scross", false, 64 * 1024).await;
    let auth = start_authority(Arc::clone(&owner_be), "widthn-authority-scross");
    let (client, pc) = arm_client(&auth, &client_be).await;
    install_test_resolver();
    let io = install_test_map_io();
    let epoch = client.lease_epoch();
    let ino = shipped_create(&client_be, "scross.bin").await;

    // Size the base so IT respects the cap while base + the holder's one
    // long-keyed entry crosses it — the composition-over-cap shape (every
    // input under cap, the composed result over it). Derived from the
    // volume's own cap, never a constant. The base stays INSIDE the
    // authority's 16-block geometry (blocks 1..=15 with growing key
    // lengths), so the scope preserves every peer entry.
    let kv_cap = owner_be.volumes[0].xattr_value_cap();
    let new_key = format!("be://data:{}", "N".repeat(512));
    let size = 16 * BLOCK;
    let base_at = |key_len: usize| -> Vec<(u32, String)> {
        (1..=15u32)
            .map(|b| (b, format!("be://data:{b:02}{}", "K".repeat(key_len))))
            .collect()
    };
    let mut key_len = 16;
    let base = loop {
        let base = base_at(key_len);
        let mut composed = base.clone();
        composed.push((0, new_key.clone()));
        if base_layout_bytes_owned(size, &composed).len() > kv_cap {
            break base;
        }
        // 15 entries × 16 B/step = 240 B per iteration — strictly less
        // than the holder entry's ~530 B, so at the first crossing the
        // BASE is still under the cap by construction (asserted below).
        key_len += 16;
    };
    assert!(
        base_layout_bytes_owned(size, &base).len() <= kv_cap,
        "precondition: the base head respects the cap"
    );
    {
        let mut composed = base.clone();
        composed.push((0, new_key.clone()));
        assert!(
            base_layout_bytes_owned(size, &composed).len() > kv_cap,
            "precondition: the COMPOSED map crosses the cap ({kv_cap} B)"
        );
    }

    // The pre-custody verbatim plant: the base head + its takes.
    let base_refs: Vec<publish::WireBlockRefOp> = base
        .iter()
        .map(|(b, k)| frame_op(k, ino, *b, true))
        .collect();
    let reply = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::SetLayoutAndSize {
                ino,
                layout: base_layout_bytes_owned(size, &base),
                size,
                refs: base_refs,
                lease_epoch: epoch,
                request_id: 0xC1,
            },
        )
        .await
        .expect("the inline base head lands (pre-custody verbatim)");
    assert!(matches!(reply, publish::PublishReply::Unit), "{reply:?}");

    // The holder's custody: block 0 only. Its scoped Put composes with
    // every peer entry the head holds — past the cap.
    let _g = client
        .acquire_range(ino, (0, BLOCK), (0, BLOCK), Duration::from_secs(1))
        .await
        .expect("block 0 range custody");
    let composes0 = squeezefs::fuse_client::METRICS
        .publish_blob_composes
        .load(Ordering::Relaxed);
    let reply = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::SetLayoutAndSize {
                ino,
                layout: base_layout_bytes(size, &[(0, new_key.as_str())]),
                size,
                refs: vec![frame_op(&new_key, ino, 0, true)],
                lease_epoch: epoch,
                request_id: 0xC2,
            },
        )
        .await
        .expect(
            "finding 21: the scoped Put whose COMPOSITION crosses the cap must SPILL to a \
             fresh CoW blob — the verbatim inline encode is the permanent-fsync-failure \
             class (the KV refuses it, the retry recomposes it, for ever)",
        );
    assert!(matches!(reply, publish::PublishReply::Unit), "{reply:?}");

    // The durable head names the fresh blob; the blob holds the FULL
    // composed map (the holder's entry + every peer entry).
    let head = raw_head(&owner_be, ino).await;
    assert_eq!(
        head.block_map_id.as_deref(),
        Some("indirect:be://data:fresh-1"),
        "the composed head crossed to INDIRECT"
    );
    assert!(
        head.block_map.is_none(),
        "no inline map on an indirect head"
    );
    let mut want_map: Vec<(u32, String)> = base.clone();
    want_map.push((0, new_key.clone()));
    want_map.sort_unstable_by_key(|&(b, _)| b);
    assert_eq!(
        io.blobs
            .lock()
            .unwrap()
            .get("be://data:fresh-1")
            .cloned()
            .expect("the fresh blob was written"),
        want_map,
        "the blob is the FULL composed map"
    );

    // Accounting: every base take intact, the holder's take, the fresh
    // blob's MAP_BLOB take — and nothing freed (the head was inline).
    let mut want: Vec<(u64, u64, u32)> = base.iter().map(|(b, k)| (kid(k), ino, *b)).collect();
    want.push((kid(&new_key), ino, 0));
    want.push((
        kid("be://data:fresh-1"),
        ino,
        block_refs::BLOCK_INDEX_MAP_BLOB,
    ));
    want.sort_unstable();
    assert_eq!(
        ledger(&owner_be).await,
        want,
        "base takes + the holder's take + the MAP_BLOB take"
    );
    assert!(
        io.freed.lock().unwrap().is_empty(),
        "nothing freed: an inline head displaces no blob"
    );
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .publish_compose_spills
            .load(Ordering::Relaxed),
        1,
        "the non-rehydrated spill's engagement gauge counts it"
    );
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .publish_blob_composes
            .load(Ordering::Relaxed),
        composes0,
        "the rehydrated-compose gauge keeps its meaning (no rehydration here)"
    );

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

/// Contract (the trigger's fail-safe half): an UNARMED mount whose chain
/// compaction crosses the inline ceiling REFUSES retried-class — the
/// error names the cap arithmetic and the compaction arm — and stages
/// NOTHING: the durable head keeps the base map, the ledger keeps the
/// base takes, and the volume's checkpoint stays healthy. Never the
/// oversize record (which admission passed and only the freeze could
/// refuse — the 2026-08-19 permanent wedge).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unarmed_chained_merge_crossing_the_inline_cap_refuses_and_stages_nothing() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    // NO map hook installed — the unarmed shape.
    let (be, ino, base) = crossing_sandbox(dir.path(), "own-cross-un", 300).await;
    squeezefs::routing::set_publish_commit_group_override(Some(1));
    let vol = &be.volumes[0];
    let (added, _composed, size) = crossing_delta(vol, &base, 250);

    let d = delta_owned(size, &added, (0, squeezefs::dlm::mint_layout_version()));
    let err = vol
        .merge_layout_and_size_chained(
            ino,
            ino,
            &d,
            bytes::Bytes::from(base_layout_bytes_owned(size, &added)),
            size,
            vec![],
        )
        .await
        .expect_err(
            "an unarmed crossing must REFUSE — staging the oversize record \
             wedges the volume's checkpoint at freeze time, forever",
        );
    let text = format!("{err}");
    assert!(
        text.contains("layout delta base unusable"),
        "the refusal carries the retried-class marker (the writeback \
         ladder's classifier keys on it): {text}"
    );
    let ceiling = vol
        .xattr_value_cap()
        .saturating_sub(squeezefs::routing::LAYOUT_INLINE_HEADROOM);
    assert!(
        text.contains(&format!("{ceiling}")),
        "the refusal names the inline ceiling it refused against: {text}"
    );

    // Nothing staged: the durable head is still the base inline map and
    // the ledger still holds exactly the base takes.
    let map = owner_map(&be, ino).await;
    assert_eq!(map.len(), base.len(), "the base map survives untouched");
    assert!(
        map.keys().all(|b| *b < base.len() as u32),
        "no delta entry leaked into the durable head"
    );
    let mut want: Vec<(u64, u64, u32)> = base.iter().map(|(b, k)| (kid(k), ino, *b)).collect();
    want.sort_unstable();
    assert_eq!(ledger(&be).await, want, "the ledger is untouched");

    for vol in &be.volumes {
        vol.shutdown().await.expect("shutdown");
    }
}

/// Contract (the trigger, site (a) — the aggregated conveyor member):
/// the SAME crossing law through the layout-merge pass (the default
/// aggregated path — the field's convicted site). One member, one pass:
/// composes to indirect, MAP_BLOB take staged, no released op, gauge
/// counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_armed_crossing_composes_via_the_aggregated_pass() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let io = install_test_map_io();
    let (be, ino, base) = crossing_sandbox(dir.path(), "own-cross-a", 300).await;
    // NO group override: the aggregated conveyor path — site (a).
    let vol = &be.volumes[0];
    let (added, composed, size) = crossing_delta(vol, &base, 250);
    let composes0 = squeezefs::fuse_client::METRICS
        .publish_blob_composes
        .load(Ordering::Relaxed);

    let d = delta_owned(size, &added, (0, squeezefs::dlm::mint_layout_version()));
    let (used, version) = vol
        .merge_layout_and_size_chained(
            ino,
            ino,
            &d,
            bytes::Bytes::from(base_layout_bytes_owned(size, &added)),
            size,
            vec![],
        )
        .await
        .expect("the aggregated crossing member composes instead of staging oversize");
    assert!(!used, "a compose stages a full Put, never a delta");
    assert_eq!(version, 0);

    let head = raw_head(&be, ino).await;
    assert_eq!(
        head.block_map_id.as_deref(),
        Some("indirect:be://data:fresh-1"),
        "the aggregated pass's compaction member crossed to INDIRECT"
    );
    let mut want_map = composed.clone();
    want_map.sort_unstable_by_key(|&(b, _)| b);
    assert_eq!(
        io.blobs
            .lock()
            .unwrap()
            .get("be://data:fresh-1")
            .cloned()
            .expect("the fresh blob was written"),
        want_map,
    );
    assert_eq!(
        ledger(&be).await,
        crossing_ledger_want(ino, &base, &added, "be://data:fresh-1"),
        "delta entries + MAP_BLOB take, no released op"
    );
    assert!(io.freed.lock().unwrap().is_empty());
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .publish_blob_composes
            .load(Ordering::Relaxed)
            - composes0,
        1,
    );

    for vol in &be.volumes {
        vol.shutdown().await.expect("shutdown");
    }
}

/// Contract (the crossing member joins the pass-local memo — the
/// batch-prior law's crossing face): a crossing member's composed state
/// SEEDS `composed_heads`, so a same-ino batch mate composes onto the
/// ACCUMULATED view (fresh-2 over fresh-1) instead of staging a delta
/// onto the just-staged indirect head — which the fold layer classifies
/// as corruption ("layout delta base unusable", every subsequent
/// checkpoint of the key refusing forever: the exact 1c poison, minted
/// from INSIDE one pass).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_armed_crossing_member_seeds_the_pass_memo_for_its_batch_mates() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let io = install_test_map_io();
    let (be, ino, base) = crossing_sandbox(dir.path(), "own-cross-m", 300).await;
    let vol = &be.volumes[0];
    let (added, _composed, size) = crossing_delta(vol, &base, 250);
    let late: Vec<(u32, String)> = (700..703u32)
        .map(|i| (i, format!("be://data:L{i:05}")))
        .collect();

    // Hold the pass so both members drain in ONE batch, FIFO: the
    // crossing member first (enqueued first), its small mate second.
    squeezefs::meta_backend::kv::backend::TEST_LAYOUT_MERGE_HOLD_MS.store(200, Ordering::Relaxed);
    let d_a = delta_owned(size, &added, (0, squeezefs::dlm::mint_layout_version()));
    let d_b = delta_owned(size, &late, (0, squeezefs::dlm::mint_layout_version()));
    let vol_a = Arc::clone(vol);
    let fb_a = bytes::Bytes::from(base_layout_bytes_owned(size, &added));
    let a = tokio::spawn(async move {
        vol_a
            .merge_layout_and_size_chained(ino, ino, &d_a, fb_a, size, vec![])
            .await
    });
    // The stagger: A is queued (and the held pass elected) before B.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let vol_b = Arc::clone(vol);
    let fb_b = bytes::Bytes::from(base_layout_bytes_owned(size, &late));
    let b = tokio::spawn(async move {
        vol_b
            .merge_layout_and_size_chained(ino, ino, &d_b, fb_b, size, vec![])
            .await
    });
    let ra = a.await.expect("task a").expect("crossing member composes");
    let rb = b.await.expect("task b").expect("batch mate composes");
    squeezefs::meta_backend::kv::backend::TEST_LAYOUT_MERGE_HOLD_MS.store(0, Ordering::Relaxed);
    assert!(!ra.0 && !rb.0, "both members stage full Puts, never deltas");

    // The final head names the SECOND fresh blob (accumulated view).
    let head = raw_head(&be, ino).await;
    assert_eq!(
        head.block_map_id.as_deref(),
        Some("indirect:be://data:fresh-2"),
        "the mate composed onto the crossing member's memo entry"
    );
    let mut want_map: Vec<(u32, String)> = base
        .iter()
        .chain(added.iter())
        .chain(late.iter())
        .cloned()
        .collect();
    want_map.sort_unstable_by_key(|&(b, _)| b);
    assert_eq!(
        io.blobs
            .lock()
            .unwrap()
            .get("be://data:fresh-2")
            .cloned()
            .expect("the final blob was written"),
        want_map,
        "BOTH members' entries in the final blob"
    );

    // Ledger net: base + both members' entries + the FINAL blob take;
    // the intermediate fresh blob's take+release nets to absent, and
    // NOTHING releases an inline head.
    let mut want: Vec<(u64, u64, u32)> = base
        .iter()
        .chain(added.iter())
        .chain(late.iter())
        .map(|(b, k)| (kid(k), ino, *b))
        .collect();
    want.push((
        kid("be://data:fresh-2"),
        ino,
        block_refs::BLOCK_INDEX_MAP_BLOB,
    ));
    want.sort_unstable();
    assert_eq!(ledger(&be).await, want);

    // Freed: exactly the superseded intermediate blob — never an "old"
    // blob (the head was inline), never the final one.
    assert_eq!(
        io.freed.lock().unwrap().clone(),
        vec!["be://data:fresh-1".to_string()],
        "the pass-superseded intermediate blob freed after the ONE commit"
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
