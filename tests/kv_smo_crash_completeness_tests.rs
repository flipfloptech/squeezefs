//! FIND-VS-A (part 2, acked-create loss): crash-replay completeness under
//! SMO churn — tests-first contract for the fix.
//!
//! The 2026-07-16 forensics (`.benchmarks/2026-07-16-find-vs-a-fix.md`)
//! caught the v3 backend losing **acked, committed** creates across a
//! process crash with a *clean* replay (`replay_dropped_torn == 0`):
//! under a create storm, leaf SMOs (compact/split) retire nodes whose
//! `dirty_floor` still names un-checkpoint-covered record seqs; the
//! checkpoint tail rule (`§4.6 pt 2`) then only sees LIVE nodes' floors,
//! so the next ledger record's `journal_tail_seq` can pass records whose
//! only durable copy rides successor images + in-RAM routing that the
//! mounted ledger record does not name. A kill between that ledger write
//! and full coverage loses the records (observed: ENOENT on 1–4 % of
//! acked creates; scoreboard FIND-VS-A R3 rows INVALID).
//!
//! Contract (design §4.10, "every ledger-acked op present and whole"):
//! **an acked commit followed by ANY crash must be served after reopen** —
//! whatever mix of checkpoints and SMOs ran in between. The reopen here is
//! the process-crash equivalent: every device write is buffered (page
//! cache), so a fresh `open` of the same file sees exactly the bytes a
//! post-kill remount would.
use squeezefs::meta_backend::kv::backend::{KvMetaBackend, TEST_PENDING_FREE_CAP};
use squeezefs::meta_backend::kv::builder::{digest_backend, format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::record::TREE_INODES;
use squeezefs::meta_backend::kv::tree::{
    test_smo_build_pause_release, TEST_SMO_BUILD_PAUSED, TEST_SMO_BUILD_PAUSE_TREE,
};
use squeezefs::meta_backend::Metadata;
use squeezefs::meta_backend::RoutedMetaBackend;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::NamedTempFile;

const VOL_LEN: u64 = 256 * 1024 * 1024;
/// Smallest legal node: leaf logs fill fast, so compact/split SMO churn
/// (the loss precondition) fires hundreds of times within one test.
const NODE_SIZE: usize = 64 * 1024;
const RING_LEN: u64 = 8 * 1024 * 1024;

async fn sandbox() -> (Arc<RoutedMetaBackend>, Arc<KvMetaBackend>, NamedTempFile) {
    let file = NamedTempFile::new().expect("temp volume");
    file.as_file().set_len(VOL_LEN).unwrap();
    format_v3(
        file.path(),
        VOL_LEN,
        &FormatV3Options {
            node_size: NODE_SIZE,
            journal_len_override: Some(RING_LEN),
            force: false,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3");
    let kv = KvMetaBackend::open(file.path()).await.expect("open");
    let routed = Arc::new(RoutedMetaBackend::new(vec![kv.clone()]));
    (routed, kv, file)
}

/// Crash-equivalent reopen: the caller dropped every Arc without
/// shutdown; buffered device writes (page cache) are exactly what a
/// post-kill remount reads. In-process, the single-writer flock releases
/// only when the detached checkpoint/pass tasks drop their last Arc —
/// poll-retry (a real crash releases the flock instantly; this is
/// harness plumbing, not the contract under test).
async fn reopen(path: &std::path::Path) -> Arc<KvMetaBackend> {
    for _ in 0..200 {
        match KvMetaBackend::open(path).await {
            Ok(be) => return be,
            Err(squeezefs::meta_backend::kv::KvError::Busy(_)) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => panic!("reopen: {e:?}"),
        }
    }
    panic!("writer guard must release once the old backend is dropped");
}

/// The storm shape from the forensics, shrunk: bursts of acked creates
/// with checkpoint cycles interleaved (the cadence tick's job), enough to
/// drive leaf compactions/splits with dirty floors dying at retire —
/// then a crash-equivalent reopen that must serve every acked name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acked_creates_survive_crash_across_smo_churn() {
    let (routed, kv, file) = sandbox().await;

    let mut acked: Vec<String> = Vec::new();
    // ~24 burst/checkpoint rounds × 400 creates ≈ 9.6k dentries: a 64 KiB
    // leaf folds ~1k records, so this drives dozens of compactions and
    // splits with checkpoints (and their ledger writes) interleaved at
    // storm-realistic points — the exact retire-with-floor overlap the
    // forensics caught losing records.
    for round in 0..24 {
        for i in 0..400 {
            let name = format!("f{round:02}_{i:04}");
            routed
                .create(1, &name, libc::S_IFREG | 0o644, 0, 0)
                .await
                .expect("create acked");
            acked.push(name);
        }
        kv.checkpoint_now().await.expect("checkpoint cycle");
    }

    // Process-crash equivalent: drop RAM state without shutdown.
    drop(routed);
    drop(kv);
    let kv2 = reopen(file.path()).await;
    let routed2 = Arc::new(RoutedMetaBackend::new(vec![kv2.clone()]));

    let mut lost: Vec<String> = Vec::new();
    for name in &acked {
        if routed2.lookup(1, name).await.is_err() {
            lost.push(name.clone());
        }
    }
    assert!(
        lost.is_empty(),
        "{} of {} ACKED creates vanished across a crash-equivalent reopen \
         (clean replay, no torn writes — the FIND-VS-A loss class): {:?} …",
        lost.len(),
        acked.len(),
        &lost[..lost.len().min(12)]
    );
}

/// Sub-mechanism (i) **stranding**, held open deterministically
/// (docs/design-smo-replay-currency.md §1, §6 PR 1(a)): an SMO's
/// successor images are built with no locks held; user commits racing
/// that build window reserve BEFORE the SMO's in-lock flip reservation,
/// so their seqs are lower than the flip's and their records reach the
/// successors only as `take_overlay()` leftovers — RAM overlays that die
/// with the process. Single-pass seq-order replay then routes them via
/// the pre-flip structure into the abandoned predecessor, and the
/// higher-seq flip unroutes them: acked, in-window, replayed "cleanly" —
/// and lost.
///
/// The test arms the `TEST_SMO_BUILD_PAUSE_TREE` seam, parks an inode-
/// leaf SMO in its build window, injects ACKED setattr commits into the
/// paused leaf's key range, releases, and asserts the commits are served
/// across a crash-equivalent reopen. RED on dev `44d14d6` (the racing
/// records fold to the predecessor's overlay and the post-replay walk
/// routes around them — getattr serves the pre-race mode); GREEN under
/// the C′ two-phase replay (PR 2).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stranded_build_window_commits_survive_crash() {
    // Park the checkpoint cadence (the crash_kill_tests precedent):
    // checkpoints and maintenance run only where this test drives them,
    // so the SMO under test is the one the seam pauses.
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
            test_smo_build_pause_release();
        }
    }
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let _cleanup = Cleanup;

    let (_routed, kv, file) = sandbox().await;

    // Setup: enough creates that the INODES tree is depth ≥ 2 (leaf SMOs
    // then journal pointer flips — a root swap journals none and is the
    // design's C′ carve-out, not this contract). Clean shutdown + reopen
    // leaves a fully materialized structure, an empty replay window, and
    // every node clean — the SMO cycle under test starts from nothing.
    let mut inos: Vec<u64> = Vec::with_capacity(2400);
    for i in 0..2400u32 {
        let ino = kv
            .create(1, &format!("s{i:05}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("setup create")
            .ino;
        inos.push(ino);
        if i % 400 == 399 {
            kv.checkpoint_now().await.expect("setup checkpoint");
        }
    }
    kv.shutdown().await.expect("setup shutdown");
    drop(kv);
    let kv = reopen(file.path()).await;

    // Arm the seam for the INODES tree, then dirty a contiguous ino band
    // (~200 inos ⇒ one or two leaves). Each round crosses the §4.6 pt 1
    // writeback threshold, so the parked-cadence checkpoint task runs
    // maintenance-only passes (bset appends — no barrier, no ledger)
    // until a band leaf's log fills and its SMO parks in the build
    // window. Deliberately NO checkpoint cycle after arming: the loss
    // window under test is "crash before the next cycle's flush pass
    // covers the successors" — a full cycle here would materialize the
    // successor overlays and mask the stranding (the cadence does
    // exactly that eventually in production; the kill races it).
    TEST_SMO_BUILD_PAUSE_TREE.store(u64::from(TREE_INODES), Ordering::SeqCst);
    let band = &inos[1200..1400];
    let mut parked = false;
    'drive: for round in 0..60u32 {
        let mode = libc::S_IFREG | if round % 2 == 0 { 0o640 } else { 0o600 };
        for ino in band {
            kv.setattr(*ino, Some(mode), None, None, None, None, None, None)
                .await
                .expect("band setattr acked");
        }
        // Give the woken maintenance pass its scheduling turns; a pause
        // can engage mid-round or between rounds.
        for _ in 0..100 {
            if TEST_SMO_BUILD_PAUSED.lock().expect("seam mutex").is_some() {
                parked = true;
                break 'drive;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }
    assert!(
        parked,
        "a band-leaf SMO must park in the build window within the dirtying budget"
    );
    let info = TEST_SMO_BUILD_PAUSED
        .lock()
        .expect("seam mutex")
        .clone()
        .expect("pause info published while parked");
    assert_eq!(info.tree_id, TREE_INODES, "the armed tree pauses");
    assert!(
        !info.is_root,
        "depth-2 setup: the paused SMO must journal pointer flips (a root \
         swap journals none — the C′ carve-out, covered by Option A)"
    );

    // The racing commits (design §1: `C (T ≤ c < p_flip)`): ACKED setattr
    // Puts on inos inside the PAUSED leaf's key range — memcmp against
    // the leaf bounds, exactly the revalidation the conveyor apply runs.
    // Kept small (32 × ~70 B ≪ the 4 KiB writeback threshold) so nothing
    // re-enqueues the frozen predecessor mid-SMO.
    const MARKER: u32 = libc::S_IFREG | 0o751;
    let racing: Vec<u64> = inos
        .iter()
        .copied()
        .filter(|ino| {
            let k = ino.to_be_bytes();
            k[..] >= info.min_key[..] && k[..] <= info.max_key[..]
        })
        .take(32)
        .collect();
    assert!(
        racing.len() >= 8,
        "the paused leaf ({:x?}..{:x?}) must cover a slice of the known inos",
        info.min_key,
        info.max_key
    );
    for ino in &racing {
        kv.setattr(*ino, Some(MARKER), None, None, None, None, None, None)
            .await
            .expect("racing setattr ACKED while the SMO build is parked");
    }

    // Release, then wait for the SMO itself to complete (its counter is
    // the last step of `smo_replace`): the lock window runs take_overlay
    // — the racing records reach the successors ONLY as RAM overlays —
    // and the flip's journal entry is written with a seq above every
    // racing seq. No checkpoint runs after (cadence parked): the exact
    // production shape where the kill beats the next cycle.
    let smos_before = squeezefs::meta_backend::kv::META_KV_NODE_COMPACTIONS.load(Ordering::Relaxed)
        + squeezefs::meta_backend::kv::META_KV_NODE_SPLITS.load(Ordering::Relaxed);
    test_smo_build_pause_release();
    let mut completed = false;
    for _ in 0..1000 {
        let now = squeezefs::meta_backend::kv::META_KV_NODE_COMPACTIONS.load(Ordering::Relaxed)
            + squeezefs::meta_backend::kv::META_KV_NODE_SPLITS.load(Ordering::Relaxed);
        if now > smos_before {
            completed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert!(completed, "the released SMO must run to completion");

    // Sanity: live RAM serves the markers (the bounded second merge).
    for ino in &racing {
        assert_eq!(
            kv.getattr(*ino).await.expect("live getattr").mode,
            MARKER,
            "live state must serve the racing commit (ino {ino})"
        );
    }

    // Crash-equivalent reopen: the acked racing commits MUST be served.
    drop(kv);
    let kv2 = reopen(file.path()).await;
    let mut stale: Vec<(u64, u32)> = Vec::new();
    for ino in &racing {
        let mode = kv2.getattr(*ino).await.expect("inode present").mode;
        if mode != MARKER {
            stale.push((*ino, mode));
        }
    }
    assert!(
        stale.is_empty(),
        "{} of {} ACKED build-window commits stranded across a crash-equivalent \
         reopen (clean replay — single-pass seq-order replay routed them to the \
         abandoned predecessor, then the higher-seq flip unrouted them; \
         design-smo-replay-currency §1 sub-mechanism (i)): {:?}",
        stale.len(),
        racing.len(),
        &stale[..stale.len().min(12)]
    );
}

/// Replay determinism across the phased order (§4.10 "replay
/// idempotence", extended by design-smo-replay-currency §2 C′): two
/// crash-equivalent reopens of the same post-crash bytes — each a full
/// replay of the same window over the same mounted ledger — must fold to
/// identical post-fold digests. Deterministic on the single-pass order
/// by construction; the C′ two-phase order must preserve it (read-only
/// replay, deterministic total order over the same materialized entries).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_twice_digest_stable_across_smo_windows() {
    let (routed, kv, file) = sandbox().await;

    // SMO-churn storm with interleaved checkpoints, crash-cut mid-flight
    // (no shutdown): the replay window carries content + SMO flips.
    for round in 0..8 {
        for i in 0..400 {
            routed
                .create(
                    1,
                    &format!("d{round:02}_{i:04}"),
                    libc::S_IFREG | 0o644,
                    0,
                    0,
                )
                .await
                .expect("create acked");
        }
        kv.checkpoint_now().await.expect("checkpoint cycle");
    }
    drop(routed);
    drop(kv);

    let kv_a = reopen(file.path()).await;
    let d_a = digest_backend(&kv_a).await.expect("digest a");
    drop(kv_a);
    let kv_b = reopen(file.path()).await;
    let d_b = digest_backend(&kv_b).await.expect("digest b");
    assert_eq!(
        d_a, d_b,
        "replay-twice digests diverge — the replay order is not deterministic"
    );
}

/// FIND-SMO-TAIL (docs/design-smo-replay-currency.md §1b, PR 1(d)):
/// **mid-entry checkpoint tails from multi-leaf user-tx floors.**
///
/// Floors are raw *record* seqs (`dirty_floor.fetch_min(rec.seq)`,
/// `node_cache.rs`) while the conveyor stamps per-record seqs
/// `entry_start + i` across a multi-leaf tx. An `unlink` is exactly that
/// shape: rec[0] (dentry Delete) lands on the DENTRIES leaf, rec[1]
/// (parent Δtime) and rec[2] (child Put) land on the INODES leaf — so the
/// INODES leaf's floor pins at `entry_start + 1`, strictly inside the
/// entry. Flush the dentries leaf while the inodes leaf's floor is
/// restored un-flushed (its log is full: freeze → append fails →
/// `restore_dirty_floor` → compaction SMO retires it, folding the
/// mid-entry floor into the dying-floor clamp) and the §4.6 pt 2 tail
/// computes to `entry_start + 1` — violating the `checkpoint.rs` module-
/// doc claim "the tail is always an entry boundary". Replay's chain walk
/// starts parsing AT the tail (`journal.rs`), fails mid-entry, resyncs at
/// the next page's first-entry offset, and drops the entry's own ≥-tail
/// records plus every collateral entry up to the resync point — counted
/// in `dropped_torn` once a later entry parses (acked commits made after
/// the checkpoint are the collateral: their only durable copy is the
/// journal window).
///
/// The construction is deterministic with no byte calibration: fill the
/// INODES root leaf's log with same-key setattr bursts (compactions mark
/// lifecycle boundaries), pre-position near-full, then probe with acked
/// unlinks — every landed probe shrinks the remaining log area by one
/// bset frame, so within a bounded number of probes one probe's freeze
/// MUST fail and its compaction retires the leaf with the mid-entry
/// floor. Post-SMO acked commits (> one journal page's worth) guarantee a
/// later entry parses, confirming the drop.
///
/// Asserted JOINTLY (the §1b signature edge: a mid-entry tail on the
/// newest entry can read `dropped_torn == 0`, so neither half alone is
/// the contract): **no acked state is lost AND `dropped_torn == 0`**.
/// RED on dev `89ea158` (`dropped_torn ≥ 1`, plus page-position-dependent
/// acked loss); GREEN once floors round DOWN to entry starts (PR 3).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mid_entry_tail_multi_leaf_tx_partial_flush_survives_crash() {
    // Park the checkpoint cadence (the crash_kill_tests precedent):
    // cycles run only where this test drives them.
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
        }
    }
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let _cleanup = Cleanup;

    let (routed, kv, file) = sandbox().await;

    let smo_count = || {
        squeezefs::meta_backend::kv::META_KV_NODE_COMPACTIONS.load(Ordering::Relaxed)
            + squeezefs::meta_backend::kv::META_KV_NODE_SPLITS.load(Ordering::Relaxed)
    };

    // Setup: probe files (unlink victims + survivors) and the fill file.
    // The INODES tree stays depth 1 (root leaf) — a compaction of it is a
    // root swap, which journals no pointer flips but still retires the
    // node with its floor (the dying-floor fold): the §1b mechanism does
    // not depend on tree depth, only on the floor domain.
    let mut probe_names: Vec<String> = Vec::new();
    for i in 0..80u32 {
        let name = format!("probe{i:03}");
        kv.create(1, &name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("setup create acked");
        probe_names.push(name);
    }
    let fill_ino = kv
        .create(1, "fillfile", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("fill create acked")
        .ino;
    kv.checkpoint_now().await.expect("setup checkpoint");

    // One fill burst: 24 same-ino setattr Puts (each a 1-record tx —
    // floors trivially at entry starts, so fills can never fake the RED)
    // accumulated in the leaf's open overlay, then one checkpoint cycle
    // freezing them into ONE bset append (~2.5 KiB — under the 4 KiB
    // writeback threshold, so no maintenance wake interferes).
    let mut fill_mode_flip = 0u32;
    let burst = |kv: Arc<KvMetaBackend>, flip: u32| async move {
        for j in 0..24u32 {
            let mode = libc::S_IFREG
                | if (flip + j).is_multiple_of(2) {
                    0o640
                } else {
                    0o600
                };
            kv.setattr(fill_ino, Some(mode), None, None, None, None, None, None)
                .await
                .expect("fill setattr acked");
        }
    };

    // Lifecycle calibration: bursts-to-compaction measured on the SECOND
    // lifecycle (steady-state base: the same folded records), then
    // pre-position the third lifecycle near-full with K-2 bursts.
    let mut k_bursts: Option<u64> = None;
    let mut this_lifecycle = 0u64;
    let mut lifecycles = 0u32;
    for i in 0..400u64 {
        assert!(i < 399, "fill lifecycles must converge within the budget");
        let before = smo_count();
        burst(kv.clone(), fill_mode_flip).await;
        fill_mode_flip += 1;
        kv.checkpoint_now().await.expect("fill checkpoint");
        this_lifecycle += 1;
        if smo_count() > before {
            lifecycles += 1;
            k_bursts = Some(this_lifecycle);
            this_lifecycle = 0;
            if lifecycles == 2 {
                break;
            }
        }
    }
    let k = k_bursts.expect("a fill lifecycle completed");
    assert!(k >= 3, "a 64 KiB leaf must absorb several ~2.5 KiB bursts");
    let mut placed = 0u64;
    while placed < k - 2 {
        let before = smo_count();
        burst(kv.clone(), fill_mode_flip).await;
        fill_mode_flip += 1;
        kv.checkpoint_now().await.expect("position checkpoint");
        placed += 1;
        if smo_count() > before {
            placed = 0; // unexpected recycle: restart on the fresh leaf
        }
    }

    // Probe loop: one acked multi-leaf unlink per cycle. A landed probe's
    // freeze appends ~250 B (covered — benign); the leaf's remaining log
    // area strictly shrinks, so a bounded number of probes reaches the
    // §1b cycle: dentries leaf flushes rec[0], the inodes leaf's freeze
    // (parent Δ + child Put) hits the full log, its floor — pinned at
    // the tx's `entry_start + 1` — is restored, and the compaction SMO
    // retires the leaf into the dying-floor fold: the written ledger's
    // tail is strictly inside the unlink's journal entry.
    let mut victim: Option<String> = None;
    let mut unlinked: Vec<String> = Vec::new();
    for name in probe_names.iter().take(60) {
        kv.unlink(1, name).await.expect("probe unlink acked");
        unlinked.push(name.clone());
        let before = smo_count();
        kv.checkpoint_now().await.expect("probe checkpoint");
        if smo_count() > before {
            victim = Some(name.clone());
            break;
        }
    }
    let victim = victim.expect(
        "a probe unlink must trigger the log-full compaction within the probe budget \
         (the leaf's remaining log area strictly shrinks per probe)",
    );

    // Post-SMO acked commits — the §1b collateral + drop confirmers.
    // > 4 KiB of small entries guarantees at least one entry starts past
    // the mid-entry tail's journal page: replay's resync parses it and
    // confirms the dropped entry; the ones on the tail page itself are
    // dropped with it (acked loss). None of these are flushed (no further
    // checkpoint): their only durable copy is the journal window.
    const MARKER: u32 = libc::S_IFREG | 0o751;
    let survivors: Vec<String> = probe_names
        .iter()
        .filter(|n| !unlinked.contains(*n))
        .take(40)
        .cloned()
        .collect();
    assert!(
        survivors.len() >= 24,
        "enough survivor files must remain for the confirmer commits"
    );
    let mut marked: Vec<(String, u64)> = Vec::new();
    for name in &survivors {
        let ino = kv.lookup(1, name).await.expect("survivor present").ino;
        kv.setattr(ino, Some(MARKER), None, None, None, None, None, None)
            .await
            .expect("confirmer setattr acked");
        marked.push((name.clone(), ino));
    }
    for i in 0..4u32 {
        kv.create(1, &format!("post{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("post-SMO create acked");
    }

    // Crash-equivalent reopen: page-cache bytes only, no shutdown. Both
    // Arcs drop (the routed wrapper co-owns the backend — the in-process
    // flock releases at the last drop).
    drop(routed);
    drop(kv);
    let kv2 = reopen(file.path()).await;

    // The JOINT §1b contract: no acked state lost AND a clean parse from
    // the tail. Collect every violation before asserting so the RED
    // output shows the whole signature.
    let dropped_torn = kv2.replay_stats().dropped_torn;
    let mut lost: Vec<String> = Vec::new();
    for name in &unlinked {
        if kv2.lookup(1, name).await.is_ok() {
            lost.push(format!("{name}: acked unlink resurrected"));
        }
    }
    for (name, ino) in &marked {
        match kv2.getattr(*ino).await {
            Ok(attr) if attr.mode == MARKER => {}
            Ok(attr) => lost.push(format!(
                "{name} (ino {ino}): acked MARKER mode lost (serves {:o})",
                attr.mode
            )),
            Err(e) => lost.push(format!("{name} (ino {ino}): acked inode lost ({e:?})")),
        }
    }
    for i in 0..4u32 {
        let name = format!("post{i}");
        if kv2.lookup(1, &name).await.is_err() {
            lost.push(format!("{name}: acked post-SMO create lost"));
        }
    }
    // The fill records were all checkpoint-covered below the tail: their
    // durability is not §1b's to lose — presence is the sanity check.
    let fill_attr = kv2.getattr(fill_ino).await.expect("fill inode present");
    assert_eq!(
        fill_attr.mode & libc::S_IFMT,
        libc::S_IFREG,
        "fill inode must stay a regular file"
    );
    assert!(
        lost.is_empty() && dropped_torn == 0,
        "FIND-SMO-TAIL (§1b): the checkpoint tail landed strictly inside the \
         victim unlink's journal entry ('{victim}'), so replay parsed mid-entry, \
         resynced at the next page, and dropped acked records \
         (dropped_torn = {dropped_torn}, acked losses = {}): {:?}",
        lost.len(),
        &lost[..lost.len().min(16)]
    );
}

// ---------------------------------------------------------------------------
// Option A — pending-free coverage (docs/design-smo-replay-currency.md §1
// sub-mechanism (ii), §2 Option A, §6 PR 1(b) + PR 4). The gates below
// compare checkpoint GENERATION only (backend `after_durable_barrier` live;
// `alloc_ext::load` at mount), certifying durability of the freeing
// checkpoint RECORD — not coverage of the freeing FLIP/swap. The §4.7
// contract under test: a pending free releases only once the durable
// journal tail passes the free record's own seq (the entry's HIGHEST seq —
// per-SMO-entry floor pinning then guarantees every flip of that entry is
// materialized), and a replayed free (in-window by construction) parks
// until the first post-mount durable checkpoint.
// ---------------------------------------------------------------------------

/// Shared driver for the Option-A tests: one ~2.5 KiB same-ino setattr
/// burst (under the 4 KiB writeback threshold — no maintenance wake), the
/// mid-entry test's fill shape.
async fn fill_burst(kv: &Arc<KvMetaBackend>, ino: u64, flip: u32) {
    for j in 0..24u32 {
        let mode = libc::S_IFREG
            | if (flip + j).is_multiple_of(2) {
                0o640
            } else {
                0o600
            };
        kv.setattr(ino, Some(mode), None, None, None, None, None, None)
            .await
            .expect("fill setattr acked");
    }
}

fn smo_count() -> u64 {
    squeezefs::meta_backend::kv::META_KV_NODE_COMPACTIONS.load(Ordering::Relaxed)
        + squeezefs::meta_backend::kv::META_KV_NODE_SPLITS.load(Ordering::Relaxed)
}

/// Drive burst+checkpoint lifecycles until one cycle's flush pass fires an
/// SMO (the root leaf's log filled: freeze → append fails → compaction =
/// a ROOT SWAP on a depth-1 tree). Returns bursts driven.
async fn drive_until_swap(kv: &Arc<KvMetaBackend>, ino: u64, flip: &mut u32) -> u64 {
    for i in 0..400u64 {
        let before = smo_count();
        fill_burst(kv, ino, *flip).await;
        *flip += 1;
        kv.checkpoint_now().await.expect("lifecycle checkpoint");
        if smo_count() > before {
            return i + 1;
        }
    }
    panic!("a fill lifecycle must trigger the root-swap compaction within budget");
}

/// Sub-mechanism (ii), the LIVE gate (design §1/§2-A; PR 4 row): an SMO's
/// freed extent must stay parked until the **durable journal tail passes
/// the free record's seq** — durability of the freeing checkpoint RECORD
/// is not coverage of the freeing swap. A root swap makes the violation
/// deterministic with zero extra levers: its dying floor (`res.start`)
/// clamps the covering record's tail BELOW the SMO entry, so the
/// generation-only gate provably releases the old root's extent while the
/// swap — the only durable routing to the new root being that same record
/// — still rides the replay window, §4.7's "any state replay can select
/// references only never-overwritten extents" notwithstanding. Post-fix,
/// the free drains one cycle later (the next tail passes the entry): the
/// liveness half.
///
/// RED on dev (generation gate): `pending == 0` immediately after the
/// swap-carrying cycle. GREEN under PR 4: parked through the covering
/// record, drained by the next cycle; the reuse + crash belt stays whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn root_swap_freed_extent_stays_parked_until_tail_covers_free() {
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
        }
    }
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let _cleanup = Cleanup;

    let (routed, kv, file) = sandbox().await;

    // Probe files whose acked state the crash belt re-verifies, plus the
    // fill inode (depth-1 INODES tree: its compaction is a root swap).
    let mut probes: Vec<(String, u64)> = Vec::new();
    for i in 0..24u32 {
        let name = format!("keep{i:03}");
        let ino = kv
            .create(1, &name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("probe create acked")
            .ino;
        probes.push((name, ino));
    }
    let fill_ino = kv
        .create(1, "fillfile", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("fill create acked")
        .ino;
    kv.checkpoint_now().await.expect("setup checkpoint");

    let mut flip = 0u32;
    // Warm one full lifecycle (steady-state fold base), then quiesce: two
    // cycles with no SMO leave every prior free drained in BOTH worlds
    // (the second cycle's tail is the head — nothing pins it).
    drive_until_swap(&kv, fill_ino, &mut flip).await;
    kv.checkpoint_now().await.expect("quiesce cycle 1");
    kv.checkpoint_now().await.expect("quiesce cycle 2");
    assert_eq!(
        kv.pending_free_extents(),
        0,
        "quiesced volume must have drained every prior retirement"
    );

    // The assertion lifecycle: the swap fires INSIDE one checkpoint_now
    // (freeze → append fails → root swap → dying floor res.start →
    // ledger tail < free.seq → barrier). The §4.7 gate under test decides
    // what that barrier may release.
    drive_until_swap(&kv, fill_ino, &mut flip).await;
    assert_eq!(
        kv.pending_free_extents(),
        1,
        "design-smo-replay-currency §1 (ii): the swap-covering ledger record is \
         durable but its tail sits BELOW the SMO entry (dying-floor clamp), so \
         the old root's extent must STAY PARKED — releasing on checkpoint \
         generation alone is the recycled-extent stale-route mechanism \
         (child-seq refusals / acked loss)"
    );

    // Liveness half: the next cycle's tail passes the swap entry (floors
    // discharged), and its barrier drains the free.
    kv.checkpoint_now().await.expect("coverage cycle");
    assert_eq!(
        kv.pending_free_extents(),
        0,
        "once the durable tail passes the free record's seq the extent must \
         drain back to the claimable pool (quarantine must not become a leak)"
    );

    // Reuse + crash belt: another lifecycle claims the lowest free extent
    // (the release hint prefers it — on the broken gate this is the old
    // root's extent while the swap is still window-resident), then a
    // crash-equivalent reopen must neither refuse the mount nor lose
    // acked state.
    drive_until_swap(&kv, fill_ino, &mut flip).await;
    const MARKER: u32 = libc::S_IFREG | 0o751;
    for (_, ino) in &probes {
        kv.setattr(*ino, Some(MARKER), None, None, None, None, None, None)
            .await
            .expect("probe marker acked");
    }
    drop(routed);
    drop(kv);
    let kv2 = reopen(file.path()).await; // a refusal panics here, loud
    for (name, ino) in &probes {
        let got = kv2
            .getattr(*ino)
            .await
            .unwrap_or_else(|e| panic!("acked inode {name} lost across reopen: {e:?}"))
            .mode;
        assert_eq!(got, MARKER, "acked marker on {name} lost across reopen");
    }
    let fill = kv2.getattr(fill_ino).await.expect("fill inode present");
    assert_eq!(fill.mode & libc::S_IFMT, libc::S_IFREG);
}

/// Sub-mechanism (ii), the MOUNT gate (design §2-A "mount side"; PR 4
/// row): a replayed free is **in the replay window by construction** — the
/// mounted tail did not cover its entry, so nothing proves the freeing
/// swap/flips are materialized in the durable structure (for a root swap,
/// the mounted record itself may only exist in page cache after a kill).
/// Replayed frees must therefore PARK until the first post-mount durable
/// checkpoint, not release on `retire_seq ≤ mounted_seq`.
///
/// RED on dev: reopen releases the swap's free at load (`pending == 0`).
/// GREEN under PR 4: parked at mount, drained by the first post-mount
/// checkpoint cycle; acked state whole either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mount_side_replayed_free_parks_until_post_mount_checkpoint() {
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
        }
    }
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let _cleanup = Cleanup;

    let (routed, kv, file) = sandbox().await;

    let fill_ino = kv
        .create(1, "fillfile", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("fill create acked")
        .ino;
    let keep_ino = kv
        .create(1, "keeper", libc::S_IFREG | 0o640, 0, 0)
        .await
        .expect("keeper create acked")
        .ino;
    kv.checkpoint_now().await.expect("setup checkpoint");

    let mut flip = 0u32;
    drive_until_swap(&kv, fill_ino, &mut flip).await;
    kv.checkpoint_now().await.expect("quiesce cycle 1");
    kv.checkpoint_now().await.expect("quiesce cycle 2");
    assert_eq!(kv.pending_free_extents(), 0, "quiesced before the window");

    // The window under test: a root swap whose covering ledger record is
    // written and durable, but whose tail (dying-floor clamped) leaves
    // the swap's whole journal entry — including the free — in the replay
    // window. Kill here: the next mount replays that free.
    drive_until_swap(&kv, fill_ino, &mut flip).await;

    // Crash-equivalent kill: no further cycle, RAM dropped.
    drop(routed);
    drop(kv);
    let kv2 = reopen(file.path()).await;

    assert_eq!(
        kv2.pending_free_extents(),
        1,
        "design-smo-replay-currency §2-A mount gate: a replayed free is \
         in-window by construction — it must PARK until the first post-mount \
         durable checkpoint, never release on the mounted record's generation \
         (the mounted record itself may be page-cache-only after a kill; \
         releasing here is the reuse-vs-fallback §4.7 law violation)"
    );

    // First post-mount durable checkpoint: replayed dirt flushes, the
    // fresh tail covers the window, the barrier drains the free.
    kv2.checkpoint_now()
        .await
        .expect("first post-mount checkpoint");
    assert_eq!(
        kv2.pending_free_extents(),
        0,
        "the first post-mount durable checkpoint must drain the parked free"
    );

    // Integrity: acked state whole across the kill.
    let keeper = kv2.getattr(keep_ino).await.expect("keeper inode present");
    assert_eq!(keeper.mode, libc::S_IFREG | 0o640, "keeper mode intact");
    let fill = kv2.getattr(fill_ino).await.expect("fill inode present");
    assert_eq!(fill.mode & libc::S_IFMT, libc::S_IFREG);
}

/// The §4.7 at-cap law, made mechanism (PR 4 row, review Issue 9 clauses
/// a+b+c): "pressure forces a checkpoint rather than unsafe reuse".
/// Today `free_pending` at cap errors POST-swap (`smo_replace` step 3
/// `?`), aborting the maintenance tick loudly — the extent leaks from the
/// live FIFO (never enters pending, bit set forever) and recovery rides
/// the cadence. Post-fix: a pending-headroom check at SMO admission
/// refuses BEFORE the swap; both `run_maintenance` arms handle
/// `PendingFreeFull` like reserve exhaustion (force `checkpoint_cycle(…,
/// true)` and retry — tolerant of the one no-progress cycle the dying
/// floors impose); the forced cycle COMPLETES (skip-and-defer inside the
/// flush pass), barriers, and drains the FIFO.
///
/// Observable: extents are CONSERVED across at-cap compaction lifecycles
/// (each is claim-1/free-1, net zero). RED on dev: one extent leaks per
/// at-cap SMO. GREEN under PR 4: conservation holds, no livelock, volume
/// healthy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_free_at_cap_forced_cycle_completes_and_conserves_extents() {
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
            TEST_PENDING_FREE_CAP.store(0, Ordering::SeqCst);
        }
    }
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    // The Vyukov-floor minimum: two parked retirements saturate the FIFO.
    TEST_PENDING_FREE_CAP.store(2, Ordering::SeqCst);
    let _cleanup = Cleanup;

    let (_routed, kv, _file) = sandbox().await;

    let fill_ino = kv
        .create(1, "fillfile", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("fill create acked")
        .ino;
    kv.checkpoint_now().await.expect("setup checkpoint");

    // Point A: quiesced, everything drained, K compaction lifecycles in.
    let mut flip = 0u32;
    drive_until_swap(&kv, fill_ino, &mut flip).await;
    kv.checkpoint_now().await.expect("quiesce cycle 1");
    kv.checkpoint_now().await.expect("quiesce cycle 2");
    assert_eq!(kv.pending_free_extents(), 0, "point A quiesced");
    let free_a = kv.free_extents();

    // The at-cap regime: compaction lifecycles driven through THRESHOLD
    // maintenance only (bursts > the 4 KiB writeback delta wake the
    // parked-cadence task's maintenance pass — appends + SMOs, no
    // checkpoint cycle), so retirements accumulate in the FIFO: 1, 2
    // (= cap), then the at-cap SMO. Post-fix the third-and-later
    // lifecycles force inline cycles and drain; on dev each at-cap SMO
    // leaks its old extent.
    let big_burst = |kv: Arc<KvMetaBackend>, flip: u32| async move {
        // ~6 KiB of same-ino setattrs: crosses the writeback threshold,
        // so the commit path enqueues maintenance and wakes the task.
        for j in 0..64u32 {
            let mode = libc::S_IFREG
                | if (flip + j).is_multiple_of(2) {
                    0o640
                } else {
                    0o600
                };
            kv.setattr(fill_ino, Some(mode), None, None, None, None, None, None)
                .await
                .expect("at-cap fill setattr acked");
        }
    };
    // Progress gauge: successor rewrite bytes move at IMAGE-WRITE time —
    // before the step-3 retirement — so it paces the drive in both worlds
    // (on the broken gate `META_KV_NODE_COMPACTIONS` never increments for
    // an at-cap SMO: the counter sits after the failing `free_pending`).
    let rewrite_bytes =
        || squeezefs::meta_backend::kv::META_KV_NODE_REWRITE_BYTES.load(Ordering::Relaxed);
    let rewrites_a = rewrite_bytes();
    let mut swap_attempts = 0u32;
    let mut last_rewrites = rewrites_a;
    for _ in 0..120u32 {
        big_burst(kv.clone(), flip).await;
        flip += 1;
        // Bounded poll: let the woken maintenance pass take its turns.
        for _ in 0..100 {
            if rewrite_bytes() > last_rewrites {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        if rewrite_bytes() > last_rewrites {
            last_rewrites = rewrite_bytes();
            swap_attempts += 1;
            if swap_attempts >= 6 {
                break; // well past the 2-slot FIFO: at-cap SMOs happened
            }
        }
    }
    assert!(
        swap_attempts >= 4,
        "the at-cap drive must reach compaction attempts past the 2-slot \
         FIFO (got {swap_attempts}) — the construction lost its lever"
    );

    // Point B: quiesce. The first cycles flush + cover + drain everything
    // that ever entered the FIFO (tolerating the one no-progress cycle
    // the dying floors impose); extra cycles cover straggler retirements
    // the flush passes themselves produce.
    for _ in 0..4 {
        kv.checkpoint_now().await.expect("point B quiesce cycle");
    }
    assert_eq!(
        kv.pending_free_extents(),
        0,
        "every retirement that entered the FIFO must drain at point B"
    );
    assert!(
        !kv.is_failed(),
        "the at-cap protocol under headroom must keep the volume healthy"
    );
    let free_b = kv.free_extents();
    assert_eq!(
        free_b, free_a,
        "§4.7 at-cap: compaction lifecycles are claim-1/free-1 — extents must \
         be CONSERVED. A deficit is the post-swap PendingFreeFull leak (the \
         extent never entered the pending FIFO; design-smo-replay-currency \
         §2-A at-cap truth / PR 4 clauses a+b+c)"
    );
}

/// The bounded-retry-then-loud terminal (PR 4 row, the `checkpoint_past`
/// precedent): a GENUINELY wedged tail — a floor that no forced cycle can
/// discharge pinned BELOW every parked retirement — must present as a
/// loud volume failure after N forced cycles, never as an unbounded
/// forced-cycle retry loop and never as the silent post-swap leak.
///
/// Wedge construction (healthy I/O, no fault injection): the XATTRS root
/// leaf is positioned log-full-dirty FIRST (its floor predates
/// everything), so every forced cycle's flush pass needs its compaction
/// SMO — which the saturated FIFO refuses at admission headroom
/// (clause a) and the flush pass skips-and-defers (clause c), restoring
/// the ancient floor: the tail can never reach the younger parked
/// retirements, `pending_count` never decreases, and after N cycles the
/// arm fails the volume loud (clause b's terminal).
///
/// RED on dev: the at-cap SMO leaks post-swap and the volume stays
/// "healthy" (silent custody loss). GREEN under PR 4: `is_failed()`
/// latches within the maintenance drive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_free_wedged_tail_fails_volume_loud_never_livelocks() {
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
            TEST_PENDING_FREE_CAP.store(0, Ordering::SeqCst);
        }
    }
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    TEST_PENDING_FREE_CAP.store(2, Ordering::SeqCst);
    let _cleanup = Cleanup;

    let (_routed, kv, _file) = sandbox().await;

    let fill_ino = kv
        .create(1, "fillfile", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("fill create acked")
        .ino;
    let x_ino = kv
        .create(1, "xattrfile", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("xattr host create acked")
        .ino;
    kv.checkpoint_now().await.expect("setup checkpoint");

    // Calibrate the XATTRS root leaf's lifecycle (bursts-to-compaction,
    // measured on the second lifecycle like the fill calibration): xattr
    // bursts touch ONLY the XATTRS tree, so its SMO cadence is
    // independent of the INODES churn below.
    let x_burst = |kv: Arc<KvMetaBackend>, flip: u32| async move {
        for j in 0..24u32 {
            let val = vec![u8::try_from((flip + j) % 251).unwrap(); 48];
            kv.setxattr(x_ino, "user.wedge", &val)
                .await
                .expect("xattr burst acked");
        }
    };
    let mut x_flip = 0u32;
    let mut k_bursts: Option<u64> = None;
    let mut this_lifecycle = 0u64;
    let mut lifecycles = 0u32;
    for i in 0..400u64 {
        assert!(i < 399, "xattr lifecycles must converge within the budget");
        let before = smo_count();
        x_burst(kv.clone(), x_flip).await;
        x_flip += 1;
        kv.checkpoint_now()
            .await
            .expect("xattr calibration checkpoint");
        this_lifecycle += 1;
        if smo_count() > before {
            lifecycles += 1;
            k_bursts = Some(this_lifecycle);
            this_lifecycle = 0;
            if lifecycles == 2 {
                break;
            }
        }
    }
    let k = k_bursts.expect("an xattr lifecycle completed");
    assert!(k >= 3, "a 64 KiB leaf must absorb several xattr bursts");

    // Position the XATTRS leaf at K-1 covered appends (all cycled), then
    // ONE un-cycled burst: its floor is now the OLDEST live floor, and
    // the leaf's next freeze-append must overflow into the compaction
    // path. Quiesce the INODES side first so the FIFO is empty.
    let mut placed = 0u64;
    while placed < k - 1 {
        let before = smo_count();
        x_burst(kv.clone(), x_flip).await;
        x_flip += 1;
        kv.checkpoint_now()
            .await
            .expect("xattr position checkpoint");
        placed += 1;
        if smo_count() > before {
            placed = 0;
        }
    }
    kv.checkpoint_now().await.expect("wedge quiesce");
    assert_eq!(kv.pending_free_extents(), 0, "FIFO empty before the wedge");
    x_burst(kv.clone(), x_flip).await; // the ancient floor, never cycled

    // Saturate the FIFO with two YOUNGER retirements (threshold-driven
    // INODES compactions, no cycles — their frees' seqs all post-date
    // the xattr floor), then trigger the at-cap SMO. Every forced cycle
    // must now skip the xattr leaf (headroom refused) and restore its
    // ancient floor: tail pinned below the parked frees, no progress,
    // loud terminal after N cycles.
    // Each ~6 KiB burst wakes one maintenance pass; a mutation failing
    // AFTER the loud latch is the expected fail-stop face (EIO), never a
    // test error — a failure WITHOUT the latch is.
    let big_burst = |kv: Arc<KvMetaBackend>, flip: u32| async move {
        for j in 0..64u32 {
            let mode = libc::S_IFREG
                | if (flip + j).is_multiple_of(2) {
                    0o640
                } else {
                    0o600
                };
            if let Err(e) = kv
                .setattr(fill_ino, Some(mode), None, None, None, None, None, None)
                .await
            {
                assert!(
                    kv.is_failed(),
                    "wedge setattr failed while the volume was still healthy: {e:?}"
                );
                return;
            }
        }
    };
    let mut failed_loud = false;
    'wedge: for flip in 0..120u32 {
        big_burst(kv.clone(), flip).await;
        for _ in 0..30 {
            if kv.is_failed() {
                failed_loud = true;
                break 'wedge;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }
    assert!(
        failed_loud,
        "a genuinely wedged tail with the FIFO at cap must FAIL THE VOLUME \
         LOUD after bounded forced cycles (design-smo-replay-currency PR 4, \
         the checkpoint_past precedent) — neither a silent post-swap leak \
         (dev: the extent falls out of the live FIFO and the volume keeps \
         acking) nor an unbounded forced-cycle retry loop"
    );
}

/// PR 1(c) fixture-replay (design-smo-replay-currency §6): the §2-A gate
/// decision over a REAL captured post-kill storm window. The chartered
/// fixture source was a REFUSED round; post-C′/PR-3 dev no longer
/// produces one at observable rate (8/8 recapture rounds CLEAN,
/// 2026-07-16 — C′ closed the walks that *detected* the reuse), so the
/// committed fixture (`.agents/findvsa/extract_pending_free_fixture.py`
/// over `recapture.sh` round 8, kvparse.py-derived; full dump in
/// `.agents/findvsa/capture-2026-07-16-pendingfree-expectations.txt` —
/// the FIND-VS-A harness was removed from the tree, git history at
/// `c615e3a`)
/// pins the mechanism's PRECONDITION from real bytes instead — every
/// in-window `Freed` final across the four post-kill meta volumes, with
/// the mounted record's seq + tail:
///
/// 1. **Every checkpoint-referenced free in the window is uncovered**
///    (`rec_seq ≥ mounted_tail` — in-window by construction): §2-A's
///    premise, verified against real storm bytes, so "park them all at
///    mount" releases nothing a durable tail already covers.
/// 2. **The generation gate really released at load**: a substantial
///    subset carries `retire_tag ≤ mounted_seq` (71/231 in the captured
///    round) — extents the OLD mount gate handed back to the claimable
///    pool while their freeing swap/flips rode the replay window. Those
///    are the reuse-vs-fallback §4.7 law violations the coverage gate
///    parks.
/// 3. **The coverage gate parks 100 % of them** (`coverage_gate_parked ==
///    in_window_frees`): the fixed mount decision, recomputed from the
///    same rows.
#[test]
fn recaptured_window_pins_generation_gate_release() {
    let raw = std::fs::read_to_string(format!(
        "{}/tests/fixtures/findvsa3_pending_free_window.jsonl",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture present");
    let mut lines = raw.lines();
    let hdr: serde_json::Value =
        serde_json::from_str(lines.next().expect("header line")).expect("header json");
    assert_eq!(hdr["row"], "header");
    let images = hdr["images"].as_u64().expect("images");
    let in_window = hdr["in_window_frees"].as_u64().expect("in_window_frees");
    let gen_released = hdr["generation_gate_released"]
        .as_u64()
        .expect("generation_gate_released");
    assert_eq!(images, 4, "the capture spans all four meta volumes");
    assert!(
        in_window >= 100,
        "a storm-peak kill leaves a substantial in-window retirement \
         population (got {in_window})"
    );
    assert!(
        gen_released >= 1,
        "the captured round must exhibit the §2-A face: generation-covered \
         frees inside the replay window (the OLD mount gate released these \
         at load while nothing durable covered their freeing swaps)"
    );

    let mut rows = 0u64;
    let mut recomputed_released = 0u64;
    let mut recomputed_parked = 0u64;
    for line in lines {
        let v: serde_json::Value = serde_json::from_str(line).expect("row json");
        assert_eq!(v["row"], "free");
        rows += 1;
        let rec_seq = v["rec_seq"].as_u64().expect("rec_seq");
        let retire_tag = v["retire_tag"].as_u64().expect("retire_tag");
        let mounted_seq = v["mounted_seq"].as_u64().expect("mounted_seq");
        let mounted_tail = v["mounted_tail"].as_u64().expect("mounted_tail");
        assert!(retire_tag > 0, "sentinel frees are excluded by extraction");
        // Pin 1: in-window by construction — nothing durable covers it.
        assert!(
            rec_seq >= mounted_tail,
            "extent {} carries a free BELOW the mounted tail — a replayed \
             free outside the window contradicts the §2-A premise",
            v["extent"]
        );
        // Pin 2: the OLD gate's decision, recomputed from the raw fields.
        let old_gate_released = retire_tag <= mounted_seq;
        assert_eq!(
            old_gate_released,
            v["generation_gate_released"].as_bool().expect("flag"),
            "extractor/consumer disagree on the generation-gate decision"
        );
        recomputed_released += u64::from(old_gate_released);
        // Pin 3: the coverage gate parks it (release needs tail > rec_seq).
        if mounted_tail <= rec_seq {
            recomputed_parked += 1;
        }
    }
    assert_eq!(rows, in_window, "header totals match the rows");
    assert_eq!(
        recomputed_released, gen_released,
        "the §2-A violation count reproduces from the committed bytes"
    );
    assert_eq!(
        recomputed_parked, in_window,
        "the coverage gate parks EVERY in-window free — the fixed mount \
         decision over the same real-storm rows"
    );
}
