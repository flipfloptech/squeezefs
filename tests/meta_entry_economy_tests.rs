//! PR M2 — journal-entry economy pins (design-metadata-throughput §5.4
//! D4.a, resolving Open Question 1) + the D4.c fill-vs-window attribution
//! counters.
//!
//! The baseline (`.benchmarks/2026-07-14-metadata-throughput-baseline.md`)
//! measured, through the mount: create **1.006** journal entries/op,
//! rename **2.002**, unlink **2.020** — and the design deliberately left
//! WHERE the second rename/unlink entry comes from to a pin test. These
//! tests settle it with mount-shaped storms through `RoutedMetaBackend`
//! (the exact handler call sequence, including the kernel's post-op ctime
//! writeback echo — `fuse_update_ctime` → `fuse_flush_times` →
//! `FUSE_SETATTR(FATTR_MTIME|FATTR_CTIME)` — visible in the baseline's own
//! per-phase snapshots as `meta_updates` = 2.0/op during the rename AND
//! unlink phases) and a `commit_tx` call-site attribution hook
//! (`META_KV_COMMIT_SITES`, `#[track_caller]`-captured `KvTx` construction
//! sites):
//!
//! - **create = 1 entry/op** (the whole-tx `routed_create_local` commit).
//! - **rename = 1 entry/op at the backend** (`routed_rename_local` is ONE
//!   `commit_tx` — exonerated); the mount's second entry is the **adjacent
//!   handler-path commit**: the kernel's ctime-flush SETATTR landing in
//!   `setattr_locked`. Site-attributed, not inferred.
//! - **unlink = 1 entry/op at the backend** plus the same SETATTR echo
//!   (the whole second entry) plus **1/batch_fill** from the FORGET-side
//!   `destroy_inodes` batches. The design's stated prior — "2.020 ⇒
//!   destroy-batch fill ≈ 1" — is REFUTED: fill is cap-sized (the
//!   baseline's own `meta_reclaim_batch_size` deltas show ~63/batch:
//!   1,579 batches ≤64 for 100 k unlinks), contributing only ~0.016/op.
//!   The model that fits is `1 (unlink tx) + 1 (SETATTR echo) + 1/fill +
//!   ambient`.
//! - **zero-commit teardown** (`routing.rs` `delete_file`): reclaim's
//!   data-path teardown of an inline/empty corpse lands ZERO journal
//!   entries — the comment-only contract at routing.rs:5754-5759 becomes
//!   a regression pin.
//! - `drain_reclaim_batch` records the D4.c **fill-vs-window attribution**:
//!   gather fill + how each batch closed (cap / window expiry / channel
//!   close), so a future fill degeneration is attributable from `.stats`
//!   alone.
//!
//! Runs against local file-backed MetaLV sandboxes (no root, no mount).

use squeezefs::fuse_client::METRICS;
use squeezefs::meta_backend::kv::{commit_sites_snapshot, META_KV_JOURNAL_ENTRIES};
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::NamedTempFile;

/// Format + open one v3 metadata volume for this harness (the
/// `reclaim_batch_tests` pattern).
async fn open_v3_meta(
    path: &std::path::Path,
    len: u64,
) -> Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
    squeezefs::meta_backend::kv::builder::format_v3(
        path,
        len,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3 meta volume");
    squeezefs::meta_backend::kv::backend::KvMetaBackend::open(path)
        .await
        .expect("open v3 meta volume")
}

async fn routed_sandbox() -> (NamedTempFile, Arc<RoutedMetaBackend>) {
    let tmp = NamedTempFile::new().unwrap();
    let kv = open_v3_meta(tmp.path(), 256 * 1024 * 1024).await;
    (tmp, Arc::new(RoutedMetaBackend::new(vec![kv])))
}

fn entries_now() -> u64 {
    META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed)
}

fn sites_now() -> HashMap<String, u64> {
    commit_sites_snapshot().into_iter().collect()
}

/// Commit-site deltas (site → count moved) across `f`.
fn site_deltas(
    before: &HashMap<String, u64>,
    after: &HashMap<String, u64>,
) -> HashMap<String, u64> {
    let mut out = HashMap::new();
    for (k, v) in after {
        let d = v - before.get(k).copied().unwrap_or(0);
        if d > 0 {
            out.insert(k.clone(), d);
        }
    }
    out
}

/// Run one probe op and return the single commit site it moved — the
/// calibration read that lets assertions NAME a call site without pinning
/// brittle line numbers.
async fn calibrate_single_site<F, Fut>(what: &str, f: F) -> String
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let before = sites_now();
    f().await;
    let moved = site_deltas(&before, &sites_now());
    assert_eq!(
        moved.len(),
        1,
        "{what} must commit through exactly one commit_tx call site, moved: {moved:?}"
    );
    let (site, n) = moved.into_iter().next().unwrap();
    assert_eq!(n, 1, "{what} must land exactly one commit at {site}");
    site
}

/// The kernel's post-mutation ctime writeback echo: after rename/unlink the
/// FUSE kernel dirties the inode's ctime locally (`fuse_update_ctime`) and
/// flushes it as `FUSE_SETATTR(FATTR_MTIME|FATTR_CTIME)` — the daemon's
/// setattr handler forwards it as a times-only `Metadata::setattr`. The
/// baseline's per-phase `.stats` snapshots pin this shape: `meta_updates`
/// (bumped once per mutation HANDLER) moved 200,000 for 100 k renames and
/// 200,000 for 100 k unlinks.
async fn kernel_ctime_flush_echo(backend: &RoutedMetaBackend, ino: u64, now_ns: u64) {
    backend
        .setattr(
            ino,
            None,
            None,
            None,
            None,
            None,
            Some(now_ns),
            Some(now_ns),
        )
        .await
        .expect("times-only setattr (the kernel ctime-flush echo)");
}

/// Baseline row `create = 1.006 entries/op`: through the mount-shaped call
/// sequence (kernel LOOKUP miss, then CREATE) each create lands exactly ONE
/// whole-tx journal entry; the measured +0.006 is ambient (SMO /
/// checkpoint-class records), not a second committer.
#[tokio::test]
async fn mount_shaped_create_storm_lands_one_journal_entry_per_op() {
    let (_t, backend) = routed_sandbox().await;
    const N: u64 = 256;

    let e0 = entries_now();
    for i in 0..N {
        let name = format!("f{i:07}");
        // Kernel LOOKUP first — must be a pure read (ENOENT).
        assert!(
            backend.lookup(1, &name).await.is_err(),
            "pre-create lookup of a fresh name must miss"
        );
        backend
            .create(1, &name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("storm create");
    }
    let delta = entries_now() - e0;
    assert!(
        (N..=N + N / 32).contains(&delta),
        "create must be ~1 journal entry/op (lookup commits nothing): \
         {delta} entries for {N} creates"
    );
}

/// OQ 1, rename half. (a) The backend rename path — the FUSE handler's
/// exact sequence: dest-probe lookup + `Metadata::rename` — is ONE journal
/// entry per op: `routed_rename_local` is exonerated. (b) Adding the
/// kernel's ctime-flush SETATTR echo reproduces the measured 2.002/op
/// shape. (c) The commit-site attribution NAMES the second committer: the
/// times-only setattr commit (`setattr_locked`'s tx), a different call
/// site from the rename tx, both in the kv backend.
#[tokio::test]
async fn rename_second_committer_is_the_ctime_flush_setattr_not_the_rename_tx() {
    let (_t, backend) = routed_sandbox().await;
    const N: u64 = 256;

    for i in 0..N {
        backend
            .create(1, &format!("f{i:07}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("seed create");
    }

    // Calibrate the two candidate committers' sites with single-op probes.
    let b = backend.clone();
    let rename_site = calibrate_single_site("rename", || async move {
        let _ = b.lookup(1, "r_cal").await; // dest probe (miss, no commit)
        b.rename(1, "f0000000", 1, "r_cal", 0)
            .await
            .expect("probe rename");
    })
    .await;
    let b = backend.clone();
    let setattr_site = calibrate_single_site("times-only setattr", || async move {
        let ino = b.lookup(1, "r_cal").await.expect("probe target").ino;
        kernel_ctime_flush_echo(&b, ino, 1_000_000_007).await;
    })
    .await;
    assert_ne!(
        rename_site, setattr_site,
        "rename tx and setattr tx must be distinct commit_tx call sites"
    );
    for site in [&rename_site, &setattr_site] {
        assert!(
            site.contains("meta_backend/kv/backend.rs"),
            "committer site must be a kv backend transaction, got {site}"
        );
    }

    // (a) Backend shape (no kernel echo): 1 entry/op — the engine path is
    // exonerated; whatever companion commit the mount measures is NOT in
    // routed_rename_local.
    let e0 = entries_now();
    let s0 = sites_now();
    for i in 1..N {
        let src = format!("f{i:07}");
        let dst = format!("r{i:07}");
        assert!(
            backend.lookup(1, &dst).await.is_err(),
            "dest probe must miss"
        );
        backend
            .rename(1, &src, 1, &dst, 0)
            .await
            .expect("storm rename");
    }
    let backend_delta = entries_now() - e0;
    let backend_sites = site_deltas(&s0, &sites_now());
    assert!(
        (N - 1..=(N - 1) + 8).contains(&backend_delta),
        "backend-shape rename must be ~1 entry/op (got {backend_delta} for {} renames)",
        N - 1
    );
    assert_eq!(
        backend_sites.get(&rename_site).copied().unwrap_or(0),
        N - 1,
        "every storm rename commits once through the rename tx site {rename_site}"
    );
    assert!(
        !backend_sites.contains_key(&setattr_site),
        "no setattr commits in the backend-shape storm — the second entry \
         is not the engine's doing"
    );

    // (b)+(c) Mount shape (with the kernel ctime-flush echo): 2 entries/op,
    // second committer NAMED as the setattr site.
    let e1 = entries_now();
    let s1 = sites_now();
    for i in 1..N {
        let dst = format!("r{i:07}");
        let src = format!("q{i:07}");
        backend
            .rename(1, &dst, 1, &src, 0)
            .await
            .expect("storm rename back");
        let ino = backend.lookup(1, &src).await.expect("renamed file").ino;
        kernel_ctime_flush_echo(&backend, ino, 2_000_000_000 + i).await;
    }
    let mount_delta = entries_now() - e1;
    let mount_sites = site_deltas(&s1, &sites_now());
    let per_op = mount_delta as f64 / (N - 1) as f64;
    assert!(
        (1.99..=2.05).contains(&per_op),
        "mount-shaped rename (rename + ctime-flush SETATTR) must land \
         ~2.0 entries/op — the baseline's measured 2.002 — got {per_op:.3}"
    );
    assert_eq!(
        mount_sites.get(&rename_site).copied().unwrap_or(0),
        N - 1,
        "first committer: the rename tx at {rename_site}"
    );
    assert_eq!(
        mount_sites.get(&setattr_site).copied().unwrap_or(0),
        N - 1,
        "SECOND committer: the kernel ctime-flush SETATTR landing at \
         {setattr_site} — OQ 1 (rename) settled: an adjacent handler-path \
         commit, absorbed by D4.b's one-tx shape"
    );
}

/// OQ 1, unlink half — the `1 + 1/fill + ambient` model, decomposed and
/// pinned term by term:
///   - the unlink tx itself: 1 entry/op;
///   - the kernel ctime-flush SETATTR echo: the whole second entry
///     (site-attributed);
///   - the FORGET-side `destroy_inodes` batches: 1 entry per BATCH — with
///     the gather working (fill = cap = 64), that is 1/64 ≈ 0.016/op,
///     matching the baseline's 2.020 vs rename's 2.002. The design's
///     round-1 prior "fill ≈ 1 explains the 2.020" is hereby REFUTED: a
///     singleton-fill mechanism would land ~3.0 entries/op in this storm
///     (1 + 1 + 1), and the baseline's own snapshots recorded 1,579
///     destroy batches (≤64 bucket) for 100 k unlinks — fill ≈ 63.
#[tokio::test]
async fn unlink_entry_economy_model_batch_fill_is_large_not_the_second_entry() {
    let (_t, backend) = routed_sandbox().await;
    const N: u64 = 320;
    const FILL: usize = 64; // SQUEEZEFS_RECLAIM_BATCH default

    let mut inos = Vec::new();
    for i in 0..N {
        let f = backend
            .create(1, &format!("u{i:07}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("seed create");
        inos.push(f.ino);
    }

    // Calibrate the setattr committer site.
    let b = backend.clone();
    let probe_ino = inos[0];
    let setattr_site = calibrate_single_site("times-only setattr", || async move {
        kernel_ctime_flush_echo(&b, probe_ino, 41).await;
    })
    .await;

    // Term 1: the unlink tx alone is ONE entry/op.
    let e0 = entries_now();
    let s0 = sites_now();
    for i in 0..N {
        let child = backend
            .unlink(1, &format!("u{i:07}"))
            .await
            .expect("storm unlink");
        assert_eq!(
            child, inos[i as usize],
            "unlink returns the doomed child ino"
        );
    }
    let unlink_delta = entries_now() - e0;
    assert!(
        (N..=N + 8).contains(&unlink_delta),
        "unlink tx must be ~1 entry/op (got {unlink_delta} for {N})"
    );
    let unlink_sites = site_deltas(&s0, &sites_now());
    assert_eq!(
        unlink_sites.len(),
        1,
        "the unlink storm commits through exactly one site: {unlink_sites:?}"
    );

    // Term 2: the kernel ctime-flush echo — the WHOLE second entry, at the
    // setattr site (the unlinked-but-not-yet-destroyed inode record is
    // still writable: FUSE keeps it alive until FORGET).
    let e1 = entries_now();
    let s1 = sites_now();
    for (i, &ino) in inos.iter().enumerate() {
        kernel_ctime_flush_echo(&backend, ino, 3_000_000_000 + i as u64).await;
    }
    let echo_delta = entries_now() - e1;
    let echo_sites = site_deltas(&s1, &sites_now());
    assert!(
        (N..=N + 8).contains(&echo_delta),
        "the ctime-flush echo adds ~1 entry/op (got {echo_delta} for {N})"
    );
    assert_eq!(
        echo_sites.get(&setattr_site).copied().unwrap_or(0),
        N,
        "OQ 1 (unlink) settled: the second per-op entry is the kernel \
         ctime-flush SETATTR at {setattr_site}, not reclaim"
    );

    // Term 3: FORGET-side destroys at healthy fill — 1 entry per BATCH,
    // recorded in the destroy-fill histogram's ≤64 bucket (index 7 of the
    // QueueDepthHistogram labels: 0,1,2,≤4,≤8,≤16,≤32,≤64,…).
    let fills_le64_before = METRICS.meta_reclaim_batch_size.buckets[7].load(Ordering::Relaxed);
    let e2 = entries_now();
    for chunk in inos.chunks(FILL) {
        backend
            .destroy_inodes(chunk)
            .await
            .expect("batched FORGET-side destroy");
    }
    let destroy_delta = entries_now() - e2;
    let n_batches = inos.len().div_ceil(FILL) as u64;
    assert_eq!(
        destroy_delta, n_batches,
        "destroys amortize to ONE entry per batch ({n_batches} batches for {N} corpses)"
    );
    let fills_le64_after = METRICS.meta_reclaim_batch_size.buckets[7].load(Ordering::Relaxed);
    assert_eq!(
        fills_le64_after - fills_le64_before,
        n_batches,
        "every destroy batch records its fill in the ≤64 bucket (fill = cap, not 1)"
    );

    // The model, assembled: 1 + 1 + 1/fill (+0 ambient in this sandbox).
    let total = unlink_delta + echo_delta + destroy_delta;
    let per_op = total as f64 / N as f64;
    assert!(
        (2.0..=2.05).contains(&per_op),
        "unlink economy must fit 1 + 1 + 1/fill ≈ 2.016 — the measured \
         2.020 shape — got {per_op:.3}"
    );
    assert!(
        per_op < 2.5,
        "a singleton destroy-fill mechanism (the refuted prior) would land \
         ≥ 3.0 entries/op; measured {per_op:.3}"
    );
}

/// The already-true teardown economy, pinned (design §5.4 D4.c pt 2): the
/// reclaim worker's `delete_file` on an inline/empty corpse lands ZERO
/// journal entries — the routing.rs:5754-5759 no-per-corpse-commit
/// contract, currently comment-only, so a future data-path change cannot
/// silently reintroduce a per-corpse commit. `destroy_inodes` covers the
/// slot (and the xattr block) inside its own batched commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unlink_teardown_delete_file_lands_zero_journal_entries() {
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::cache::TieredCache;
    use squeezefs::dlm::DlmClient;
    use squeezefs::nvme_dev::NvmeBlockDev;
    use squeezefs::routing::DataRouter;

    let dlm = DlmClient::new("local").unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "entry_economy_teardown")
            .await
            .unwrap(),
    );
    let s = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("32MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let m = NamedTempFile::new().unwrap();
    let routed = Arc::new(RoutedMetaBackend::new(vec![
        open_v3_meta(m.path(), 256 * 1024 * 1024).await,
    ]));
    router.set_meta_backend(routed.clone());

    // An inline/empty corpse: created, unlinked, awaiting FORGET-side
    // reclaim — the exact delete_file caller shape.
    let f = routed
        .create(1, "corpse", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create corpse");
    routed.unlink(1, "corpse").await.expect("unlink corpse");

    let mut con = dlm.get_connection().await.expect("dlm connection");
    let e0 = entries_now();
    router
        .delete_file(&squeezefs::keys::inode_path(f.ino), &mut con)
        .await
        .expect("reclaim data-path teardown");
    assert_eq!(
        entries_now() - e0,
        0,
        "delete_file on an inline/empty corpse must land ZERO journal \
         entries (routing.rs no-per-corpse-commit contract) — \
         destroy_inodes owns the slot + xattr reap"
    );

    // And the batched destroy that owns the commit: exactly one entry.
    let e1 = entries_now();
    routed
        .destroy_inodes(&[f.ino])
        .await
        .expect("batched destroy");
    assert_eq!(
        entries_now() - e1,
        1,
        "the corpse's slot zero rides destroy_inodes' own single entry"
    );
}

/// D4.c fill-vs-window attribution: `drain_reclaim_batch` counts HOW each
/// gather closed (cap / window expiry / channel close) and the post-dedup
/// fill it produced — the counters that decide between the batch-fill
/// degeneration suspects (per-FORGET spawn jitter vs window-vs-arrival
/// cadence vs semaphore splitting) from `.stats` alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reclaim_gather_close_reasons_and_fill_are_counted() {
    use squeezefs::fuse_client::drain_reclaim_batch;
    let cap_before = METRICS
        .meta_reclaim_gather_cap_closes
        .load(Ordering::Relaxed);
    let win_before = METRICS
        .meta_reclaim_gather_window_closes
        .load(Ordering::Relaxed);
    let chan_before = METRICS
        .meta_reclaim_gather_channel_closes
        .load(Ordering::Relaxed);
    let fill_hist = |i: usize| METRICS.meta_reclaim_gather_fill.buckets[i].load(Ordering::Relaxed);

    // (1) Cap close: 100 buffered inos, cap 64 — the first drain fills to
    // the cap without waiting out the window.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<u64>(1024);
    for ino in 100..200u64 {
        tx.try_send(ino).expect("buffered send");
    }
    let fill64_before = fill_hist(7); // "<=64"
    let batch = drain_reclaim_batch(&mut rx, 64, std::time::Duration::from_millis(20))
        .await
        .expect("cap batch");
    assert_eq!(batch.len(), 64, "cap-bounded gather");
    assert_eq!(
        METRICS
            .meta_reclaim_gather_cap_closes
            .load(Ordering::Relaxed),
        cap_before + 1,
        "a cap-sized gather counts one cap close"
    );
    assert_eq!(
        fill_hist(7),
        fill64_before + 1,
        "the gather fill histogram records the 64-fill batch"
    );

    // (2) Window close: the 36 leftovers drain instantly, the sender stays
    // alive and the cap is unmet — the gather must give up at window
    // expiry and count it.
    let leftover = drain_reclaim_batch(&mut rx, 64, std::time::Duration::from_millis(10))
        .await
        .expect("leftover batch");
    assert_eq!(leftover.len(), 100 - 64, "leftover fill");
    let win_mid = METRICS
        .meta_reclaim_gather_window_closes
        .load(Ordering::Relaxed);
    assert!(
        win_mid > win_before,
        "a window-expiry gather (sender alive, cap unmet) counts a window close"
    );

    // (3) Channel close: pending inos + dropped sender — the gather drains
    // what is there and counts the close. Duplicate inos dedup, and the
    // FILL histogram records the post-dedup size.
    tx.try_send(7).unwrap();
    tx.try_send(7).unwrap();
    tx.try_send(8).unwrap();
    drop(tx);
    let fill2_before = fill_hist(2); // "2"
    let last = drain_reclaim_batch(&mut rx, 64, std::time::Duration::from_millis(20))
        .await
        .expect("final batch");
    assert_eq!(last, vec![7, 8], "gather dedups repeat FORGETs");
    assert_eq!(
        METRICS
            .meta_reclaim_gather_channel_closes
            .load(Ordering::Relaxed),
        chan_before + 1,
        "a closed-channel gather counts one channel close"
    );
    assert_eq!(
        fill_hist(2),
        fill2_before + 1,
        "the fill histogram records the POST-DEDUP size (2, not 3)"
    );

    // Closed-and-empty: the worker loop's exit — no batch, no counters.
    assert!(
        drain_reclaim_batch(&mut rx, 64, std::time::Duration::from_millis(5))
            .await
            .is_none(),
        "closed empty channel ends the gather loop"
    );
}

// ---------------------------------------------------------------------
// D1.a attribution rig (design-metadata-throughput §5.1): the
// SQUEEZEFS_OP_PROFILE-gated per-op phase profile, the under-`i_rwsem`
// span estimator, and the watchdog-ready op registry scaffolding.
// ---------------------------------------------------------------------

use squeezefs::fuse_client::{
    op_profile_enabled, op_profile_inflight, op_profile_phase_json, op_profile_under_lock_json,
    FuseOpKind, OpProf,
};

fn hist_total(hist: &serde_json::Value) -> u64 {
    hist.as_object()
        .expect("histogram object")
        .values()
        .map(|v| v.as_u64().unwrap_or(0))
        .sum()
}

fn phase_total(json: &serde_json::Value, op: &str, phase: &str) -> u64 {
    hist_total(&json[op][phase])
}

/// The rig's gate is a LAUNCH-TIME knob, memoized like `SQUEEZEFS_TIMEOUT`
/// (`get_fuse_timeout`) — zero per-op env reads, and default OFF: with the
/// variable unset the rig must add nothing to any hot path (`OpProf::begin`
/// = one memoized atomic load + branch → `None`).
#[test]
fn op_profile_gate_is_memoized_and_default_off() {
    assert!(
        !op_profile_enabled(),
        "SQUEEZEFS_OP_PROFILE unset ⇒ the rig is OFF by default"
    );
    assert!(
        OpProf::begin(FuseOpKind::Create, 1).is_none(),
        "disabled rig hands handlers None — no stamps, no slots"
    );
    std::env::set_var("SQUEEZEFS_OP_PROFILE", "1");
    assert!(
        !op_profile_enabled(),
        "the gate consulted the environment after first resolution — a \
         per-op env::var read on the hot path"
    );
    std::env::remove_var("SQUEEZEFS_OP_PROFILE");
}

/// Phase recording + registry lifecycle: begin claims a registry slot
/// (the D1.b watchdog's future scan surface — (op, ino, start)), the
/// marks split the op into handler→backend / backend / backend→reply /
/// total, drop records all four and releases the slot. Slot exhaustion
/// degrades to unregistered-but-still-profiled (never blocks, never
/// loses the histogram sample).
#[tokio::test]
async fn op_prof_records_phases_and_recycles_registry_slots() {
    let inflight0 = op_profile_inflight();
    let before = op_profile_phase_json();

    let p = OpProf::begin_forced(FuseOpKind::Create, 42);
    assert_eq!(
        op_profile_inflight(),
        inflight0 + 1,
        "begin claims a registry slot"
    );
    p.mark_backend_start();
    p.mark_backend_done();
    drop(p);
    assert_eq!(
        op_profile_inflight(),
        inflight0,
        "drop releases the registry slot"
    );

    let after = op_profile_phase_json();
    for phase in ["handler_to_backend", "backend", "backend_to_reply", "total"] {
        assert_eq!(
            phase_total(&after, "create", phase),
            phase_total(&before, "create", phase) + 1,
            "one create op records one sample in the {phase} phase"
        );
    }

    // Exhaustion: more live ops than slots must not block or panic; the
    // overflow ops still profile (histograms move), just unregistered.
    let before_total = phase_total(&op_profile_phase_json(), "getattr", "total");
    let herd: Vec<OpProf> = (0..300)
        .map(|i| OpProf::begin_forced(FuseOpKind::Getattr, i))
        .collect();
    assert!(
        op_profile_inflight() <= inflight0 + 256,
        "registry is a FIXED slab (256 slots)"
    );
    drop(herd);
    assert_eq!(
        op_profile_inflight(),
        inflight0,
        "every claimed slot is released on drop"
    );
    assert_eq!(
        phase_total(&op_profile_phase_json(), "getattr", "total"),
        before_total + 300,
        "slot exhaustion never drops histogram samples"
    );
}

/// The under-`i_rwsem` span estimator (§5.1 artifact 1): a create's
/// preceding LOOKUP is paired by (parent, name) through a bounded
/// latch-free table; LOOKUP-arrival → CREATE-reply lands in
/// `fuse_create_under_lock_ns`. One lookup arms exactly one pairing
/// (the slot is consumed), and an unpaired create records nothing.
#[tokio::test]
async fn under_lock_estimator_pairs_lookup_arrival_to_create_reply() {
    let before = hist_total(&op_profile_under_lock_json());

    // LOOKUP arrival, then the paired CREATE reply.
    let lk = OpProf::begin_forced(FuseOpKind::Lookup, 1);
    lk.note_lookup_arrival(1, "pair_me");
    drop(lk);
    let cr = OpProf::begin_forced(FuseOpKind::Create, 1);
    cr.pair_create_reply(1, "pair_me");
    drop(cr);
    assert_eq!(
        hist_total(&op_profile_under_lock_json()),
        before + 1,
        "a (parent, name)-paired LOOKUP→CREATE records one under-lock span"
    );

    // The pairing slot is consumed: a second create of the same name
    // without a fresh lookup records nothing.
    let cr2 = OpProf::begin_forced(FuseOpKind::Create, 1);
    cr2.pair_create_reply(1, "pair_me");
    drop(cr2);
    assert_eq!(
        hist_total(&op_profile_under_lock_json()),
        before + 1,
        "a consumed pairing slot must not double-count"
    );

    // Never-looked-up name: nothing to pair.
    let cr3 = OpProf::begin_forced(FuseOpKind::Create, 1);
    cr3.pair_create_reply(1, "never_looked_up");
    drop(cr3);
    assert_eq!(
        hist_total(&op_profile_under_lock_json()),
        before + 1,
        "an unpaired create records no span"
    );
}
