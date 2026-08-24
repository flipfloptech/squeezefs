//! Idea 4 — discard elision until pressure
//! (`docs/design-rewrite-program.md` §3; the rewrite program's
//! zero-mid-row-discards vehicle).
//!
//! A terminal free on a BdevDiscard-class backing no longer rides the
//! reclaim queue at all: `begin_free → tier purge → debt record →
//! finish_free` — immediately reallocatable, ZERO device commands. The
//! discard becomes RAM-tracked DEBT drained at the trim venues (idle /
//! pressure watermark / fstrim / defrag); allocation CANCELS a reused
//! offset's debt, and the trim protocol claims each offset OUT of the
//! free list before issuing (KD-4.4 — a discard can never race a new
//! owner's DMA).
//!
//! Contracts:
//! 1. **Elided-until-pressure**: an elided terminal free issues zero
//!    device commands, enters no queue, records debt, and the offset is
//!    immediately reallocatable; reuse cancels the debt (gauge honest).
//! 2. **Trim drains debt**: `trim_elided` claims + issues + returns;
//!    per-block ledger counters and the trim family both count; debt
//!    gauge returns to 0; the trimmed offsets stay allocatable.
//! 3. **Stale debt never destroys an owned offset**: a reused offset's
//!    bytes survive a later trim (the claim-cancels-debt +
//!    claim-out-of-free-list mutual exclusion).
//! 4. **The lever**: `SQUEEZEFS_DISCARD_ELISION=0` /
//!    `set_discard_elision(false)` restores the queued-reclaim path
//!    verbatim.
//! 5. **Class fence**: file backings (FilePunch class) never elide —
//!    the host-FS sparse-reclaim ENOSPC motivation stays alive (the
//!    test seam `set_elision_class_all` is what lets contracts 1–3 run
//!    on a file-backed harness at all).
//! 6. **Watermark derivation (no constants)**: elide while
//!    `debt ≤ virgin` (the never-minted tail) — the pure predicate +
//!    `virgin_bytes` arithmetic.
//! 7. **Fenced daemons never trim**: destructive device commands cease
//!    permanently (the D0 `failed` latch, the reclaimer's law).
//!
//! RED against `a0f38a4`: no elision path exists — every bdev-class
//! terminal free queues a device discard.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::METRICS;
use squeezefs::routing::DataRouter;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile};

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Restore production posture on scope exit (knob hygiene): elision ON
/// (the default), class-all seam OFF.
struct LeverGuard;
impl Drop for LeverGuard {
    fn drop(&mut self) {
        squeezefs::block_reclaim::set_discard_elision(true);
        squeezefs::block_reclaim::set_elision_class_all(false);
    }
}

async fn make_router() -> (
    DataRouter,
    Arc<BlockAllocator>,
    NamedTempFile,
    tempfile::TempDir,
) {
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new("discard_elision_test").await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("32MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm, cache, ba.clone(), nvme);
    (router, ba, b, s)
}

/// Pin the manners foreground signal PERPETUALLY MOVING: the idle/pressure
/// venue worker then defers forever (within-watermark stores pay zero
/// device commands under foreground — exactly the row posture), so every
/// assertion below races nothing. Explicit `trim_elided`/`reclaim_drain`
/// calls bypass manners by design.
fn pin_foreground(router: &DataRouter) {
    let c = Arc::new(std::sync::atomic::AtomicU64::new(0));
    router
        .backend_router
        // +1: the worker's first probe must already differ from its
        // zero-initialized last-value (fetch_add returns the PREVIOUS
        // value — a first-probe 0 reads as idle and drains).
        .set_reclaim_foreground_signal(Arc::new(move || c.fetch_add(1, Ordering::Relaxed) + 1));
}

fn elided() -> u64 {
    METRICS.block_free_reclaim_elided.load(Ordering::Relaxed)
}
fn debt_gauge() -> u64 {
    METRICS.block_free_elided_debt_bytes.load(Ordering::Relaxed)
}
fn queued() -> u64 {
    METRICS.block_free_reclaim_queued.load(Ordering::Relaxed)
}
fn punches() -> u64 {
    METRICS.block_free_file_punches.load(Ordering::Relaxed)
}
fn trim_blocks() -> u64 {
    METRICS.block_free_trim_discards.load(Ordering::Relaxed)
}
fn trim_bytes() -> u64 {
    METRICS.block_free_trim_bytes.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Contract 1 — elided-until-pressure: zero device commands, immediate
// reallocatability, honest debt bookkeeping, claim-cancels-debt.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn elided_terminal_free_is_reallocatable_with_zero_device_commands() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::block_reclaim::set_discard_elision(true);
    squeezefs::block_reclaim::set_elision_class_all(true);
    let (router, ba, _b, _s) = make_router().await;
    pin_foreground(&router);

    let offset = ba.allocate_block().await.expect("alloc");
    ba.publish_block(offset);
    let bs = router.block_size.load(Ordering::Relaxed);

    let (e0, q0, p0, g0) = (elided(), queued(), punches(), debt_gauge());
    router
        .backend_router
        .free_block(&offset.to_string())
        .await
        .expect("terminal free");

    assert_eq!(elided() - e0, 1, "the free must ELIDE (the engagement arm)");
    assert_eq!(
        queued() - q0,
        0,
        "an elided free never enters the reclaim queue — the ledger \
         identity is queued + elided ≡ terminal frees"
    );
    assert_eq!(punches() - p0, 0, "zero device commands during elision");
    assert_eq!(debt_gauge() - g0, bs, "debt gauge records the freed bytes");
    assert_eq!(
        router.backend_router.elided_debt_bytes(),
        debt_gauge(),
        "router gauge accessor mirrors the METRICS gauge"
    );

    // Immediately reallocatable — and the claim CANCELS the debt.
    let again = ba.allocate_block().await.expect("realloc");
    assert_eq!(
        again, offset,
        "the elided offset is the only free-listed block: allocation \
         must hand it out immediately (no reclaim window)"
    );
    assert_eq!(
        debt_gauge(),
        g0,
        "claim-cancels-debt: a reused offset owes no discard"
    );
}

// ---------------------------------------------------------------------------
// Contract 2 — trim drains the debt (claim → issue → return).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trim_drains_debt_with_counted_commands_and_returns_offsets() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::block_reclaim::set_discard_elision(true);
    squeezefs::block_reclaim::set_elision_class_all(true);
    let (router, ba, backing, _s) = make_router().await;
    pin_foreground(&router);

    let mut offsets = Vec::new();
    for _ in 0..3 {
        let o = ba.allocate_block().await.expect("alloc");
        // Land real bytes so the punch has something to deallocate.
        router
            .nvme_writer
            .write_block(o, bytes::Bytes::from(vec![0xABu8; 8192]))
            .await
            .expect("write");
        ba.publish_block(o);
        offsets.push(o);
    }
    let bs = router.block_size.load(Ordering::Relaxed);
    for o in &offsets {
        router
            .backend_router
            .free_block(&o.to_string())
            .await
            .expect("free");
    }
    assert_eq!(router.backend_router.elided_debt_bytes(), 3 * bs);

    let (t0, tb0, p0) = (trim_blocks(), trim_bytes(), punches());
    let (blocks, bytes) = router.backend_router.trim_elided(false).await;
    assert_eq!(blocks, 3, "trim reclaims every debt block");
    assert_eq!(bytes, 3 * bs, "trim reclaims every debt byte");
    assert_eq!(trim_blocks() - t0, 3, "trim family counts per block");
    assert_eq!(trim_bytes() - tb0, 3 * bs, "trim family counts bytes");
    assert_eq!(
        punches() - p0,
        3,
        "the per-block device-reclaim ledger still accounts every block \
         (file harness ⇒ punches; bdev ⇒ discards)"
    );
    assert_eq!(
        router.backend_router.elided_debt_bytes(),
        0,
        "debt gauge returns to zero after the trim"
    );

    // The punched ranges actually deallocated (read back zeros).
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(backing.path()).expect("open backing");
    let mut buf = vec![0u8; 8192];
    for o in &offsets {
        f.read_exact_at(&mut buf, *o).expect("pread");
        assert!(buf.iter().all(|&x| x == 0), "trimmed range deallocated");
    }

    // The offsets RETURNED to the free list (trim claims are transient).
    let mut got = Vec::new();
    for _ in 0..3 {
        got.push(ba.allocate_block().await.expect("realloc after trim"));
    }
    got.sort_unstable();
    let mut want = offsets.clone();
    want.sort_unstable();
    assert_eq!(got, want, "trimmed offsets stay allocatable (returned)");
}

// ---------------------------------------------------------------------------
// Contract 3 — stale debt never destroys an owned offset.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_debt_never_discards_an_owned_offset() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::block_reclaim::set_discard_elision(true);
    squeezefs::block_reclaim::set_elision_class_all(true);
    let (router, ba, backing, _s) = make_router().await;
    pin_foreground(&router);

    let offset = ba.allocate_block().await.expect("alloc");
    ba.publish_block(offset);
    router
        .backend_router
        .free_block(&offset.to_string())
        .await
        .expect("free");

    // Reuse the offset: the new owner lands real bytes.
    let again = ba.allocate_block().await.expect("realloc");
    assert_eq!(again, offset, "premise: the same offset is reused");
    let pattern: Vec<u8> = (0..8192usize).map(|i| (i % 251) as u8).collect();
    router
        .nvme_writer
        .write_block(offset, bytes::Bytes::from(pattern.clone()))
        .await
        .expect("owner write");
    ba.publish_block(offset);

    // A later trim must not touch the owned offset.
    let t0 = trim_blocks();
    let (blocks, _bytes) = router.backend_router.trim_elided(false).await;
    assert_eq!(blocks, 0, "no debt may survive the reuse");
    assert_eq!(trim_blocks() - t0, 0, "zero trim commands");

    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(backing.path()).expect("open backing");
    let mut buf = vec![0u8; 8192];
    f.read_exact_at(&mut buf, offset).expect("pread");
    assert_eq!(
        buf, pattern,
        "the owner's bytes are intact — a stale debt entry can never \
         become a discard on a reused offset (KD-4.4)"
    );
}

// ---------------------------------------------------------------------------
// Contract 4 — the lever restores the queued-reclaim path verbatim.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn elision_lever_off_restores_queued_reclaim_verbatim() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::block_reclaim::set_discard_elision(false);
    squeezefs::block_reclaim::set_elision_class_all(true);
    let (router, ba, _b, _s) = make_router().await;
    pin_foreground(&router);

    let offset = ba.allocate_block().await.expect("alloc");
    ba.publish_block(offset);

    let (e0, q0, p0) = (elided(), queued(), punches());
    router
        .backend_router
        .free_block(&offset.to_string())
        .await
        .expect("free");
    router.backend_router.reclaim_drain().await;

    assert_eq!(elided() - e0, 0, "lever off ⇒ nothing elides");
    assert_eq!(queued() - q0, 1, "lever off ⇒ the queued path, verbatim");
    assert_eq!(punches() - p0, 1, "the queued reclaim issued as before");
}

// ---------------------------------------------------------------------------
// Contract 5 — class fence: file backings never elide (no seam).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_backings_keep_the_queued_reclaim_without_the_seam() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::block_reclaim::set_discard_elision(true);
    squeezefs::block_reclaim::set_elision_class_all(false);
    let (router, ba, _b, _s) = make_router().await;
    pin_foreground(&router);

    let offset = ba.allocate_block().await.expect("alloc");
    ba.publish_block(offset);

    let (e0, q0) = (elided(), queued());
    router
        .backend_router
        .free_block(&offset.to_string())
        .await
        .expect("free");
    router.backend_router.reclaim_drain().await;

    assert_eq!(
        elided() - e0,
        0,
        "a FilePunch-class backing must never elide: host-FS sparse \
         reclaim is the ENOSPC motivation (KD-4.1)"
    );
    assert_eq!(queued() - q0, 1, "file backings ride the queue as today");
}

// ---------------------------------------------------------------------------
// Contract 6 — the watermark derivation (no constants).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watermark_is_debt_versus_virgin_tail() {
    let _g = serial().await;
    // Pure predicate: elide while debt ≤ virgin.
    assert!(squeezefs::block_reclaim::debt_within_watermark(0, 0));
    assert!(squeezefs::block_reclaim::debt_within_watermark(100, 100));
    assert!(!squeezefs::block_reclaim::debt_within_watermark(101, 100));

    // virgin_bytes arithmetic: never-minted tail × chunk.
    let ba = BlockAllocator::new("wm_test").await.unwrap();
    let chunk = ba.chunk_size();
    ba.set_capacity_bytes(8 * chunk);
    assert_eq!(ba.virgin_bytes(), 8 * chunk, "untouched store: all virgin");
    let _a = ba.allocate_block().await.expect("alloc");
    let _b = ba.allocate_block().await.expect("alloc");
    assert_eq!(
        ba.virgin_bytes(),
        6 * chunk,
        "virgin = (capacity − cursor) × chunk"
    );
    // Unbounded (capacity 0) allocators have an infinite virgin tail:
    // pressure never fires (idle/trim venues still drain).
    let ba2 = BlockAllocator::new("wm_test2").await.unwrap();
    assert_eq!(ba2.virgin_bytes(), u64::MAX, "unbounded ⇒ infinite virgin");
}

// ---------------------------------------------------------------------------
// Contract 7 — a fenced daemon never trims.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fenced_daemon_never_trims() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::block_reclaim::set_discard_elision(true);
    squeezefs::block_reclaim::set_elision_class_all(true);
    let (router, ba, _b, _s) = make_router().await;
    pin_foreground(&router);

    let offset = ba.allocate_block().await.expect("alloc");
    ba.publish_block(offset);
    router
        .backend_router
        .free_block(&offset.to_string())
        .await
        .expect("free");
    assert!(router.backend_router.elided_debt_bytes() > 0, "debt exists");

    // Latch the D0 fail-stop.
    router
        .backend_router
        .set_reclaim_fence_signal(Arc::new(|| true));

    let (t0, p0) = (trim_blocks(), punches());
    let (blocks, bytes) = router.backend_router.trim_elided(true).await;
    assert_eq!(
        (blocks, bytes),
        (0, 0),
        "a fenced holder issues NO destructive device commands — the \
         reclaimer's fence-halt law applies to trim verbatim"
    );
    assert_eq!(trim_blocks() - t0, 0);
    assert_eq!(punches() - p0, 0);
}

// ---------------------------------------------------------------------------
// Contract 8 — a fully GRACE-HELD backlog paces the drainer (finding 12,
// 2026-08-23): with the freed-offset grace plane armed, every elided
// offset is held OUT of the free list until readers acknowledge, so the
// drainer's trim claims all lose and `take_debt_batch` correctly filters
// every candidate. The shipped loop counted "a batch ran" as progress and
// looped hot — ~5,000 empty passes/s of blocking-pool churn on an IDLE
// set authority, holding the box at 85–88 °C and tripping the acceptance
// rig's quiet-box gate. The law: a pass that reclaims nothing ticks
// coarsely (never a hot loop); grace releases ride allocation demand and
// the next tick sees them.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fully_grace_held_backlog_paces_the_drainer() {
    use squeezefs::membership::{
        self, JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MembershipOwner,
    };
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::block_reclaim::set_discard_elision(true);
    squeezefs::block_reclaim::set_elision_class_all(true);

    // Arm the freed-offset grace plane in-process (the
    // reader_free_grace_tests fixture shape): an installed OWNER plus one
    // member that acknowledges nothing, so the bound stays 0 and every
    // deferred offset is held for the whole test.
    struct GraceGuard;
    impl Drop for GraceGuard {
        fn drop(&mut self) {
            squeezefs::free_grace::reset_for_test();
            membership::uninstall();
        }
    }
    squeezefs::free_grace::reset_for_test();
    membership::uninstall();
    let _grace = GraceGuard;
    let ticks = Arc::new(std::sync::atomic::AtomicU64::new(10_000));
    let clock = LeaseClock::manual(Arc::clone(&ticks));
    let clocks = LeaseClocks::derive(std::time::Duration::from_micros(250))
        .expect("the shipped derivation must be safe");
    let owner = MembershipOwner::arm("drain-pace-owner", 3, 2, clocks, clock).expect("owner arms");
    membership::install_owner(Arc::clone(&owner));
    let grant = match owner.join(JoinRequest {
        id: "drain-pace-member".to_string(),
        role: MemberRole::Reader,
        endpoint: None,
        pid: std::process::id(),
        boot: "boot-drain-pace".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    }) {
        JoinOutcome::Granted(g) => g,
        other => panic!("member join must be granted: {other:?}"),
    };
    squeezefs::free_grace::arm_owner_plane(LeaseClock::manual(Arc::clone(&ticks)), owner.clocks())
        .expect("the derived bound is safe");
    owner.refresh_free_grace_bound();
    assert!(
        squeezefs::free_grace::armed(),
        "the plane must be armed for the holds to exist"
    );

    let (router, ba, _b, _s) = make_router().await;
    // A CONSTANT foreground signal (the inverse of `pin_foreground`): the
    // bug's arm is the IDLE posture, reached once the manners law's
    // confirm horizon (IDLE_CONFIRM_TICKS × 50 ms = 1 s) sees a stable
    // signal — the deferred arm before it already ticks at 50 ms by
    // design.
    router
        .backend_router
        .set_reclaim_foreground_signal(Arc::new(|| 0));

    // Terminal frees whose elided debt defers into grace: held out of the
    // free list, unclaimable by the trim, un-taken by take_debt_batch.
    for _ in 0..4 {
        let offset = ba.allocate_block().await.expect("alloc");
        ba.publish_block(offset);
        router
            .backend_router
            .free_block(&offset.to_string())
            .await
            .expect("terminal free");
    }
    assert!(
        squeezefs::free_grace::held_offsets() >= 4,
        "the fixture is honest only if the frees are grace-held"
    );
    assert!(debt_gauge() > 0, "the elided debt is outstanding");

    // The frees woke the drainer. Let the confirm horizon pass (1 s of
    // stable signal), then measure a 1 s idle window: a paced drainer
    // runs a bounded handful of passes there; the hot loop ran thousands
    // per second.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let before = METRICS.block_free_debt_drain_passes.load(Ordering::Relaxed);
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    let passes = METRICS
        .block_free_debt_drain_passes
        .load(Ordering::Relaxed)
        .saturating_sub(before);
    assert!(
        passes <= 25,
        "an unclaimable (grace-held) backlog must tick coarsely, never \
         loop hot: {passes} drainer passes in 1 s of idle"
    );
    assert!(
        debt_gauge() > 0,
        "the grace-held debt is still outstanding (nothing was lost to \
         the pacing — the ledger stays honest)"
    );

    // Gauge hygiene for the suite's siblings: the GLOBAL debt gauge must
    // not carry this test's grace-held bytes into the next test. Release
    // the holds through the PRODUCTION path — the member acknowledges
    // everything, the bound republishes, an allocation harvests the ring
    // back onto the free list — then take the now-claimable debt, which
    // decrements the gauge.
    // A large FINITE ack (u64::MAX is min_acked_free_epoch's
    // "acknowledged nothing" sentinel and reads as 0).
    assert!(
        matches!(
            owner.renew("drain-pace-member", grant.epoch, 1_000_000_000),
            squeezefs::membership::RenewOutcome::Renewed(_)
        ),
        "the acknowledging renewal must be admitted"
    );
    owner.refresh_free_grace_bound();
    let _ = ba.allocate_block().await.expect("harvesting allocation");
    let _ = ba.take_debt_batch(usize::MAX);
    assert_eq!(debt_gauge(), 0, "suite-clean: no leaked debt gauge bytes");
}
