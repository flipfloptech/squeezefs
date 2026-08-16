//! **The shipped-publish ERA GATE + idempotence witness** — DLM S9,
//! finding #6 of the rung-10 arm (`.benchmarks/2026-08-16-mw-s9-arm.md`,
//! findings ledger #6 + Named residuals item 1), adjudicated in
//! `docs/design-mw-layout-versions.md` §6a (publish schema 5).
//!
//! # The hole (RED against dev `b68f933b`)
//!
//! The layout-publish verbs (`SetLayoutAndSize` / `MergeLayoutAndSize` /
//! `CommitBlockRefs`, plus `ParkWriteTimes` / `DestroyInodes` /
//! `CreateWithRdevSize`) carried **no lease-epoch/era gate** — only
//! `FreeBlocks` / `HarvestLaneFree` / `RaiseAllocLane` did — so a
//! swept-but-not-yet-self-fenced zombie's layout publishes were still
//! APPLIED by the authority. And the vocabulary's no-retry law composed
//! with the never-lossy writeback ladder into an **unwitnessed duplicate
//! re-ship**: a freeze-window lost-reply publish re-shipped with no
//! `(lease_epoch, request_id)` window could re-apply — the s9-colocated-
//! fence leg's durable divergent delta chain (fsck C1 on `TREE_XATTRS`
//! plus 220 C8 findings, drift 48,620). Bit 15 DETECTED the divergence
//! (its design role); this suite pins the PREVENTION laws.
//!
//! # The laws under contract (design §6a)
//!
//! 1. every MUTATING publish verb carries `lease_epoch`; a verb whose
//!    epoch is not LIVE custody refuses `PUBLISH_STALE_LEASE` and applies
//!    **nothing** — refused BEFORE the dedup window (the FreeBlocks
//!    precedent verbatim);
//! 2. the layout-publish class joins the RETRIED class under the
//!    `(lease_epoch, request_id)` witness — a duplicate re-ship answers
//!    the winner's own cached outcome (journal-entry equality; a later
//!    publish's state is never clobbered by an earlier frame's replay);
//! 3. a current-epoch stale refusal IS the client's fence signal (the
//!    pull-based revocation law): grants dead + custody generation
//!    advanced + the writer self-fence (custody poison), surfaced as
//!    `WriterGuardFenced`; a refusal for an epoch the client already
//!    REPLACED fences nothing;
//! 4. under the poison latch — and ONLY under it — a fence-class
//!    constant-writeback unit resolves as a verified fencing-stale no-op
//!    (`writeback_fence_noops`) instead of retrying forever; a LIVE era's
//!    work is never dropped;
//! 5. solo mounts are structurally untouched: the epoch/witness are
//!    gathered only in the shipped arm.
//!
//! **No numbers here — ruling D11.** The live repro is the rig's
//! `s9-colocated-fence` leg (its trailing fsck oracle is the acceptance
//! gate); this file is its deterministic cargo port per the repro-port
//! mandate.

use squeezefs::data_custody;
use squeezefs::data_grant::{self, WriteCustodyClient, WriteCustodyOwner};
use squeezefs::error::SqueezefsError;
use squeezefs::fuse_client::{self, METRICS};
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

/// The `job:enroll`-class storage-trust secret (S3's root of trust).
const SECRET: &[u8] = b"s9-publish-era-gate-storage-trust-secret";

const VOL_LEN: u64 = 256 * 1024 * 1024;

const NODE: &str = "node-era-gate-a";

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
        ship::disarm_ownership();
        data_custody::test_reset_custody_generation();
        data_custody::test_clear_poison();
    }
}

fn restore() -> Restore {
    Restore
}

// ---------------------------------------------------------------------------
// Fixtures (the dlm_multi_writer_tests shapes: one authority backend +
// listener, one all-foreign client backend, a real custody join)
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

/// One opened single-volume set; `stamp` engages bit 15 (the version gate)
/// between format and open, the Phase-8 shape.
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
}

fn start_authority(inner: Arc<RoutedMetaBackend>) -> Authority {
    let ms = Arc::new(AtomicU64::new(1_000));
    let clock = LeaseClock::manual(Arc::clone(&ms));
    let clocks = LeaseClocks::with_params(
        Duration::from_millis(3_000),
        Duration::from_millis(200),
        Duration::from_millis(400),
    )
    .expect("positive T_self");
    let owner = WriteCustodyOwner::arm(
        "era-gate-authority",
        squeezefs::dlm::durable_term() + 1,
        squeezefs::dlm::durable_term(),
        clocks,
        clock,
        None,
    )
    .expect("the custody authority arms");
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
    }
}

/// Arm the CLIENT half: all-foreign ownership over `client_be`, a publish
/// client, and a REAL custody join (the lease epoch every mutating publish
/// now presents). Returns the joined custody client.
async fn arm_client(
    auth: &Authority,
    client_be: &Arc<RoutedMetaBackend>,
) -> (Arc<WriteCustodyClient>, Arc<publish::PublishClient>) {
    let foreign: Vec<(usize, PeerOwner)> = (0..client_be.volumes.len())
        .map(|v| (v, PeerOwner::new("era-gate-authority", &auth.endpoint)))
        .collect();
    ship::arm_ownership(OwnerMap::for_volumes(client_be, foreign).expect("owner map"));
    let pc = publish::PublishClient::new(NODE, SECRET.to_vec());
    publish::install_client(Arc::clone(&pc));
    let client = WriteCustodyClient::connect(&auth.endpoint, SECRET, NODE)
        .await
        .expect("the co-writer joins the custody plane");
    data_grant::install_custody_client(Arc::clone(&client));
    (client, pc)
}

fn journal_entries() -> u64 {
    squeezefs::meta_backend::kv::META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed)
}

/// One layout delta at the shipped shape (the dlm_multi_writer_tests
/// fixture), version pair applied when `versions` is `Some`.
fn delta(size: u64, key: &str, versions: Option<(u64, u64)>) -> LayoutDelta {
    let mut d = LayoutDelta::from_final_state(
        "striped",
        size,
        None,
        Some("be://data"),
        None,
        None,
        vec![(0, key.to_string())],
    );
    if let Some((base, version)) = versions {
        d.set_versions(base, version);
    }
    d
}

/// A DECODABLE base layout `Put` (the fold applies deltas onto it, so the
/// bytes must be real bincode `LayoutMetadata`, not an opaque marker).
fn base_layout_bytes(size: u64) -> Vec<u8> {
    bincode::serialize(&LayoutMetadata {
        file_type: "striped".into(),
        size,
        block_map_id: None,
        block_prefix: Some("be://data".into()),
        file_id: None,
        data_key: None,
        block_map: Some(std::collections::HashMap::new()),
    })
    .expect("serialize base layout")
}

async fn owner_size(be: &Arc<RoutedMetaBackend>, ino: u64) -> u64 {
    use squeezefs::meta_backend::Metadata;
    be.getattr(ino).await.expect("the owner has the ino").size
}

// ===========================================================================
// 1. The era gate: a stale-era layout publish refuses, applies nothing,
//    and IS the client's fence signal
// ===========================================================================

/// Contract (law 1 + law 3): after the authority sweeps this client's
/// custody, EVERY mutating publish verb refuses by era — nothing is
/// applied on the owner — and the refusal composes the client's full
/// fence (custody poison, `WriterGuardFenced` class), because a
/// current-epoch stale refusal is the pull-based revocation channel
/// firing on the publish plane.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_era_layout_publish_refuses_applies_nothing_and_is_the_fence_signal() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own", false).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli", false).await;
    let auth = start_authority(Arc::clone(&owner_be));
    let (_client, _pc) = arm_client(&auth, &client_be).await;

    // Live-era baseline: the shipped publish path works under custody.
    let ino = publish::create_with_rdev_size(
        &client_be,
        1,
        "fenced.bin",
        libc::S_IFREG | 0o644,
        0,
        0,
        0,
        0,
    )
    .await
    .expect("a live-era shipped create lands")
    .ino;
    publish::set_layout_and_size(&client_be, ino, b"layout:v1:live", 4096, &[])
        .await
        .expect("a live-era layout publish lands");
    assert_eq!(owner_size(&owner_be, ino).await, 4096);

    // The sweep: the authority revokes this client (the SIGSTOP-past-TTL
    // shape) while the client has NOT yet self-fenced — finding #6's
    // window.
    auth.owner.revoke_client(NODE, "test: swept past its TTL");
    assert!(
        !data_custody::poisoned(),
        "the window is real: the swept client has not yet self-fenced"
    );

    let stale_before = publish::stats().stale_refusals;
    let journal_before = journal_entries();

    // Every mutating verb in the zombie window must REFUSE.
    let set_err = publish::set_layout_and_size(&client_be, ino, b"layout:v2:zombie", 8192, &[])
        .await
        .expect_err("a swept era's set_layout_and_size refuses");
    assert!(
        matches!(set_err, SqueezefsError::WriterGuardFenced),
        "the refusal surfaces in the FENCE class (every retry ladder returns it \
         immediately): {set_err:?}"
    );
    let d = delta(8192, "be://data:0", None);
    publish::merge_layout_and_size(
        &client_be,
        ino,
        &d,
        bytes::Bytes::from_static(b"x"),
        8192,
        Vec::new(),
    )
    .await
    .expect_err("a swept era's merge_layout_and_size refuses");
    publish::commit_block_refs(&client_be, ino, &[])
        .await
        .expect_err("a swept era's commit_block_refs refuses");
    publish::park_write_times(&client_be, ino, 111, 222)
        .await
        .expect_err("a swept era's park_write_times refuses");
    publish::destroy_inodes(&client_be, &[ino])
        .await
        .expect_err("a swept era's destroy_inodes refuses");

    // Nothing was applied: the owner's durable state is the live era's.
    assert_eq!(
        owner_size(&owner_be, ino).await,
        4096,
        "a refusal means NOTHING was applied"
    );
    assert_eq!(
        journal_entries(),
        journal_before,
        "a refused publish stages no journal entry (applies nothing)"
    );
    assert!(
        publish::stats().stale_refusals - stale_before >= 5,
        "every refusal is counted on the era gate's own row"
    );

    // Law 3: the current-epoch stale refusal composed the FULL fence —
    // the client learned it is dead at this round trip (pull-based
    // revocation), not at some later renewal.
    assert!(
        data_custody::poisoned(),
        "a current-epoch stale refusal poisons the writer's custody (the self-fence \
         composition) — the zombie is dead the moment the authority says so"
    );

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

// ===========================================================================
// 2. The witness: a duplicate re-ship answers from the window
// ===========================================================================

/// Contract (law 2): a duplicate re-ship of ONE logical layout publish —
/// the freeze-window lost-reply retry, carrying the SAME
/// `(lease_epoch, request_id)` — is answered from the owner's dedup
/// window: journal-entry equality (nothing re-applied), the reply is the
/// winner's own, the replay is counted, and a LATER publish's state is
/// never clobbered by the earlier frame's replay (the divergence mint the
/// s9-colocated-fence leg convicted).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_duplicate_reship_answers_from_the_witness_and_never_clobbers_a_later_publish() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    // Stamped: bit 15 live, so the chain the duplicate would divergently
    // re-base is REAL (the leg's shape).
    let (owner_be, _p) = sandbox(dir.path(), "own", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli", false).await;
    let auth = start_authority(Arc::clone(&owner_be));
    let (client, pc) = arm_client(&auth, &client_be).await;
    let epoch = client.lease_epoch();

    let ino = publish::create_with_rdev_size(
        &client_be,
        1,
        "witnessed.bin",
        libc::S_IFREG | 0o644,
        0,
        0,
        0,
        0,
    )
    .await
    .expect("create ships")
    .ino;
    let base = base_layout_bytes(4096);
    publish::set_layout_and_size(&client_be, ino, &base, 4096, &[])
        .await
        .expect("the base Put lands");

    // P1: the first link (claim 0, mint V1).
    let v1 = squeezefs::dlm::mint_layout_version();
    let p1 = publish::PublishCall::MergeLayoutAndSize {
        ino,
        delta: delta(8192, "be://data:0", Some((0, v1))).encode(),
        full_layout: base_layout_bytes(8192),
        size: 8192,
        refs: Vec::new(),
        lease_epoch: epoch,
        request_id: 0xA1,
    };
    let first = pc
        .ship(&auth.endpoint, p1.clone())
        .await
        .expect("P1 applies");

    // P2: the second link (claim V1, mint V2) — a LATER logical publish.
    let v2 = squeezefs::dlm::mint_layout_version();
    let p2 = publish::PublishCall::MergeLayoutAndSize {
        ino,
        delta: delta(12288, "be://data:1", Some((v1, v2))).encode(),
        full_layout: base_layout_bytes(12288),
        size: 12288,
        refs: Vec::new(),
        lease_epoch: epoch,
        request_id: 0xA2,
    };
    pc.ship(&auth.endpoint, p2).await.expect("P2 applies");
    assert_eq!(owner_size(&owner_be, ino).await, 12288);

    // The duplicate: P1 re-ships VERBATIM (the lost-reply retry — same
    // frame, same witness key; retries never re-key). Pre-fix this
    // re-applied (claim 0 against the advanced head re-based with P1's
    // STALE full layout — the clobber).
    let replays_before = publish::stats().replays;
    let journal_before = journal_entries();
    let replayed = pc
        .ship(&auth.endpoint, p1)
        .await
        .expect("the duplicate is ANSWERED, not re-applied");
    assert_eq!(
        replayed, first,
        "a replay answers the winner's own cached outcome"
    );
    assert_eq!(
        publish::stats().replays - replays_before,
        1,
        "and it is counted as a witness hit"
    );
    assert_eq!(
        journal_entries(),
        journal_before,
        "journal-entry equality: the duplicate staged NOTHING"
    );
    assert_eq!(
        owner_size(&owner_be, ino).await,
        12288,
        "the later publish's state survives the earlier frame's replay — the divergence \
         mint is structurally impossible"
    );

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

// ===========================================================================
// 3. The writeback resolution law: verified fencing-stale no-op, ONLY
//    under the poison latch
// ===========================================================================

/// Contract (law 4): the constant-writeback disposition for a fence-class
/// unit failure keys on the ONE-WAY poison latch — the verified era
/// death — and on nothing else. Poisoned + fence class ⇒ the unit
/// resolves as a fencing-stale no-op (counted, never requeued — the
/// retry-forever wedge closed); a LIVE era's fence-class transient (or
/// any non-fence error) keeps the never-lossy ladder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fence_class_writeback_unit_resolves_as_a_verified_noop_only_under_poison() {
    let _serial = serial();
    let _restore = restore();

    let noops = || METRICS.writeback_fence_noops.load(Ordering::Relaxed);

    // Live era: fence-class errors keep the ladder (never a silent drop
    // of a live era's work).
    data_custody::test_clear_poison();
    let before = noops();
    assert!(
        !fuse_client::writeback_fence_resolution(&SqueezefsError::WriterGuardFenced),
        "a fence-class error WITHOUT the poison latch keeps the never-lossy ladder"
    );
    assert_eq!(noops(), before, "and counts nothing");

    // Dead era: the latch is set (T_self / D0 fence / the authoritative
    // stale-lease refusal — the only setters), so the unit can never
    // succeed until remount and the remount contract owns its bytes.
    data_custody::poison("test: verified era death");
    let before = noops();
    assert!(
        fuse_client::writeback_fence_resolution(&SqueezefsError::WriterGuardFenced),
        "poisoned + fence class resolves as the verified fencing-stale no-op"
    );
    assert_eq!(noops(), before + 1, "counted on writeback_fence_noops");

    // The latch never widens the class: a non-fence error on a poisoned
    // mount still rides the ladder (it may be a genuinely transient
    // condition on a unit whose bytes teardown will own).
    let before = noops();
    assert!(
        !fuse_client::writeback_fence_resolution(&SqueezefsError::Io(std::io::Error::other(
            "transient"
        ))),
        "a non-fence error never takes the no-op arm, poisoned or not"
    );
    assert_eq!(noops(), before);
}

// ===========================================================================
// 4. Out-of-order and cross-era shapes refuse
// ===========================================================================

/// Contract (laws 1–3's composition): (a) a delta claiming a base that is
/// not the durable head REFUSES across the wire (the §3 version gate's
/// shipped face — the belt under the ordering construction) and applies
/// nothing; (b) after a revocation + re-join, a frame presenting the DEAD
/// epoch refuses by era while the re-joined client keeps working — and
/// the stale refusal for the REPLACED epoch fences nothing (the guard is
/// presented == current).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn out_of_order_and_cross_era_publishes_refuse() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p) = sandbox(dir.path(), "own", true).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli", false).await;
    let auth = start_authority(Arc::clone(&owner_be));
    let (client, pc) = arm_client(&auth, &client_be).await;
    let epoch1 = client.lease_epoch();

    let ino = publish::create_with_rdev_size(
        &client_be,
        1,
        "ordered.bin",
        libc::S_IFREG | 0o644,
        0,
        0,
        0,
        0,
    )
    .await
    .expect("create ships")
    .ino;
    let base = base_layout_bytes(4096);
    publish::set_layout_and_size(&client_be, ino, &base, 4096, &[])
        .await
        .expect("the base Put lands");
    let v1 = squeezefs::dlm::mint_layout_version();
    pc.ship(
        &auth.endpoint,
        publish::PublishCall::MergeLayoutAndSize {
            ino,
            delta: delta(8192, "be://data:0", Some((0, v1))).encode(),
            full_layout: base_layout_bytes(8192),
            size: 8192,
            refs: Vec::new(),
            lease_epoch: epoch1,
            request_id: 0xB1,
        },
    )
    .await
    .expect("the first link applies");

    // (a) OUT OF ORDER / foreign provenance: a delta claiming a base the
    // durable head never was — the two-writers-disagree shape §3 refuses.
    let bogus_base = squeezefs::dlm::mint_layout_version();
    let v_next = squeezefs::dlm::mint_layout_version();
    let journal_before = journal_entries();
    let err = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::MergeLayoutAndSize {
                ino,
                delta: delta(16384, "be://data:9", Some((bogus_base, v_next))).encode(),
                full_layout: base_layout_bytes(16384),
                size: 16384,
                refs: Vec::new(),
                lease_epoch: epoch1,
                request_id: 0xB2,
            },
        )
        .await
        .expect_err("a divergent-base delta refuses loud across the wire");
    assert!(
        err.to_string().contains("6.2")
            || err.to_string().to_lowercase().contains("divergent")
            || err.to_string().to_lowercase().contains("base"),
        "the refusal names the version-gate divergence: {err}"
    );
    assert_eq!(
        owner_size(&owner_be, ino).await,
        8192,
        "a refused out-of-order publish applies nothing"
    );
    assert_eq!(journal_entries(), journal_before);

    // (b) CROSS ERA: revoke + re-join (a new lease epoch), then present
    // the DEAD epoch.
    auth.owner.revoke_client(NODE, "test: revoked for re-join");
    let client2 = WriteCustodyClient::connect(&auth.endpoint, SECRET, NODE)
        .await
        .expect("the re-join mints a fresh lease");
    data_grant::install_custody_client(Arc::clone(&client2));
    let epoch2 = client2.lease_epoch();
    assert_ne!(epoch1, epoch2, "a lease epoch is never reused");

    let stale_before = publish::stats().stale_refusals;
    pc.ship(
        &auth.endpoint,
        publish::PublishCall::CommitBlockRefs {
            ino,
            refs: Vec::new(),
            lease_epoch: epoch1,
            request_id: 0xB3,
        },
    )
    .await
    .expect_err("the dead era's frame refuses by era even after the identity re-joined");
    assert_eq!(publish::stats().stale_refusals - stale_before, 1);
    assert!(
        !data_custody::poisoned(),
        "a stale refusal for an epoch this client already REPLACED fences nothing — the \
         guard is presented == current"
    );

    // The re-joined era keeps working (an epoch advance never poisons).
    pc.ship(
        &auth.endpoint,
        publish::PublishCall::CommitBlockRefs {
            ino,
            refs: Vec::new(),
            lease_epoch: epoch2,
            request_id: 0xB4,
        },
    )
    .await
    .expect("the LIVE era's publish still lands");

    auth.listener.shutdown();
    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

// ===========================================================================
// 5. Solo mounts are structurally untouched
// ===========================================================================

/// Contract (law 5): with nothing armed — every mount that ships — the
/// publish helpers take today's local path verbatim: no wire, no custody
/// probe outcome, no era refusals, no witness rows, no fence-noop
/// movement. The epoch/witness are gathered ONLY in the shipped arm.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_solo_publish_path_is_structurally_untouched() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (be, _p) = sandbox(dir.path(), "solo", false).await;

    let before = publish::stats();
    let fence_noops_before = METRICS.writeback_fence_noops.load(Ordering::Relaxed);

    let ino = publish::create_with_rdev_size(&be, 1, "solo.bin", libc::S_IFREG | 0o644, 0, 0, 0, 0)
        .await
        .expect("the local create is unchanged")
        .ino;
    publish::set_layout_and_size(&be, ino, b"layout:v1:solo", 4096, &[])
        .await
        .expect("the local layout publish is unchanged");
    let d = delta(8192, "be://data:0", None);
    publish::merge_layout_and_size(
        &be,
        ino,
        &d,
        bytes::Bytes::from_static(b"x"),
        8192,
        Vec::new(),
    )
    .await
    .expect("the local merge is unchanged");
    publish::commit_block_refs(&be, ino, &[])
        .await
        .expect("the local ref commit is unchanged");
    publish::park_write_times(&be, ino, 1, 1)
        .await
        .expect("the local park is unchanged");

    let after = publish::stats();
    assert_eq!(after.shipped, before.shipped, "nothing shipped");
    assert_eq!(
        after.local,
        before.local + 5,
        "every call took the local path"
    );
    assert_eq!(
        after.stale_refusals, before.stale_refusals,
        "the era gate never runs on the local path"
    );
    assert_eq!(
        after.replays, before.replays,
        "the witness never runs on the local path"
    );
    assert_eq!(
        METRICS.writeback_fence_noops.load(Ordering::Relaxed),
        fence_noops_before
    );

    shutdown(&be).await;
}
