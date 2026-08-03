//! DLM **stage S7** — the data-plane custody-epoch fence, the dead-epoch
//! allocation quarantine, and WERO on data namespaces
//! (`docs/pre-rc-engineering-spec.md` §6.9 S7 row, §6.7 "Recovery",
//! §7 **RES-6**, risks **R2**/**R7**).
//!
//! ## What S7 adds to what already shipped
//!
//! RES-6's LOCAL face landed with `tests/dma_fence_gate_tests.rs`: one
//! relaxed load per submit against the D0 writer guard's per-volume
//! `failed` latch, refusing `WriterGuardFenced` and counting
//! `data_dma_fence_refusals`. That is a **boolean** fence on ONE device
//! handle: it answers "is this device's probe latched *now*", which is
//! exactly the wrong question the moment custody can move. A DMA
//! authorized under custody epoch E and submitted after the mount lost E
//! passes a boolean gate whose probe has not fired yet (a sibling
//! volume's latch, a submission racing the latch, an S9 remote client
//! whose grant died) and lands on offsets the successor writer has
//! already replayed and reallocated.
//!
//! S7 therefore makes the fence **epoch-bearing**: a submission carries
//! the custody epoch it was authorized under, and ONE authorization point
//! (`data_custody::authorize_dma`, which the device submit gate and every
//! epoch carrier call) refuses anything whose epoch is no longer current.
//! Plus the two mechanisms that make a dead epoch safe:
//!
//! * **Dead-epoch allocation quarantine** — blocks belonging to a dead
//!   epoch enter a do-not-reallocate quarantine until the epoch is proven
//!   drained; the job wire's expired-lease destination quarantine applied
//!   verbatim (§6.7 "Recovery"), pushed DOWN into the allocator so the
//!   fresh-destination law is structural instead of asserted.
//! * **WERO on data namespaces** — Write Exclusive – Registrants Only
//!   (rtype 2) held for the mount lifetime, so a zombie's DMA is rejected
//!   by the DEVICE and not merely by its own latch. Multi-writer
//!   **refuses to arm** where the substrate cannot enforce it (§6.7 "On
//!   external consensus": that includes the repo's own loop substrate).
//!
//! ## Contracts pinned here
//!
//! 1. **Epoch-stale DMA is refused at the authorization point**, loudly
//!    (`WriterGuardFenced`), counted in `data_dma_fence_refusals` (THE
//!    tripwire) with the class split `data_dma_epoch_refusals`.
//! 2. **A healthy mount never refuses**: repeated authorize+submit cycles
//!    move neither counter (the must-stay-0 property).
//! 3. **Poison is process-wide and sticky**: the first device latch
//!    observation retires custody for the whole process, so a sibling
//!    device with no probe of its own refuses too — and it never clears
//!    in-process (a fenced holder is dead until remount).
//! 4. **Quarantine admission/release with the drain proof**: a
//!    quarantined offset is never handed out by any allocation path, and
//!    `release_quarantine` (the drain proof) is the ONLY thing that
//!    returns it — gauged by `dlm_quarantined_offsets` /
//!    `dlm_quarantine_releases`.
//! 5. **A quarantined offset's free never publishes**: `finish_free`
//!    defers the free-list publish while quarantined (no double free, no
//!    reallocation), and the deferred publish happens at release.
//! 6. **Under space pressure the verdict is ENOSPC, never a deadlock**:
//!    with every free block quarantined, allocation refuses
//!    `StorageFull` honestly and promptly. Handing a possibly-live
//!    zombie's offset to a new owner is data corruption; a forced drain
//!    of the quarantine would be exactly that. ENOSPC is the correct
//!    answer and the drain proof is the recovery act.
//! 7. **Fence-halt composition with the reclaim queue**: a fenced queue
//!    still drops entries WITHOUT `finish_free`
//!    (`block_free_reclaim_fence_halts`), and a quarantined offset whose
//!    reclaim DID complete still is not reallocatable.
//! 8. **Non-PR substrate refuses multi-writer arming, loudly and
//!    specifically** (naming the namespace path), while the single-writer
//!    posture degrades to detection grade without refusing; a PR-capable
//!    substrate whose format lacks the S7 incompat bit refuses too
//!    (nothing stamps it — ruling D9).
//! 9. **The WERO hold is shared, not forked**: the mount's hold and the
//!    job wire's first-enrollment hold compose on ONE key (a second
//!    acquire would otherwise conflict at the device and silently
//!    downgrade the job wire's guarantee class), and the last release
//!    leaves zero residue.
//! 10. **Incompat bit 11 is disjoint** from every other feature bit.
//!
//! RED against dev 2961ab53: `squeezefs::data_custody` does not exist,
//! `NvmeBlockDev::write_block_authorized` does not exist, the allocator
//! has no quarantine, and there is no incompat bit 11.
//!
//! ## What is NOT pinned here (and cannot be)
//!
//! Device REJECTION of a fenced holder's DMA is a property of a real
//! PR-capable namespace. The deferred device leg — a stop/resume-past-TTL
//! cycle on both target stacks showing the device (not the latch) refuse
//! — is specified in `docs/design-nvmeof-target-management.md` §6.8 under
//! *S7 data-plane custody fence*, to be run by
//! `tests/run_nvmeof_fidelity.sh` when the deferred stack runs. In-process
//! we pin the decision, the counting, the composition and the refusals.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::block_reclaim::{ReclaimEntry, ReclaimQueue};
use squeezefs::data_custody::{self, declare_dead_epoch, CustodyPosture, DeadEpoch};
use squeezefs::error::SqueezefsError;
use squeezefs::fuse_client::METRICS;
use squeezefs::meta_backend::kv::superblock::{
    FEATURES_INCOMPAT_KNOWN, FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS, FEATURE_INCOMPAT_KV_CLAIM_SET,
    FEATURE_INCOMPAT_KV_DURABLE_TERM,
    FEATURE_INCOMPAT_KV_DYNAMIC_ROUTING, FEATURE_INCOMPAT_KV_GUEST_SLOTS,
    FEATURE_INCOMPAT_KV_LAYOUT_DELTAS, FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA,
    FEATURE_INCOMPAT_KV_PARTITIONED_APPEND, FEATURE_INCOMPAT_KV_SLOT_MIGRATION,
    FEATURE_INCOMPAT_KV_V3, FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE,
    FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING, FEATURE_INCOMPAT_NODE_SEQ_WATERMARK,
};
use squeezefs::meta_backend::reservation::{
    clear_override, install_override, FakeNvmeNamespace, FakeReservationClient,
};
use squeezefs::nvme_dev::NvmeBlockDev;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tempfile::NamedTempFile;

/// Tests that touch PROCESS-GLOBAL custody state (the poison latch, the
/// durable term, the WERO registry, the quarantine gauges) serialize on
/// this — libtest runs a file's tests on threads, and the gate's
/// `--test-threads=1` bounds files, not tests within one.
///
/// An atomic latch rather than a `Mutex`: these are async tests, the guard
/// is deliberately held across `.await` points (that is the whole point of
/// serializing them), and unwinding releases it.
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

/// Clears the sticky poison latch when a test that fenced the process
/// finishes (production has no clear path — a fenced holder is dead until
/// remount; the seam exists exactly so one test can fence without ending
/// the binary).
struct PoisonGuard;
impl Drop for PoisonGuard {
    fn drop(&mut self) {
        data_custody::test_clear_poison();
    }
}

fn refusals() -> u64 {
    METRICS.data_dma_fence_refusals.load(Ordering::Relaxed)
}

fn epoch_refusals() -> u64 {
    METRICS.data_dma_epoch_refusals.load(Ordering::Relaxed)
}

fn quarantined() -> u64 {
    METRICS.dlm_quarantined_offsets.load(Ordering::Relaxed)
}

fn quarantine_releases() -> u64 {
    METRICS.dlm_quarantine_releases.load(Ordering::Relaxed)
}

fn fence_halts() -> u64 {
    METRICS
        .block_free_reclaim_fence_halts
        .load(Ordering::Relaxed)
}

fn backing(bytes: u64) -> (NamedTempFile, NvmeBlockDev) {
    let f = NamedTempFile::new().expect("backing file");
    std::fs::File::create(f.path())
        .unwrap()
        .set_len(bytes)
        .unwrap();
    let dev = NvmeBlockDev::new(f.path().to_str().unwrap());
    (f, dev)
}

async fn allocator(id: &str) -> Arc<BlockAllocator> {
    Arc::new(BlockAllocator::new(id).await.expect("allocator"))
}

/// Advance the process's custody epoch the way production does: adopt a
/// HIGHER durable writer term (DLM S2 — the successor's term bump is what
/// makes every earlier-era authorization stale by construction, spec
/// §6.7 decision 4). Monotone, so tests must climb.
fn advance_custody_epoch() {
    let next = squeezefs::dlm::durable_term() + 1;
    squeezefs::dlm::adopt_durable_term(next);
}

// ---------------------------------------------------------------------------
// 1 + 2: the authorization point
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn epoch_stale_dma_is_refused_at_the_authorization_point() {
    let _serial = serial();
    let (_f, dev) = backing(16 * 1024 * 1024);
    let payload = bytes::Bytes::from(vec![0x5Au8; 4096]);

    // Authorize under the CURRENT epoch and submit: lands, nothing counted.
    let auth = data_custody::authorize_dma(None).expect("healthy mount authorizes");
    let before = refusals();
    let before_epoch = epoch_refusals();
    dev.write_block_authorized(0, payload.clone(), auth)
        .await
        .expect("a current-epoch submission must land");
    assert_eq!(refusals(), before, "no refusal on a healthy mount");
    assert_eq!(epoch_refusals(), before_epoch);

    // Custody moves (the successor bumped the durable term): the SAME
    // authorization is now stale, and the submission is refused at the
    // authorization point — before any device work.
    advance_custody_epoch();
    assert_ne!(
        data_custody::current_epoch(),
        auth,
        "a term bump must advance the custody epoch"
    );
    let err = dev
        .write_block_authorized(4096, payload.clone(), auth)
        .await
        .expect_err("S7: an epoch-stale DMA must be refused");
    assert!(
        matches!(err, SqueezefsError::WriterGuardFenced),
        "the refusal must be loud and classifiable, got {err:?}"
    );
    assert_eq!(
        refusals(),
        before + 1,
        "every refusal counts in the data_dma_fence_refusals tripwire"
    );
    assert_eq!(
        epoch_refusals(),
        before_epoch + 1,
        "the epoch class is split out of the latch class"
    );

    // The device itself is NOT fenced: a freshly authorized submission
    // still lands (an epoch-stale carrier is a stale CARRIER, not a dead
    // mount — the distinction the boolean latch cannot express).
    let fresh = data_custody::authorize_dma(None).expect("still healthy");
    dev.write_block_authorized(8192, payload.clone(), fresh)
        .await
        .expect("a re-authorized submission lands");
    assert!(!dev.fenced(), "an epoch refusal must not latch the device");

    // Reads are never gated (refusing them turns a fail-stop into a hang).
    let got = dev.read_block(0, 4096).await.expect("reads are open");
    assert_eq!(&got[..], &payload[..]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_healthy_mount_never_refuses_dma() {
    let _serial = serial();
    let (_f, dev) = backing(16 * 1024 * 1024);
    let before = refusals();
    let before_epoch = epoch_refusals();
    for i in 0..64u64 {
        let auth = data_custody::authorize_dma(None).expect("healthy");
        dev.write_block_authorized(i * 4096, bytes::Bytes::from(vec![1u8; 4096]), auth)
            .await
            .expect("healthy submit");
    }
    // The unauthorized-carrier form (every legacy call site) authorizes at
    // submit and must be equally silent.
    for i in 0..64u64 {
        dev.write_block(i * 4096, bytes::Bytes::from(vec![2u8; 4096]))
            .await
            .expect("healthy submit");
    }
    assert_eq!(
        (refusals(), epoch_refusals()),
        (before, before_epoch),
        "must-stay-0: a healthy mount refuses nothing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fence_poison_is_process_wide_and_sticky() {
    let _serial = serial();
    let _poison = PoisonGuard;
    let (_fa, dev_a) = backing(8 * 1024 * 1024);
    let (_fb, dev_b) = backing(8 * 1024 * 1024);
    // Only device A carries a D0 probe. Device B is the sibling volume
    // whose own probe has not fired (or was never wired) — the shape the
    // per-device boolean latch got wrong.
    let fenced = Arc::new(AtomicBool::new(false));
    let probe = fenced.clone();
    dev_a.set_fence_signal(Arc::new(move || probe.load(Ordering::Relaxed)));

    let auth = data_custody::authorize_dma(None).expect("healthy");
    let before = refusals();
    fenced.store(true, Ordering::Relaxed);

    // A's latch fires and RETIRES process custody.
    let err = dev_a
        .write_block(0, bytes::Bytes::from(vec![3u8; 4096]))
        .await
        .expect_err("the fenced device refuses");
    assert!(matches!(err, SqueezefsError::WriterGuardFenced));
    assert!(
        data_custody::poisoned(),
        "the fence poisons process custody"
    );

    // B has no probe at all and must refuse anyway.
    let err = dev_b
        .write_block(0, bytes::Bytes::from(vec![4u8; 4096]))
        .await
        .expect_err("a sibling device must observe the process fence");
    assert!(matches!(err, SqueezefsError::WriterGuardFenced));

    // Every carrier is stale, and no new authorization is minted.
    assert!(matches!(
        dev_b
            .write_block_authorized(0, bytes::Bytes::from(vec![5u8; 4096]), auth)
            .await,
        Err(SqueezefsError::WriterGuardFenced)
    ));
    assert!(matches!(
        data_custody::authorize_dma(None),
        Err(SqueezefsError::WriterGuardFenced)
    ));
    assert!(refusals() >= before + 4, "every refusal counted");

    // Sticky: the probe going quiet changes nothing.
    fenced.store(false, Ordering::Relaxed);
    assert!(
        data_custody::poisoned(),
        "a fenced holder is dead until remount"
    );
}

// ---------------------------------------------------------------------------
// 4 + 5: dead-epoch allocation quarantine
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quarantine_admission_and_release_need_a_drain_proof() {
    // The `dlm_quarantined_offsets` / `dlm_quarantine_releases` gauges are
    // process-global: their DELTAS are only readable serialized.
    let _serial = serial();
    let ba = allocator("s7-quarantine-admit").await;
    // Four blocks allocated then freed: all four are reallocatable.
    let mut offs = Vec::new();
    for _ in 0..4 {
        offs.push(ba.allocate_block().await.expect("allocate"));
    }
    for o in &offs {
        ba.free_block(*o).await.expect("free");
    }

    let dead: DeadEpoch = declare_dead_epoch("s7 test: client epoch TTL fired");
    let q0 = quarantined();
    assert!(
        ba.quarantine_offset(offs[0], dead),
        "first admission is new"
    );
    assert!(
        !ba.quarantine_offset(offs[0], dead),
        "re-admission is idempotent"
    );
    assert!(ba.quarantine_offset(offs[1], dead));
    assert_eq!(quarantined(), q0 + 2, "the live gauge counts admissions");
    assert!(ba.is_quarantined(offs[0]) && ba.is_quarantined(offs[1]));

    // Drain the whole free list: a quarantined offset can never come out
    // of ANY allocation path.
    let mut handed = Vec::new();
    for _ in 0..4 {
        let Ok(o) = ba.allocate_block().await else {
            break;
        };
        handed.push(o);
    }
    for o in &handed {
        assert!(
            !ba.is_quarantined(*o),
            "allocator handed out quarantined offset {o} — the fresh-destination law"
        );
    }
    assert!(
        handed.contains(&offs[2]) && handed.contains(&offs[3]),
        "un-quarantined offsets stay allocatable"
    );

    // The drain proof releases them; nothing else does.
    let rel0 = quarantine_releases();
    assert_eq!(
        ba.release_quarantine(dead),
        2,
        "the drain proof releases exactly this epoch's offsets"
    );
    assert_eq!(quarantine_releases(), rel0 + 2);
    assert_eq!(quarantined(), q0, "the live gauge closes back to its floor");
    assert!(!ba.is_quarantined(offs[0]) && !ba.is_quarantined(offs[1]));

    // And now — only now — they are allocatable again.
    let mut after = Vec::new();
    for _ in 0..2 {
        after.push(ba.allocate_block().await.expect("post-proof allocate"));
    }
    after.sort_unstable();
    let mut want = vec![offs[0], offs[1]];
    want.sort_unstable();
    assert_eq!(after, want, "released offsets return to the free list");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quarantined_offsets_free_never_publishes_until_the_proof() {
    let ba = allocator("s7-quarantine-free").await;
    let live = ba.allocate_block().await.expect("allocate");
    let dead = declare_dead_epoch("s7 test: dead epoch owns a LIVE offset");
    // Quarantining an offset a live owner holds records the gate without
    // touching the owner: the entry only ever defers the FUTURE free.
    assert!(ba.quarantine_offset(live, dead));
    let doubles0 = METRICS.block_double_frees.load(Ordering::Relaxed);

    // The dead epoch's block is freed by recovery/cleanup: the free
    // window completes, but the free-list PUBLISH is deferred.
    ba.free_block(live).await.expect("free");
    assert!(
        ba.is_quarantined(live),
        "the free must not clear the quarantine"
    );
    assert!(
        !ba.free_block_indices().contains(&(live / ba.chunk_size())),
        "a quarantined offset must never enter the free list"
    );
    // Nothing else can hand it out.
    for _ in 0..4 {
        let o = ba.allocate_block().await.expect("allocate");
        assert_ne!(o, live, "a quarantined offset was reallocated");
    }

    // The proof publishes the deferred free.
    assert_eq!(ba.release_quarantine(dead), 1);
    assert!(
        ba.free_block_indices().contains(&(live / ba.chunk_size())),
        "the deferred free publishes at the drain proof"
    );
    assert_eq!(
        METRICS.block_double_frees.load(Ordering::Relaxed),
        doubles0,
        "the deferred publish is not a second free"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quarantine_under_space_pressure_refuses_enospc_and_never_deadlocks() {
    let ba = allocator("s7-quarantine-pressure").await;
    // Four blocks of capacity, all allocated, all freed, all quarantined.
    ba.set_capacity_bytes(4 * ba.chunk_size());
    let mut offs = Vec::new();
    for _ in 0..4 {
        offs.push(ba.allocate_block().await.expect("allocate"));
    }
    for o in &offs {
        ba.free_block(*o).await.expect("free");
    }
    let dead = declare_dead_epoch("s7 test: the whole free list is a dead epoch's");
    for o in &offs {
        assert!(ba.quarantine_offset(*o, dead));
    }

    // The RULING: ENOSPC, promptly — never a forced drain of the
    // quarantine (that would hand a possibly-live zombie's offset to a
    // new owner: silent cross-writer corruption), and never a wait for a
    // proof that only recovery can produce.
    let t0 = std::time::Instant::now();
    let err = ba
        .allocate_block()
        .await
        .expect_err("a fully quarantined store must refuse");
    assert!(
        matches!(&err, SqueezefsError::Io(io) if io.kind() == std::io::ErrorKind::StorageFull),
        "the verdict is StorageFull, got {err:?}"
    );
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(5),
        "the refusal must be prompt — a quarantine that can deadlock allocation is a bug"
    );

    // The drain proof is the recovery act that clears the pressure.
    assert_eq!(ba.release_quarantine(dead), 4);
    ba.allocate_block()
        .await
        .expect("post-proof allocation succeeds");
}

// ---------------------------------------------------------------------------
// 7: composition with the async reclaim queue
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quarantine_composes_with_the_reclaim_fence_halt() {
    let ba = allocator("s7-quarantine-reclaim").await;
    let (_f, _dev) = backing(16 * 1024 * 1024);
    let offset = ba.allocate_block().await.expect("allocate");
    let dead = declare_dead_epoch("s7 test: reclaim composition");
    assert!(ba.quarantine_offset(offset, dead));

    // A FENCED queue drops entries WITHOUT finish_free (the successor's
    // recovery owns the accounting) — unchanged by S7, and the offset
    // stays unallocatable either way.
    let q = ReclaimQueue::from_env();
    q.set_fence_signal(Arc::new(|| true));
    let halts0 = fence_halts();
    assert!(ba.begin_free(offset), "terminal free");
    let inflight = ba.inflight_register(offset);
    q.enqueue(ReclaimEntry {
        allocator: ba.clone(),
        inflight,
        device_path: "/dev/null".to_string(),
        offset,
        size: ba.chunk_size(),
    })
    .await;
    // A fenced batch is POPPED and DROPPED (the entry count is what the
    // drain reports), but no device command is issued and no `finish_free`
    // runs — the successor's recovery owns the accounting.
    assert_eq!(q.drain_off_thread().await, 1, "the entry was consumed");
    assert!(fence_halts() > halts0, "the halt is counted");
    assert!(
        !ba.free_block_indices()
            .contains(&(offset / ba.chunk_size())),
        "no finish_free without a reclaim"
    );
    assert!(
        ba.is_quarantined(offset),
        "the quarantine outlives the halt"
    );

    // Even a COMPLETED reclaim cannot make it reallocatable — only the
    // drain proof can (the two gates are independent by design).
    let q2 = ReclaimQueue::from_env();
    let inflight2 = ba.inflight_register(offset);
    q2.enqueue(ReclaimEntry {
        allocator: ba.clone(),
        inflight: inflight2,
        device_path: "/dev/null".to_string(),
        offset,
        size: ba.chunk_size(),
    })
    .await;
    let _ = q2.drain_off_thread().await;
    assert!(
        !ba.free_block_indices()
            .contains(&(offset / ba.chunk_size())),
        "a quarantined offset's completed reclaim still defers the publish"
    );
    assert_eq!(ba.release_quarantine(dead), 1);
    assert!(ba
        .free_block_indices()
        .contains(&(offset / ba.chunk_size())));
}

// ---------------------------------------------------------------------------
// 8 + 9: WERO on data namespaces
// ---------------------------------------------------------------------------

#[test]
fn multi_writer_refuses_to_arm_on_a_non_pr_substrate() {
    let _serial = serial();
    let path = std::path::PathBuf::from(format!(
        "/tmp/squeezefs-s7-nonpr-{}-{}",
        std::process::id(),
        line!()
    ));
    let ns = FakeNvmeNamespace::without_pr_support();
    install_override(
        &path,
        FakeReservationClient::new(ns.clone(), "nqn-s7", "host-s7"),
    );

    // Multi-writer: REFUSED, loudly, naming the substrate (§6.7 "On
    // external consensus" — the repo's own loop substrate is exactly this
    // shape).
    let err = data_custody::arm_data_plane(
        CustodyPosture::MultiWriter,
        std::slice::from_ref(&path),
        true,
    )
    .expect_err("multi-writer must refuse a detection-grade substrate");
    let msg = err.to_string();
    assert!(
        msg.contains(&path.display().to_string()),
        "the refusal must NAME the namespace: {msg}"
    );
    assert!(
        msg.contains("multi-writer") && msg.contains("reservation"),
        "the refusal must say what it refused and why: {msg}"
    );

    // Single-writer on the same substrate: no refusal — the D0 guard
    // already governs and the documented class is detection grade.
    let hold = data_custody::arm_data_plane(
        CustodyPosture::SingleWriter,
        std::slice::from_ref(&path),
        false,
    )
    .expect("single-writer never refuses on a non-PR substrate");
    assert!(
        hold.is_none(),
        "no WERO hold on a detection-grade substrate"
    );
    assert_eq!(data_custody::wero_mode(), "detection");
    clear_override(&path);
}

#[test]
fn multi_writer_refuses_a_format_without_the_s7_incompat_bit() {
    let _serial = serial();
    let path = std::path::PathBuf::from(format!(
        "/tmp/squeezefs-s7-nobit-{}-{}",
        std::process::id(),
        line!()
    ));
    let ns = FakeNvmeNamespace::new();
    install_override(
        &path,
        FakeReservationClient::new(ns.clone(), "nqn-s7b", "host-s7b"),
    );
    // PR-capable, but the format does not carry bit 11 — which is EVERY
    // volume today (ruling D9: the bit is built, never stamped).
    let err = data_custody::arm_data_plane(
        CustodyPosture::MultiWriter,
        std::slice::from_ref(&path),
        false,
    )
    .expect_err("multi-writer must refuse an unstamped format");
    let msg = err.to_string();
    assert!(
        msg.contains("multi-writer") && msg.contains("format"),
        "the refusal must name the missing format capability: {msg}"
    );
    assert_eq!(ns.holder(), None, "a refused arm takes no reservation");
    clear_override(&path);
}

#[test]
fn the_data_plane_wero_hold_is_shared_not_forked() {
    let _serial = serial();
    let path = std::path::PathBuf::from(format!(
        "/tmp/squeezefs-s7-wero-{}-{}",
        std::process::id(),
        line!()
    ));
    let ns = FakeNvmeNamespace::new();
    install_override(
        &path,
        FakeReservationClient::new(ns.clone(), "nqn-s7c", "host-s7c"),
    );
    let paths = vec![path.clone()];

    // The mount arms (multi-writer posture, stamped format): WERO held,
    // guarantee class pr.
    let mount_hold = data_custody::arm_data_plane(CustodyPosture::MultiWriter, &paths, true)
        .expect("PR-capable + stamped arms")
        .expect("a WERO hold");
    let key = ns.holder().expect("WERO reservation held");
    assert_eq!(data_custody::wero_mode(), "pr");
    assert_eq!(METRICS.data_plane_fence_mode.load(Ordering::Relaxed), 1);

    // A registered host writes; an unregistered one is device-rejected —
    // the enforcement class the local latch cannot provide.
    assert!(ns.write_allowed(b"host-s7c"), "the registrant writes");
    assert!(
        !ns.write_allowed(b"host-stranger"),
        "an unregistered host is device-rejected under WERO"
    );

    // The job wire's first-enrollment hold must COMPOSE with the mount's,
    // not fork a second reservation (a second key would conflict at the
    // device and silently downgrade the job wire to deferred-reclaim).
    let wire_hold = data_custody::acquire_wero(&paths).expect("the shared hold");
    assert_eq!(
        ns.holder(),
        Some(key),
        "the second acquirer joins the SAME hold"
    );
    drop(wire_hold);
    assert_eq!(
        ns.holder(),
        Some(key),
        "the reservation stands while any holder remains"
    );

    // Last release: zero residue.
    drop(mount_hold);
    assert_eq!(ns.holder(), None, "the last release drops the reservation");
    assert!(!ns.is_registered(key), "and its registration with it");
    assert_eq!(data_custody::wero_mode(), "detection");
    clear_override(&path);
}

#[test]
fn incompat_bit_11_is_disjoint_from_every_other_feature_bit() {
    assert_eq!(
        FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA,
        1 << 11,
        "S7 takes bit 11 (0..=10 are taken; bit 10 is writer-scoped staging, which merged first)"
    );
    for (name, bit) in [
        ("KV_V3", FEATURE_INCOMPAT_KV_V3),
        ("NODE_SEQ_WATERMARK", FEATURE_INCOMPAT_NODE_SEQ_WATERMARK),
        ("KV_GUEST_SLOTS", FEATURE_INCOMPAT_KV_GUEST_SLOTS),
        ("KV_VOLUME_LIFECYCLE", FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE),
        ("KV_SLOT_MIGRATION", FEATURE_INCOMPAT_KV_SLOT_MIGRATION),
        ("KV_LAYOUT_DELTAS", FEATURE_INCOMPAT_KV_LAYOUT_DELTAS),
        ("KV_DYNAMIC_ROUTING", FEATURE_INCOMPAT_KV_DYNAMIC_ROUTING),
        ("KV_DURABLE_TERM", FEATURE_INCOMPAT_KV_DURABLE_TERM),
        (
            "KV_PARTITIONED_APPEND",
            FEATURE_INCOMPAT_KV_PARTITIONED_APPEND,
        ),
        ("KV_BLOCK_REFCOUNTS", FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS),
        (
            "KV_WRITER_SCOPED_STAGING",
            FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING,
        ),
        // DLM S6 / §6.2 item 7 (the claim-set record) took bit 12 after
        // this pin: it is listed here so a future renumber of EITHER bit
        // turns this assertion red instead of aliasing on disk.
        ("KV_CLAIM_SET", FEATURE_INCOMPAT_KV_CLAIM_SET),
    ] {
        assert_eq!(
            FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA & bit,
            0,
            "bit 11 collides with {name}"
        );
    }
    assert_ne!(
        FEATURES_INCOMPAT_KNOWN & FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA,
        0,
        "this binary must UNDERSTAND bit 11 (old binaries refuse it loud)"
    );
}

// ---------------------------------------------------------------------------
// 11: concurrency — authorization, quarantine and allocation together
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_authorization_and_quarantine_never_leak_an_offset() {
    let _serial = serial();
    let ba = allocator("s7-concurrent").await;
    let (_f, dev) = backing(256 * 1024 * 1024);
    let doubles0 = METRICS.block_double_frees.load(Ordering::Relaxed);

    // The recovery task runs the whole quarantine lifecycle — declare a
    // dead epoch, admit blocks IT owns (allocation is exclusive, so it can
    // never name a live writer's block), free them under quarantine, prove
    // the drain, release — while four writers authorize, allocate and
    // submit against the same allocator and device.
    let recovery = {
        let ba = ba.clone();
        tokio::spawn(async move {
            let mut cycles = 0usize;
            for round in 0..8 {
                let dead = declare_dead_epoch(&format!("s7 test: concurrent round {round}"));
                let mut cohort = Vec::new();
                for _ in 0..4 {
                    let Ok(o) = ba.allocate_block().await else {
                        break;
                    };
                    cohort.push(o);
                }
                for o in &cohort {
                    assert!(ba.quarantine_offset(*o, dead), "admission");
                }
                // Freed WHILE quarantined: the publish is owed to the proof.
                for o in &cohort {
                    ba.free_block(*o).await.expect("free");
                    assert!(
                        !ba.free_block_indices().contains(&(*o / ba.chunk_size())),
                        "a quarantined offset entered the free list"
                    );
                }
                tokio::task::yield_now().await;
                assert_eq!(
                    ba.release_quarantine(dead),
                    cohort.len(),
                    "the drain proof releases the whole cohort"
                );
                cycles += cohort.len();
            }
            cycles
        })
    };

    let mut writers = Vec::new();
    for w in 0..4u64 {
        let ba = ba.clone();
        let dev = dev.clone();
        writers.push(tokio::spawn(async move {
            let mut mine = Vec::new();
            for i in 0..8u64 {
                let auth = data_custody::authorize_dma(None)
                    .unwrap_or_else(|e| panic!("a healthy mount must authorize: {e:?}"));
                let Ok(off) = ba.allocate_block().await else {
                    continue;
                };
                // Allocation is exclusive, so a block handed to this
                // writer can never be in a dead epoch's cohort.
                assert!(
                    !ba.is_quarantined(off),
                    "allocator handed out quarantined offset {off}"
                );
                dev.write_block_authorized(
                    off,
                    bytes::Bytes::from(vec![(w * 8 + i) as u8; 4096]),
                    auth,
                )
                .await
                .expect("healthy submit");
                mine.push(off);
            }
            mine
        }));
    }

    let cycles = recovery.await.expect("recovery task");
    assert!(cycles > 0, "the quarantine lifecycle ran");
    let mut all = Vec::new();
    for w in writers {
        all.extend(w.await.expect("writer"));
    }
    // Exactly-once: no offset was handed to two live writers.
    let mut sorted = all.clone();
    sorted.sort_unstable();
    let before = sorted.len();
    sorted.dedup();
    assert_eq!(before, sorted.len(), "one offset, two owners");
    assert_eq!(
        METRICS.block_double_frees.load(Ordering::Relaxed),
        doubles0,
        "no double frees under concurrency"
    );
    assert_eq!(
        ba.quarantined_count(),
        0,
        "every cohort closed at its drain proof"
    );
}
