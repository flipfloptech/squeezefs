//! DLM stage **S9 — the multi-writer data plane**
//! (`docs/pre-rc-engineering-spec.md` §6.9 S9 row — *"custody tokens,
//! remote clients DMA directly"*, guarantee after: **full multi-writer on
//! PR substrates, refused on non-PR**; §6.7 "Recovery" / "On external
//! consensus"; §6.3's coherence obligations; execution-plan rulings
//! **D8** (N coherent writers + N coherent readers, *including concurrent
//! writers to different regions of the SAME large file*), **D9** (bits are
//! built, never stamped) and **D11** (the measured half is frozen)).
//!
//! # What S9 adds to what already shipped
//!
//! Every piece S9 needs was landed by a prior stage *for this stage*:
//!
//! | Inherited | From | What S9 does with it |
//! |---|---|---|
//! | `authorize_dma(Some(epoch))` — THE authorization point | S7 | the remote grant's epoch is carried through it; **no third door** |
//! | `declare_dead_epoch` → `quarantine_offset` → drain proof → `release_quarantine` | S7 | a revoked/expired grant's in-flight offsets ride exactly this lifecycle |
//! | `acquire_wero` — the ONE data-namespace reservation | S7 | joined, never re-acquired; its preempt IS the drain proof |
//! | `MembershipOwner::evict` / the stricter member clock `T_self` | S6 | custody expiry and the writer's own pre-emptive self-fence |
//! | `arm_ownership` (no caller — *"the mount arm is yours"*) | S8 | armed by S9's mount arm, together with membership and custody |
//! | `is_local_slot` — the ownership extension point | S4 | a foreign home is no longer a refusal: it is a **remote acquire** |
//! | `FileCustody`'s sorted byte-range interval list + `span_range_shared` | S11 | disjoint-range grants to two nodes; overlap refuses |
//!
//! # The contracts pinned here
//!
//! 1. **Two nodes writing disjoint FILES** hold custody concurrently, each
//!    DMA'ing directly (the data never funnels through the owner — only
//!    the custody does), and the ledger closes: a client's `grants`
//!    equals the owner's `served`.
//! 2. **Two nodes writing disjoint RANGES of ONE file** both hold custody
//!    at the same time (ruling D8's shared-file target), arbitrated by the
//!    owner's own S11 interval algebra — no second range rule.
//! 3. **A conflicting range is REFUSED**, loudly and specifically, inside
//!    the caller's wait budget (never granted, never silently widened).
//! 4. **grant → revoke → quarantine → drain proof → reallocation**: the
//!    revoked epoch's in-flight offsets are non-reallocatable until a
//!    `DrainProof` exists, and the proof is *unconstructible* without
//!    evidence (a landed WERO preempt, or an attested proof of death).
//! 5. **A client that misses `T_self` self-fences BEFORE the owner may
//!    re-grant** — S6's asymmetry composed with S7's poison, in that
//!    order, so a false-positive eviction costs availability and never
//!    divergence.
//! 6. **Owner failover with the grace window**: a successor must bump the
//!    durable term before arming, admits reclaim, refuses conflicting
//!    fresh acquires, and every pre-failover grant is stale by
//!    construction.
//! 7. **The mount arm refuses on a non-PR substrate** (§6.7 "On external
//!    consensus" — the repo's own loop substrate is exactly that shape)
//!    and **on a format lacking the capability bits**, naming the missing
//!    one and its offline stamping path.
//! 8. **The custody epoch stays ONE decision**: a revoked grant advances
//!    the epoch (it does not poison — poison is mount-wide and sticky),
//!    and an authorization minted under the old generation is refused at
//!    `authorize_dma` with the `data_dma_epoch_refusals` class split.
//! 9. **The publish path ships instead of refusing**: a foreign-home ino's
//!    `set_layout_and_size` / `merge_layout_and_size` /
//!    `commit_block_refs` / `park_write_times` / `destroy_inodes` /
//!    `create_with_rdev_size` / `xattr_value_cap` / `readdir_stream`
//!    execute on the owner and are visible there.
//! 10. **Concurrency**: the whole machinery under a multi-threaded storm
//!     never grants overlapping incompatible custody and never leaks a
//!     quarantined offset.
//!
//! RED against `dev` (b2e72d89): `squeezefs::data_grant` and
//! `squeezefs::multi_writer` do not exist, `data_custody` has no custody
//! generation, `dlm` cannot adopt a remote grant, `dlm_slot` refuses every
//! foreign home, and `meta_ship::publish` does not exist.
//!
//! # What one process CANNOT pin (stated, not hidden)
//!
//! * **Device rejection** of a fenced writer's DMA is a property of a real
//!   PR-capable namespace; the fake namespace pins the *decision* and the
//!   ladder, never the silicon. The deferred leg is
//!   `docs/design-nvmeof-target-management.md` §6.8.1.
//! * **Two independent node caches** diverging needs two hosts; here the
//!   two "nodes" are two independently formatted+opened volume sets in one
//!   process (the S8 test discipline), which is what makes the wire, the
//!   custody arbitration, the epoch algebra and the quarantine real.
//! * **The field arm**: nothing stamps the capability bits (D9), and the
//!   D0 Layer-B2 gate still refuses a fresh foreign claim on every
//!   substrate — so the co-writer posture cannot be reached in the field
//!   yet. These tests stamp the bits through the offline `set_*_bit`
//!   paths, which is exactly the Phase-8 reformat window's act.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cluster_wire as cw;
use squeezefs::data_custody::{self, CustodyEpoch};
use squeezefs::data_grant::{
    self, CustodyQuarantine, DrainProof, WriteCustodyClient, WriteCustodyOwner,
};
use squeezefs::dlm::LockMode;
use squeezefs::fuse_client::METRICS;
use squeezefs::membership::{LeaseClock, LeaseClocks};
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::meta_backend::{open_routed_meta_set, plan_meta_slot_set, RoutedMetaBackend};
use squeezefs::meta_ship::{self as ship, publish, OwnerMap, PeerOwner};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

/// The `job:enroll` storage-trust secret both halves prove possession of
/// (S3's root of trust: whoever can read the shared metadata volume is
/// inside the trust domain).
const SECRET: &[u8] = b"s9-multi-writer-storage-trust-secret";

const VOL_LEN: u64 = 256 * 1024 * 1024;

/// Process-global custody state (the poison latch, the custody
/// generation, the durable term, the WERO registry, the ownership plane)
/// forces serialization: libtest runs a file's tests on threads, and the
/// gate's `--test-threads=1` bounds files, not tests within a file.
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

/// Restores every process-global S9 posture, so a panicking test can never
/// leave the binary armed.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        data_grant::uninstall_custody_client();
        publish::uninstall_client();
        ship::disarm_ownership();
        data_custody::test_reset_custody_generation();
        data_custody::test_clear_poison();
    }
}

fn restore() -> Restore {
    Restore
}

fn epoch_refusals() -> u64 {
    METRICS.data_dma_epoch_refusals.load(Ordering::Relaxed)
}

fn quarantined() -> u64 {
    METRICS.dlm_quarantined_offsets.load(Ordering::Relaxed)
}

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

/// One "node's" formatted + opened metadata volume set. `stamp` runs
/// between format and open, because a mount reads the superblock ONCE (at
/// open) — which is also why the Phase-8 reformat window is an offline act.
async fn sandbox(dir: &Path, tag: &str, volumes: usize) -> (Arc<RoutedMetaBackend>, Vec<PathBuf>) {
    sandbox_stamped(dir, tag, volumes, false).await
}

async fn sandbox_stamped(
    dir: &Path,
    tag: &str,
    volumes: usize,
    stamp: bool,
) -> (Arc<RoutedMetaBackend>, Vec<PathBuf>) {
    let plan = plan_meta_slot_set(volumes).expect("derived plan");
    let mut uris = Vec::new();
    let mut paths = Vec::new();
    for i in 0..volumes {
        let p = make_file(dir, &format!("{tag}-meta{i}"), VOL_LEN);
        squeezefs::meta_backend::kv::builder::format_v3_stamped(
            &p,
            VOL_LEN,
            &opts(),
            plan.stamps[i].clone(),
        )
        .await
        .expect("format stamped meta volume");
        if stamp {
            stamp_capabilities(&p).await;
        }
        uris.push(p.display().to_string());
        paths.push(p);
    }
    let routed = open_routed_meta_set(&uris).await.expect("open routed set");
    (routed, paths)
}

async fn shutdown(routed: &Arc<RoutedMetaBackend>) {
    for vol in &routed.volumes {
        vol.shutdown().await.expect("volume shutdown");
    }
}

/// Deterministic lease clocks for the custody plane: short TTL, a manual
/// millisecond source, and a `T_self` that is strictly earlier than the
/// owner's deadline by construction (the S6 formula, unchanged).
fn clocks() -> (LeaseClocks, Arc<AtomicU64>, LeaseClock) {
    let ms = Arc::new(AtomicU64::new(1_000));
    let clock = LeaseClock::manual(Arc::clone(&ms));
    let c = LeaseClocks::with_params(
        Duration::from_millis(3_000),
        Duration::from_millis(200),
        Duration::from_millis(400),
    )
    .expect("2*skew + purge < TTL, so T_self is positive");
    (c, ms, clock)
}

/// A quarantine sink over one real `BlockAllocator` — the shape the mount
/// arm builds over the backend router.
struct AllocQuarantine {
    alloc: Arc<BlockAllocator>,
}

impl std::fmt::Debug for AllocQuarantine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AllocQuarantine")
            .field("quarantined", &self.alloc.quarantined_count())
            .finish()
    }
}

impl CustodyQuarantine for AllocQuarantine {
    fn quarantine(&self, offsets: &[u64], epoch: squeezefs::data_custody::DeadEpoch) -> usize {
        data_custody::quarantine_offsets(&self.alloc, offsets.iter().copied(), epoch)
    }
    fn release(&self, epoch: squeezefs::data_custody::DeadEpoch) -> usize {
        self.alloc.release_quarantine(epoch)
    }
}

/// The custody + publish authority, served on the S3 wire's pinned lanes.
struct Authority {
    _listener: Arc<cw::RpcListener>,
    owner: Arc<WriteCustodyOwner>,
    endpoint: String,
}

fn start_authority(
    owner: Arc<WriteCustodyOwner>,
    inner: Option<Arc<RoutedMetaBackend>>,
) -> Authority {
    let mut router = data_grant::AsyncVerbRouter::new().with_custody(Arc::clone(&owner));
    if let Some(inner) = inner {
        router = router.with_publish(publish::PublishService::new(
            inner,
            tokio::runtime::Handle::current(),
        ));
    }
    let cfg = cw::RpcListenerConfig {
        bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
        service_threads: 2,
        ..cw::RpcListenerConfig::default()
    };
    let listener = cw::RpcListener::start_async(cfg, SECRET.to_vec(), Arc::new(router))
        .expect("the S9 authority listens");
    let endpoint = listener.endpoint().to_string();
    Authority {
        _listener: listener,
        owner,
        endpoint,
    }
}

async fn owner_with_quarantine(
    alloc: Option<Arc<BlockAllocator>>,
) -> (Arc<WriteCustodyOwner>, Arc<AtomicU64>) {
    let (c, ms, clock) = clocks();
    let sink: Option<Arc<dyn CustodyQuarantine>> = alloc.map(|alloc| {
        let s: Arc<dyn CustodyQuarantine> = Arc::new(AllocQuarantine { alloc });
        s
    });
    let owner = WriteCustodyOwner::arm(
        "owner-a",
        squeezefs::dlm::durable_term() + 1,
        squeezefs::dlm::durable_term(),
        c,
        clock,
        sink,
    )
    .expect("the custody authority arms");
    (owner, ms)
}

async fn client_for(endpoint: &str, id: &str) -> Arc<WriteCustodyClient> {
    WriteCustodyClient::connect(endpoint, SECRET, id)
        .await
        .expect("a co-writer dials the authority")
}

// ---------------------------------------------------------------------------
// 1. Two nodes, disjoint files: custody concurrently, DMA directly
// ---------------------------------------------------------------------------

/// Contract: two co-writers acquire whole-file custody on DIFFERENT inos
/// from one authority and both hold it at the same time. The reply carries
/// each client's own epoch-bearing authorization, and the DMA that follows
/// is authorized locally (`authorize_dma`) — the owner never sees a byte
/// of data, only the custody.
///
/// The ledger closes: two grants issued, two served, two held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_writing_disjoint_files_hold_custody_concurrently() {
    let _serial = serial();
    let _restore = restore();
    let (owner, _ms) = owner_with_quarantine(None).await;
    let auth = start_authority(Arc::clone(&owner), None);

    let ledger_before = data_grant::stats();
    let a = client_for(&auth.endpoint, "node-a").await;
    let b = client_for(&auth.endpoint, "node-b").await;

    let lease_a = a
        .acquire(1000, None, LockMode::Exclusive, Duration::from_millis(500))
        .await
        .expect("node-a takes whole-file custody of ino 1000");
    let lease_b = b
        .acquire(2000, None, LockMode::Exclusive, Duration::from_millis(500))
        .await
        .expect("node-b takes whole-file custody of a DIFFERENT ino");

    assert!(lease_a.is_held().await, "node-a's custody is live");
    assert!(lease_b.is_held().await, "node-b's custody is live");
    assert_ne!(
        lease_a.fencing_token(),
        lease_b.fencing_token(),
        "the authority's single mint gives every grant a globally unique token"
    );
    assert_eq!(auth.owner.held(), 2, "both grants live on the authority");

    // The DATA never funnels through the authority: each client authorizes
    // its own DMA locally, under the epoch its grant established.
    let epoch = data_custody::authorize_dma(None).expect("a granted writer authorizes");
    data_custody::authorize_dma(Some(epoch)).expect("the carried epoch is current");

    let stats = auth.owner.stats();
    assert_eq!(stats.granted, 2, "two grants served");
    assert_eq!(stats.conflicts, 0, "disjoint files never conflict");
    assert_eq!(
        data_grant::stats().grants - ledger_before.grants,
        2,
        "the client-side ledger closes against the authority's (deltas: the counters are \
         process-global, like every house family)"
    );

    drop(lease_a);
    drop(lease_b);
    // Release is the client telling the authority; give the lane a beat to
    // observe it through the wire rather than sleeping on a guess.
    a.drain_releases().await;
    b.drain_releases().await;
    assert_eq!(auth.owner.held(), 0, "both grants retired at release");
}

// ---------------------------------------------------------------------------
// 2 + 3. One file, two regions (ruling D8) — and the conflicting range
// ---------------------------------------------------------------------------

/// Contract (ruling **D8**, verbatim: *"a file especially a large one
/// could be getting read/written to different blocks by different
/// applications and want locks on them"* — the CROSS-NODE face): two
/// co-writers hold disjoint byte ranges of ONE ino simultaneously, and the
/// arbitration is the authority's own S11 interval algebra. S9 adds a
/// transport, never a second range rule.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_writing_disjoint_ranges_of_one_file_both_hold_custody() {
    let _serial = serial();
    let _restore = restore();
    let (owner, _ms) = owner_with_quarantine(None).await;
    let auth = start_authority(Arc::clone(&owner), None);
    let a = client_for(&auth.endpoint, "node-a").await;
    let b = client_for(&auth.endpoint, "node-b").await;

    const INO: u64 = 4242;
    let lo = a
        .acquire(
            INO,
            Some((0, 4 << 20)),
            LockMode::Exclusive,
            Duration::from_millis(500),
        )
        .await
        .expect("node-a takes [0,4Mi)");
    let hi = b
        .acquire(
            INO,
            Some((4 << 20, 8 << 20)),
            LockMode::Exclusive,
            Duration::from_millis(500),
        )
        .await
        .expect("node-b takes the DISJOINT [4Mi,8Mi) of the same file");

    assert!(lo.is_held().await && hi.is_held().await);
    assert_eq!(
        auth.owner.held(),
        2,
        "disjoint spans of one file are two live grants"
    );
    assert_eq!(auth.owner.stats().conflicts, 0);

    // W1's seventh clause reads LOCAL custody, and under S9 local custody
    // is the authority's arbitrated answer: node-a's own span is solely
    // its own, so the clause is inert on it...
    assert!(
        !squeezefs::dlm::span_range_shared(INO, 0, 4 << 20, lo.fencing_token()),
        "a span this node was granted exclusively is not range-shared for it"
    );
    // ...and a span it does NOT hold is range-shared, which is exactly the
    // patch ineligibility the clause exists to express.
    assert!(
        squeezefs::dlm::span_range_shared(INO, 0, 8 << 20, lo.fencing_token()),
        "a wider span than the grant covers is range-shared"
    );

    drop(lo);
    drop(hi);
    a.drain_releases().await;
    b.drain_releases().await;
}

/// Contract: an OVERLAPPING exclusive range is refused — loudly, named,
/// and inside the caller's own wait budget. Never granted, never widened,
/// never silently converted to a whole-file lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_conflicting_range_is_refused_inside_the_wait_budget() {
    let _serial = serial();
    let _restore = restore();
    let (owner, _ms) = owner_with_quarantine(None).await;
    let auth = start_authority(Arc::clone(&owner), None);
    let a = client_for(&auth.endpoint, "node-a").await;
    let b = client_for(&auth.endpoint, "node-b").await;

    const INO: u64 = 5150;
    let held = a
        .acquire(
            INO,
            Some((0, 1 << 20)),
            LockMode::Exclusive,
            Duration::from_millis(500),
        )
        .await
        .expect("node-a takes [0,1Mi)");

    let err = b
        .acquire(
            INO,
            Some((512 << 10, 2 << 20)),
            LockMode::Exclusive,
            Duration::from_millis(150),
        )
        .await
        .expect_err("an overlapping exclusive span must be REFUSED");
    let msg = err.to_string();
    assert!(
        msg.contains(&INO.to_string()),
        "the refusal names the object: {msg}"
    );
    assert!(
        msg.to_lowercase().contains("custody") || msg.to_lowercase().contains("held"),
        "the refusal says what it refused and why: {msg}"
    );
    assert_eq!(
        auth.owner.held(),
        1,
        "a refused acquire leaves exactly the one live grant"
    );
    assert_eq!(auth.owner.stats().conflicts, 1, "counted as a conflict");

    // A whole-file request conflicts with the live span too (whole-inode
    // custody covers every span — the S11 law, unchanged).
    let err = b
        .acquire(INO, None, LockMode::Exclusive, Duration::from_millis(150))
        .await
        .expect_err("whole-file custody conflicts with a live span");
    assert!(!err.to_string().is_empty());

    drop(held);
    a.drain_releases().await;

    // With the span released the same request now succeeds — the refusal
    // was custody, not a broken predicate.
    let after = b
        .acquire(
            INO,
            Some((512 << 10, 2 << 20)),
            LockMode::Exclusive,
            Duration::from_millis(500),
        )
        .await
        .expect("the span is grantable once the holder releases");
    drop(after);
    b.drain_releases().await;
}

// ---------------------------------------------------------------------------
// 4. grant → revoke → quarantine → drain proof → reallocation
// ---------------------------------------------------------------------------

/// Contract (§6.7 "Recovery", S7's lifecycle applied verbatim): a revoked
/// grant's in-flight destinations enter the dead-epoch quarantine, no
/// allocation path can hand them out, and only a **`DrainProof`** releases
/// them. The proof is unconstructible without evidence, which is what
/// makes "a release without a proof is a correctness bug" a type property
/// rather than a comment.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_custody_quarantines_its_offsets_until_a_drain_proof() {
    let _serial = serial();
    let _restore = restore();
    let alloc = Arc::new(
        BlockAllocator::new("s9-quarantine")
            .await
            .expect("allocator"),
    );
    alloc.set_capacity_bytes(64 * alloc.chunk_size());
    let offset = alloc
        .allocate_block()
        .await
        .expect("one destination for the co-writer");

    let (owner, _ms) = owner_with_quarantine(Some(Arc::clone(&alloc))).await;
    let auth = start_authority(Arc::clone(&owner), None);
    let a = client_for(&auth.endpoint, "node-a").await;

    let lease = a
        .acquire(7000, None, LockMode::Exclusive, Duration::from_millis(500))
        .await
        .expect("custody granted");
    // The co-writer declares its in-flight destinations on the renewal it
    // owes anyway (the job wire's pre-allocated-destination law: the
    // authority must know which offsets a dead epoch could still be
    // writing).
    a.declare_inflight(&[offset]);
    a.renew_all().await.expect("the renewal carries the set");

    let before = quarantined();
    let dead = auth
        .owner
        .revoke_client("node-a", "operator revoke (test)")
        .into_iter()
        .next()
        .expect("the revoke produced a dead grant");
    assert_eq!(dead.offsets, vec![offset], "the declared set is the cohort");
    assert_eq!(
        quarantined(),
        before + 1,
        "the dead epoch's offset is quarantined"
    );
    assert!(alloc.is_quarantined(offset));

    // The offset is unreachable from EVERY allocation path while unproven.
    alloc.free_block(offset).await.expect("terminal free");
    for _ in 0..8 {
        if let Ok(next) = alloc.allocate_block().await {
            assert_ne!(
                next, offset,
                "a quarantined offset must never be reallocated"
            );
        }
    }

    // No proof, no release: the proof cannot even be constructed from a
    // preempt that landed nowhere.
    assert!(
        DrainProof::preempt_landed(0).is_none(),
        "a preempt that landed on zero namespaces is not a proof"
    );

    let proof = DrainProof::preempt_landed(1).expect("a landed preempt IS the proof");
    assert_eq!(
        auth.owner.release_dead(&dead, proof),
        1,
        "the proof releases the cohort"
    );
    assert!(!alloc.is_quarantined(offset));
    assert!(
        alloc
            .free_block_indices()
            .contains(&(offset / alloc.chunk_size())),
        "the deferred free-list publish happens at release, and only there"
    );

    // **Revocation is PULL-based, and that is a contract, not an accident.**
    // Until the client's next renewal it still BELIEVES it holds custody —
    // the window is bounded by its own `T_self`, at which point it
    // self-fences, and by the DEVICE, which rejects a preempted host's DMA.
    // What must never happen is the belief outliving the renewal.
    assert!(
        lease.is_held().await,
        "before its renewal the client has not yet learned — the pull-based window"
    );
    let err = a
        .renew_all()
        .await
        .expect_err("a revoked client's renewal must fail loud");
    assert!(err.to_string().to_lowercase().contains("custody"));
    assert!(
        !lease.is_held().await,
        "once the renewal answered, the belief is gone"
    );
    // And the epoch MOVED rather than the mount being poisoned: losing
    // custody costs the work authorized under it, never the process.
    assert!(!data_custody::poisoned());
    drop(lease);
}

// ---------------------------------------------------------------------------
// 5. The client misses T_self and self-fences BEFORE the owner re-grants
// ---------------------------------------------------------------------------

/// Contract (§6.7 "Two lease clocks, and the client's is stricter", S6's
/// asymmetry composed with S7's poison): the co-writer's own deadline
/// precedes the authority's TTL, so a client that cannot renew fail-stops
/// its own data custody FIRST. False-positive eviction then costs
/// availability, never divergence.
///
/// Ordering is the whole contract: at `T_self` the client is poisoned; the
/// authority may only re-grant at `T_owner`, which is strictly later.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_misses_t_self_fences_before_the_owner_may_regrant() {
    let _serial = serial();
    let _restore = restore();
    let (owner, ms) = owner_with_quarantine(None).await;
    let auth = start_authority(Arc::clone(&owner), None);
    // The co-writer runs on the SAME manual clock as the authority, so the
    // two deadlines are comparable and provable without a sleep (the S6
    // seam discipline: seams, never sleeps).
    let a = WriteCustodyClient::connect_with_clock(
        &auth.endpoint,
        SECRET,
        "node-a",
        LeaseClock::manual(Arc::clone(&ms)),
        0,
    )
    .await
    .expect("a co-writer joins on the manual clock");
    let lease = a
        .acquire(9100, None, LockMode::Exclusive, Duration::from_millis(500))
        .await
        .expect("custody granted");

    let t_self = a.t_self_deadline_ms();
    let t_owner = auth
        .owner
        .lease_deadline_ms("node-a")
        .expect("the authority tracks its member's deadline");
    assert!(
        t_self < t_owner,
        "the client's deadline ({t_self}) must be strictly earlier than the owner's ({t_owner})"
    );

    // Advance past the CLIENT's deadline but not the owner's.
    ms.store(t_self + 1, Ordering::SeqCst);
    assert!(a.self_fence_due(), "the client is past its own deadline");
    assert!(
        auth.owner.expire_due().is_empty(),
        "the authority may not expire the lease yet — its deadline is later"
    );

    let fence = a.self_fence("test: renewal never completed");
    assert!(
        fence.poisoned_data_custody,
        "a WRITER's self-fence poisons process data custody (S7)"
    );
    assert!(data_custody::poisoned());
    assert!(
        data_custody::authorize_dma(None).is_err(),
        "no DMA may land after the client fenced itself"
    );

    // NOW the owner's deadline passes: the eviction mints the dead epoch,
    // and by construction the client stopped writing before this instant.
    ms.store(t_owner + 1, Ordering::SeqCst);
    let expired = auth.owner.expire_due();
    assert_eq!(expired.len(), 1, "the authority sweeps the expired grant");
    assert_eq!(expired[0].client, "node-a");
    drop(lease);
}

// ---------------------------------------------------------------------------
// 6. Owner failover with the grace window
// ---------------------------------------------------------------------------

/// Contract (§6.7 "Recovery", owner-failure half): lock state is RAM-only
/// and is reconstructed by re-assertion. The successor bumps the durable
/// term BEFORE arming (an equal era is refused), opens a grace window that
/// admits reclaim and refuses conflicting fresh acquires, and every
/// pre-failover grant is stale by construction.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owner_failover_admits_reclaim_and_refuses_fresh_acquires() {
    let _serial = serial();
    let _restore = restore();
    let (c, _ms, clock) = clocks();
    let term = squeezefs::dlm::durable_term();

    // An equal era is refused: the successor's grants would be
    // indistinguishable from the dead authority's, so a zombie's token would
    // still dominate.
    assert!(
        WriteCustodyOwner::arm(
            "owner-b",
            term + 1,
            term + 1,
            c.clone(),
            clock.clone(),
            None
        )
        .is_err(),
        "a successor must bump the durable term before arming"
    );

    let successor_term = term + 1;
    squeezefs::dlm::adopt_durable_term(successor_term);
    let successor = WriteCustodyOwner::arm("owner-b", successor_term, term, c, clock, None)
        .expect("the successor arms in a strictly greater era");
    successor.open_grace(vec!["node-a".to_string()]);
    let auth = start_authority(Arc::clone(&successor), None);
    let a = client_for(&auth.endpoint, "node-a").await;
    let b = client_for(&auth.endpoint, "node-b").await;

    // A conflicting FRESH acquire is refused inside the window...
    let err = b
        .acquire(1234, None, LockMode::Exclusive, Duration::from_millis(150))
        .await
        .expect_err("a fresh acquire is refused inside the grace window");
    assert!(
        err.to_string().to_lowercase().contains("grace"),
        "the refusal names the window: {err}"
    );
    // ...and a RECLAIM is admitted, which is what stops failover from
    // becoming a cluster-wide forced-flush storm.
    let reclaimed = a
        .reclaim(&[1234])
        .await
        .expect("a reclaim is admitted inside the window");
    assert_eq!(reclaimed.len(), 1, "the reclaim carries a fresh-era grant");
    assert_eq!(
        squeezefs::dlm::token_term(reclaimed[0].token),
        successor_term,
        "the grant is minted in the SUCCESSOR's era"
    );
    assert!(successor.stats().reclaims >= 1);
    assert_eq!(
        successor.stats().grace_conflicts,
        1,
        "dlm_grace_conflicts counts the refused fresh acquire"
    );

    // Every prior-era authorization is stale by construction.
    let pre = CustodyEpoch::from_raw(squeezefs::dlm::compose_token(term, 7));
    assert!(
        data_custody::authorize_dma(Some(pre)).is_err(),
        "a pre-failover authorization can never submit"
    );
}

// ---------------------------------------------------------------------------
// 7. The mount arm: what it demands, and what it refuses
// ---------------------------------------------------------------------------

/// Contract (§6.9's S9 guarantee, verbatim: *"full multi-writer on PR
/// substrates; **refused on non-PR**"*): the arm refuses a
/// detection-grade substrate, naming the namespace, and takes no
/// reservation on the way out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mount_arm_refuses_a_non_pr_substrate() {
    use squeezefs::meta_backend::reservation::{
        clear_override, install_override, FakeNvmeNamespace, FakeReservationClient,
    };
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (routed, _paths) = sandbox_stamped(dir.path(), "nonpr", 1, true).await;
    let data = dir.path().join("nonpr-data");
    std::fs::File::create(&data)
        .unwrap()
        .set_len(1 << 20)
        .unwrap();
    let ns = FakeNvmeNamespace::without_pr_support();
    install_override(
        &data,
        FakeReservationClient::new(ns.clone(), "nqn-s9", "host-s9"),
    );

    let err = squeezefs::multi_writer::arm_multi_writer(
        &routed,
        std::slice::from_ref(&data),
        false,
        tokio::runtime::Handle::current(),
        None,
        // DLM S9 blocker #3's admission: no data-plane router, which is
        // admissible only because this arm refuses before it would need one
        // (and refuses LOUDLY if a claim set ever enrolls co-writers without
        // one — `tests/mw_cowriter_lane_tests.rs`).
        None,
    )
    .await
    .expect_err("multi-writer must refuse a detection-grade substrate");
    let msg = err.to_string();
    assert!(
        msg.contains(&data.display().to_string()),
        "the refusal NAMES the namespace: {msg}"
    );
    assert!(
        msg.to_lowercase().contains("reservation"),
        "and says the substrate cannot enforce: {msg}"
    );
    assert_eq!(ns.holder(), None, "a refused arm takes no reservation");
    clear_override(&data);
    shutdown(&routed).await;
}

/// Contract (ruling **D9**): the arm refuses a format that does not carry
/// the capability bits — which is EVERY volume in the field, because
/// nothing stamps them. The refusal names the missing bit and its offline
/// stamping path, so the operator knows what the Phase-8 reformat window
/// owes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mount_arm_refuses_an_unstamped_format_naming_the_bit() {
    use squeezefs::meta_backend::reservation::{
        clear_override, install_override, FakeNvmeNamespace, FakeReservationClient,
    };
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    // A FRESH format: `SuperblockV3::plan` deliberately stamps none of the
    // multi-writer bits (D9).
    let (routed, _paths) = sandbox(dir.path(), "nobit", 1).await;
    let data = dir.path().join("nobit-data");
    std::fs::File::create(&data)
        .unwrap()
        .set_len(1 << 20)
        .unwrap();
    let ns = FakeNvmeNamespace::new();
    install_override(
        &data,
        FakeReservationClient::new(ns.clone(), "nqn-s9b", "host-s9b"),
    );

    let err = squeezefs::multi_writer::arm_multi_writer(
        &routed,
        std::slice::from_ref(&data),
        false,
        tokio::runtime::Handle::current(),
        None,
        None,
    )
    .await
    .expect_err("multi-writer must refuse an unstamped format");
    let msg = err.to_string();
    assert!(
        msg.contains("bit"),
        "the refusal names the missing capability bit: {msg}"
    );
    assert_eq!(ns.holder(), None, "a refused arm takes no reservation");
    clear_override(&data);
    shutdown(&routed).await;
}

/// Stamp the capability bits S9's arm requires through the OFFLINE paths —
/// which is exactly what the Phase-8 reformat window does. Deliberately
/// not the full known-bit set: bits 8 (partitioned append) and 12 (ino
/// lanes) express TWO APPENDERS PER VOLUME, and S9's ownership granularity
/// is the volume, so every §6.2 single-appender structure still has
/// exactly one appender.
async fn stamp_capabilities(path: &Path) {
    for (what, res) in [
        ("durable-term", sb::set_durable_term_bit(path).await),
        (
            "durable-block-refcounts",
            sb::set_block_refcounts_bit(path).await,
        ),
        (
            "writer-scoped-staging",
            sb::set_writer_scoped_staging_bit(path).await,
        ),
        (
            "multi-writer-data",
            sb::set_multi_writer_data_bit(path).await,
        ),
        (
            "block-key-incarnation",
            sb::set_block_key_incarnation_bit(path).await,
        ),
        ("claim-set", sb::set_claim_set_bit(path).await),
    ] {
        res.unwrap_or_else(|e| panic!("stamping {what} failed: {e}"));
    }
}

/// Contract: the arm's capability set is exactly the six bits whose
/// absence would make a second writer unsound, and the two it deliberately
/// does NOT require are the two that express two appenders on ONE volume.
#[test]
fn the_arms_capability_set_is_the_six_bits_and_says_why() {
    let required = squeezefs::multi_writer::REQUIRED_INCOMPAT;
    for bit in [
        sb::FEATURE_INCOMPAT_KV_DURABLE_TERM,
        sb::FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS,
        sb::FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING,
        sb::FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA,
        sb::FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION,
        sb::FEATURE_INCOMPAT_KV_CLAIM_SET,
    ] {
        assert_eq!(required & bit, bit, "bit {bit:#x} must be required");
    }
    for bit in [
        sb::FEATURE_INCOMPAT_KV_PARTITIONED_APPEND,
        sb::FEATURE_INCOMPAT_KV_INO_LANES,
    ] {
        assert_eq!(
            required & bit,
            0,
            "bit {bit:#x} expresses two appenders per volume, which volume-granular \
             ownership never produces"
        );
    }
    assert_eq!(
        required & !sb::FEATURES_INCOMPAT_KNOWN,
        0,
        "the arm can never require a bit this binary does not understand"
    );
    // S9 needs NO new incompat bit: every capability it gates on was
    // already built by the §6.2 format work. The original pin here froze
    // the whole mask above bit 14 ("bit 15 and above stay free"), which
    // went red the day item 9 legitimately claimed bit 15 — the same
    // brittleness S8's census pin had, and the same fix: assert what S9
    // actually claims. S9's REQUIRED set must be a subset of the bits
    // that existed when S9 landed (0..=14), so a later stage's new bit
    // can never silently become an S9 arming requirement — and that is
    // the property worth pinning, not the global bit population.
    assert_eq!(
        required & !((1u64 << 15) - 1),
        0,
        "S9's required capability set must stay within bits 0..=14 — a later \
         stage's bit cannot silently join the arm's requirements"
    );
}

// ---------------------------------------------------------------------------
// 8. The custody epoch is ONE decision (no third door)
// ---------------------------------------------------------------------------

/// Contract (S7's own instruction to S9: *"anything needing a non-fatal
/// custody change must ADVANCE THE EPOCH, not poison it"*): losing one
/// grant advances the process's custody generation, so every authorization
/// minted under the previous generation is refused at the ONE
/// authorization point — with the `data_dma_epoch_refusals` class split —
/// while the mount stays alive and can acquire fresh custody.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn losing_a_grant_advances_the_epoch_and_never_poisons_the_mount() {
    let _serial = serial();
    let _restore = restore();
    // The shipped posture: generation 0, so the epoch IS the era's base
    // and every pre-S9 behaviour is byte-identical.
    assert_eq!(data_custody::custody_generation(), 0);
    assert_eq!(
        data_custody::current_epoch().raw(),
        squeezefs::dlm::term_base(),
        "generation 0 ⇒ the epoch is exactly S7's term base"
    );

    let stale = data_custody::authorize_dma(None).expect("healthy mount authorizes");
    let before = epoch_refusals();
    let now = data_custody::advance_custody_generation("test: grant revoked");
    assert_ne!(stale, now, "the advance moved the epoch");
    assert!(
        !data_custody::poisoned(),
        "a non-fatal custody change must NOT poison the mount"
    );
    assert!(
        data_custody::authorize_dma(Some(stale)).is_err(),
        "an authorization from the previous generation can never submit"
    );
    assert_eq!(
        epoch_refusals(),
        before + 1,
        "counted in the epoch class split, not just the parent tripwire"
    );
    // Fresh custody works immediately: this is availability preserved, not
    // a fail-stop.
    let fresh = data_custody::authorize_dma(None).expect("the mount is alive");
    data_custody::authorize_dma(Some(fresh)).expect("a fresh authorization submits");
    assert_eq!(fresh, now);
}

// ---------------------------------------------------------------------------
// 9. The publish path ships instead of refusing
// ---------------------------------------------------------------------------

/// Contract (S8's explicit hand-off: *"the FUSE daemon does not use the
/// `Metadata` trait for the publish path … wiring the daemon is YOUR
/// deliverable"*): with an armed ownership plane, the daemon's non-trait
/// publish surface on a FOREIGN-home ino executes on the owner and is
/// visible there — the layout, its size, the durable block-reference
/// ledger, the parked times, the create and the reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_publish_surface_ships_to_the_owner() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (owner_be, _p1) = sandbox(dir.path(), "own", 1).await;
    let (client_be, _p2) = sandbox(dir.path(), "cli", 1).await;

    let publish_before = publish::stats();
    let (custody, _ms) = owner_with_quarantine(None).await;
    let auth = start_authority(Arc::clone(&custody), Some(Arc::clone(&owner_be)));
    publish::install_client(publish::PublishClient::new("node-a", SECRET.to_vec()));

    // Every volume of the client's set is owned by the peer.
    let foreign: Vec<(usize, PeerOwner)> = (0..client_be.volumes.len())
        .map(|v| (v, PeerOwner::new("owner-a", &auth.endpoint)))
        .collect();
    ship::arm_ownership(OwnerMap::for_volumes(&client_be, foreign).expect("owner map"));

    // create_with_rdev_size: minted on the OWNER, and its ino is what the
    // client uses from here on.
    let inode =
        publish::create_with_rdev_size(&client_be, 1, "shipped", libc::S_IFREG | 0o644, 0, 0, 0, 0)
            .await
            .expect("the create ships");
    assert!(inode.ino >= 2);

    // xattr_value_cap is a routed READ of the owner's geometry (the
    // layout-inline ceiling depends on it, so a wrong answer would corrupt
    // the caller's own sizing decision).
    let cap = publish::xattr_value_cap(&client_be, inode.ino)
        .await
        .expect("the cap ships");
    assert!(cap > 0 && cap as u64 <= u32::MAX as u64);

    // The layout publish, the delta publish and the reference ledger.
    let layout = b"layout:v1:shipped".to_vec();
    publish::set_layout_and_size(&client_be, inode.ino, &layout, 4096, &[])
        .await
        .expect("set_layout_and_size ships");
    let delta = squeezefs::layout_wire::LayoutDelta::from_final_state(
        "striped",
        8192,
        None,
        Some("be://data"),
        None,
        None,
        vec![(0, "be://data:0".to_string())],
    );
    publish::merge_layout_and_size(
        &client_be,
        inode.ino,
        &delta,
        bytes::Bytes::from(layout.clone()),
        8192,
        Vec::new(),
    )
    .await
    .expect("merge_layout_and_size ships");
    publish::commit_block_refs(&client_be, inode.ino, &[])
        .await
        .expect("commit_block_refs ships");
    publish::park_write_times(&client_be, inode.ino, 111, 222)
        .await
        .expect("park_write_times ships");

    // The owner's own view proves the ops LANDED there, not locally.
    let on_owner = {
        use squeezefs::meta_backend::Metadata;
        owner_be.getattr(inode.ino).await.expect("the owner has it")
    };
    assert_eq!(on_owner.size, 8192, "the shipped size is the owner's truth");

    // readdir_stream is the routed paged read the daemon uses.
    let page = publish::readdir_stream(&client_be, 1, 0, 16)
        .await
        .expect("readdir_stream ships");
    assert!(
        page.iter().any(|(_, e)| e.name == "shipped"),
        "the owner's directory page carries the shipped create"
    );

    // destroy_inodes is the batched multi-ino teardown.
    publish::destroy_inodes(&client_be, &[inode.ino])
        .await
        .expect("destroy_inodes ships");

    let s = publish::stats();
    assert!(
        s.shipped - publish_before.shipped >= 8,
        "every publish verb took the wire: {s:?}"
    );
    assert_eq!(
        s.local, publish_before.local,
        "a foreign-home ino never publishes locally"
    );
    assert_eq!(s.refusals, publish_before.refusals);
    assert_eq!(s.panics, 0, "an owner-side publish must never unwind");

    // Disarmed, the SAME calls take today's path with no session at all.
    ship::disarm_ownership();
    let before = publish::stats();
    publish::park_write_times(&client_be, 1, 1, 1)
        .await
        .expect("the local path is unchanged");
    let after = publish::stats();
    assert_eq!(after.shipped, before.shipped, "nothing shipped");
    assert_eq!(after.local, before.local + 1, "it took the local path");

    shutdown(&owner_be).await;
    shutdown(&client_be).await;
}

/// Contract: an armed ownership plane with NO publish client installed
/// must REFUSE a foreign-home publish loud — never silently execute it
/// locally, which is precisely the silent-divergence bug the S4 refusal
/// exists to prevent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_foreign_publish_without_a_client_refuses_rather_than_going_local() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let (client_be, _p) = sandbox(dir.path(), "noclient", 1).await;
    let foreign: Vec<(usize, PeerOwner)> = (0..client_be.volumes.len())
        .map(|v| (v, PeerOwner::new("owner-a", "127.0.0.1:1")))
        .collect();
    ship::arm_ownership(OwnerMap::for_volumes(&client_be, foreign).expect("owner map"));

    let err = publish::park_write_times(&client_be, 5, 1, 1)
        .await
        .expect_err("a foreign publish with no client must refuse");
    assert!(
        err.to_string().contains("S9"),
        "the refusal names the stage that owns the seam: {err}"
    );
    assert!(publish::stats().refusals >= 1);
    shutdown(&client_be).await;
}

// ---------------------------------------------------------------------------
// 10. S4's foreign-home refusal becomes a remote acquire
// ---------------------------------------------------------------------------

/// Contract (S4's own words: *"`is_local_slot` is the ownership extension
/// point, and a foreign home is currently a LOUD REFUSAL at every
/// acquire… Replacing that refusal with real remote custody is the heart
/// of S9"*): with the custody client armed, a foreign-home acquire travels
/// to the owner and returns real custody; with it disarmed the refusal is
/// exactly S4's, unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreign_home_acquire_ships_instead_of_refusing() {
    let _serial = serial();
    let _restore = restore();
    let (owner, _ms) = owner_with_quarantine(None).await;
    let auth = start_authority(Arc::clone(&owner), None);

    let dlm = squeezefs::dlm_slot::SlotLockManager::new().expect("the slot lock manager");
    // No slot is local: every acquire homes foreign.
    squeezefs::dlm_slot::test_set_local_slots(Some(&[]));

    // Unarmed: S4's refusal, verbatim — no node grants custody its owner
    // never issued.
    let err = dlm
        .acquire_lock("inode_600", None, Duration::from_millis(100))
        .await
        .expect_err("without a custody client a foreign home refuses");
    assert!(
        err.to_string().contains("S9") || err.to_string().contains("does not own"),
        "the refusal explains the missing half: {err}"
    );

    // Armed: the same acquire ships and returns custody.
    let client = client_for(&auth.endpoint, "node-a").await;
    data_grant::install_custody_client(Arc::clone(&client));
    let rpcs_before = squeezefs::dlm_slot::dlm_rpcs();
    let lease = dlm
        .acquire_lock("inode_600", None, Duration::from_millis(500))
        .await
        .expect("a foreign home now ships to its owner");
    assert!(lease.is_held().await);
    assert_eq!(
        squeezefs::dlm_slot::dlm_rpcs(),
        rpcs_before + 1,
        "dlm_rpcs counts the LOCK round trip it always named"
    );
    assert_eq!(auth.owner.held(), 1);
    // The fencing read of a foreign object served through the same grant:
    // the owner's token, never a locally invented one.
    assert_eq!(
        dlm.get_fencing_token_ino(600),
        lease.fencing_token(),
        "the adopted grant IS this object's readable generation here"
    );

    drop(lease);
    client.drain_releases().await;
    squeezefs::dlm_slot::test_set_local_slots(None);
}

// ---------------------------------------------------------------------------
// 11. Concurrency: the storm
// ---------------------------------------------------------------------------

/// Contract: under a multi-threaded storm of overlapping and disjoint
/// range acquires from several co-writers, the authority never grants
/// incompatible overlapping custody, every refusal is counted, and no
/// offset leaves the quarantine without a proof.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_custody_never_grants_overlapping_spans() {
    let _serial = serial();
    let _restore = restore();
    let (owner, _ms) = owner_with_quarantine(None).await;
    let auth = start_authority(Arc::clone(&owner), None);
    let quarantined_before = quarantined();
    const INO: u64 = 31337;
    const LANES: u64 = 8;

    let mut tasks = Vec::new();
    for lane in 0..LANES {
        let endpoint = auth.endpoint.clone();
        tasks.push(tokio::spawn(async move {
            let c = WriteCustodyClient::connect(&endpoint, SECRET, &format!("node-{lane}"))
                .await
                .expect("dial");
            let mut held = 0u32;
            for round in 0..4u64 {
                // Half the lanes ask for their OWN disjoint megabyte; half
                // ask for a span that overlaps lane 0's.
                let span = if lane % 2 == 0 {
                    (lane << 20, (lane + 1) << 20)
                } else {
                    (0, 1 << 20)
                };
                match c
                    .acquire(
                        INO,
                        Some(span),
                        LockMode::Exclusive,
                        Duration::from_millis(50),
                    )
                    .await
                {
                    Ok(lease) => {
                        held += 1;
                        // Hold it across an await point, then release.
                        tokio::task::yield_now().await;
                        assert!(lease.fencing_token() > 0, "round {round} minted a token");
                        drop(lease);
                        c.drain_releases().await;
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        assert!(
                            !msg.is_empty(),
                            "every refusal must carry its reason (round {round})"
                        );
                    }
                }
            }
            held
        }));
    }
    let mut granted = 0u32;
    for t in tasks {
        granted += t.await.expect("no lane panicked");
    }
    assert!(granted > 0, "the storm made progress");
    assert_eq!(
        auth.owner.held(),
        0,
        "every grant this storm took was retired"
    );
    let s = auth.owner.stats();
    assert_eq!(s.granted - s.released, 0, "the grant ledger closes: {s:?}");
    assert_eq!(
        quarantined(),
        quarantined_before,
        "a clean storm quarantines nothing"
    );
}

/// Contract: the whole stack composes under load — two co-writers, one
/// authority, disjoint files, concurrent renewals, and one revoke in the
/// middle. The revoked client's authorizations die; the survivor's keep
/// working (custody loss is per-client, never a cluster-wide stop).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_revoke_under_load_stops_one_writer_and_not_the_other() {
    let _serial = serial();
    let _restore = restore();
    let alloc = Arc::new(BlockAllocator::new("s9-load").await.expect("allocator"));
    alloc.set_capacity_bytes(256 * alloc.chunk_size());
    let (owner, _ms) = owner_with_quarantine(Some(Arc::clone(&alloc))).await;
    let auth = start_authority(Arc::clone(&owner), None);

    let a = client_for(&auth.endpoint, "node-a").await;
    let b = client_for(&auth.endpoint, "node-b").await;
    let la = a
        .acquire(11, None, LockMode::Exclusive, Duration::from_millis(500))
        .await
        .expect("a");
    let lb = b
        .acquire(22, None, LockMode::Exclusive, Duration::from_millis(500))
        .await
        .expect("b");

    let off = alloc.allocate_block().await.expect("destination");
    a.declare_inflight(&[off]);
    a.renew_all().await.expect("renew a");
    b.renew_all().await.expect("renew b");

    let dead = auth.owner.revoke_client("node-a", "load test revoke");
    assert_eq!(dead.len(), 1);
    assert!(alloc.is_quarantined(off));

    // node-b is untouched: its renewal still succeeds and its custody is
    // still live. node-a learns at ITS renewal (the pull-based window) —
    // per-client custody loss, never a cluster-wide stop.
    b.renew_all().await.expect("the survivor renews");
    assert!(lb.is_held().await, "the survivor's custody is live");
    assert!(
        a.renew_all().await.is_err(),
        "the revoked client's renewal must fail"
    );
    assert!(!la.is_held().await, "the revoked client's custody is gone");
    assert!(lb.is_held().await, "and the survivor is STILL live");

    let proof = DrainProof::proven_dead("test: attested proof of death");
    assert_eq!(auth.owner.release_dead(&dead[0], proof), 1);
    drop(la);
    drop(lb);
}
