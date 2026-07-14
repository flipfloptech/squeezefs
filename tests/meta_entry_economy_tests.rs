//! Journal-entry economy: the PR M2 attribution pins (design-
//! metadata-throughput §5.4 D4.a, which resolved Open Question 1) tightened
//! to the **PR M6 G4 thresholds** — rename ≤ 1.02 and unlink ≤ 1.05
//! entries/op — plus the D4.c fill-vs-window attribution counters.
//!
//! History: the baseline measured, through the mount, create **1.006**
//! entries/op, rename **2.002**, unlink **2.020**. M2's pins named the
//! second rename/unlink entry on a live mount: the kernel's post-op ctime
//! writeback (`fuse_update_ctime` → `fuse_flush_times` →
//! `FUSE_SETATTR(FATTR_MTIME|FATTR_CTIME)`, exactly 1.000/op under
//! writeback cache) landing as a times-only `Metadata::setattr` commit at
//! `setattr_locked` — and REFUTED the destroy-batch-fill prior (fill ≈ 62,
//! contributing only 1/fill ≈ 0.016/op). Model:
//! `2.019 ≈ 1 (op tx) + 1.000 (SETATTR echo) + 0.016 (destroy) + ambient`.
//!
//! PR M6 closes G4 on both terms the measurements left standing:
//!
//! - **D4.b rename one-tx shape**: `routed_rename_local` carries parent
//!   Δtimes (shared-parent-safe merge records), the moved inode's Δctime,
//!   and dest accounting in ONE `KvTx` — still exactly one entry/op at the
//!   rename site (records grew; entries did not), closing the POSIX
//!   parent-mtime-on-rename gap and removing the crash window between the
//!   old fragments.
//! - **The SETATTR echo is ABSORBED, not committed**: rename/unlink
//!   replies carry no attrs, so under writeback cache the kernel holds
//!   locally-authored dirty ctime and synchronously flushes it — the wire
//!   message is protocol-mandated and cannot be suppressed daemon-side.
//!   `setattr_locked` therefore recognizes the echo shape (times-only,
//!   mtime unchanged vs the folded stored value) and parks the refinement
//!   in a latch-free per-volume pending-times map: **zero journal
//!   entries** at op time (`META_KV_TIMES_ECHO_ABSORBED`), full read-side
//!   visibility (getattr/lookup fold pending over stored, monotone
//!   max-semantics), durability via batched Δtime **drain** transactions
//!   (`META_KV_TIMES_ECHO_DRAIN_COMMITS`, one entry per drain — cadence /
//!   fsync / unmount / cap triggered) under per-ino DLM guards. Real
//!   times news (changed mtime — the buffered-write flush shape; explicit
//!   utimes; any non-times field) still commits.
//! - **unlink**: `routed_unlink_local` already stages the child's Δctime
//!   in the op tx; with the echo absorbed the model closes to
//!   `1 + 1/fill + drain amortization ≤ 1.05`.
//! - **zero-commit teardown** (`routing.rs` `delete_file`): reclaim's
//!   data-path teardown of an inline/empty corpse lands ZERO journal
//!   entries — the comment-only contract at routing.rs:5754-5759 stays a
//!   regression pin.
//! - `drain_reclaim_batch` records the D4.c **fill-vs-window attribution**:
//!   gather fill + how each batch closed (cap / window expiry / channel
//!   close), so a future fill degeneration is attributable from `.stats`
//!   alone.
//!
//! Runs against local file-backed MetaLV sandboxes (no root, no mount).

use squeezefs::fuse_client::METRICS;
use squeezefs::meta_backend::kv::{
    commit_sites_snapshot, META_KV_JOURNAL_ENTRIES, META_KV_TIMES_ECHO_ABSORBED,
    META_KV_TIMES_ECHO_DRAINED, META_KV_TIMES_ECHO_DRAIN_COMMITS,
};
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

fn clock_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

/// The kernel's post-mutation ctime writeback echo, modeled faithfully:
/// after rename/unlink/link/setxattr of a writeback-cache regular file the
/// kernel authors the inode's ctime from ITS clock (`fuse_update_ctime` →
/// `inode_set_ctime_current` — the daemon's replies to those ops carry no
/// attrs, so nothing can pre-empt the dirtying) and synchronously flushes
/// `FUSE_SETATTR(FATTR_MTIME|FATTR_CTIME)` with **mtime = the kernel's
/// cached (daemon-authored, round-tripped) mtime — UNCHANGED — and ctime =
/// kernel-now**. The daemon's setattr handler forwards it as a times-only
/// `Metadata::setattr`. The baseline's per-phase `.stats` snapshots pin the
/// cadence: `meta_updates` (bumped once per mutation HANDLER) moved 200,000
/// for 100 k renames and 200,000 for 100 k unlinks. Returns the echoed
/// ctime.
async fn kernel_ctime_flush_echo(backend: &RoutedMetaBackend, ino: u64) -> u64 {
    let cached = backend
        .getattr(ino)
        .await
        .expect("echo target must be readable (FUSE keeps it alive until FORGET)");
    let kernel_ctime = clock_ns();
    backend
        .setattr(
            ino,
            None,
            None,
            None,
            None,
            None,
            Some(cached.mtime),
            Some(kernel_ctime),
        )
        .await
        .expect("times-only setattr (the kernel ctime-flush echo)");
    kernel_ctime
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

/// PR M6 / G4, rename half (was M2's OQ-1 pin of the 2.002 shape — the
/// mechanism it named is now absorbed). (a) The backend rename path — the
/// FUSE handler's exact sequence: dest-probe lookup + `Metadata::rename` —
/// stays ONE journal entry per op at the rename tx site even though D4.b
/// grew the tx (parent Δtimes + moved-inode Δctime ride the same entry).
/// (b) Adding the kernel's ctime-flush SETATTR echo — the mount shape that
/// measured 2.002 — must now meet **G4: ≤ 1.02 entries/op**, with ZERO
/// commits at the setattr tx site (`META_KV_TIMES_ECHO_ABSORBED` moves
/// instead) and the refined ctime visible through getattr.
#[tokio::test]
async fn mount_shaped_rename_meets_g4_one_entry_per_op() {
    let (_t, backend) = routed_sandbox().await;
    const N: u64 = 1024;

    for i in 0..N {
        backend
            .create(1, &format!("f{i:07}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("seed create");
    }

    // Calibrate the two candidate committers' sites with single-op probes.
    // A COMMITTED setattr shape (explicit mtime change — the buffered-write
    // flush class) calibrates the setattr tx site: post-M6 a pure echo
    // commits nothing, so it can no longer serve as the calibration probe.
    let b = backend.clone();
    let rename_site = calibrate_single_site("rename", || async move {
        let _ = b.lookup(1, "r_cal").await; // dest probe (miss, no commit)
        b.rename(1, "f0000000", 1, "r_cal", 0)
            .await
            .expect("probe rename");
    })
    .await;
    let b = backend.clone();
    let setattr_site = calibrate_single_site("mtime-change setattr", || async move {
        let ino = b.lookup(1, "r_cal").await.expect("probe target").ino;
        let t = clock_ns();
        b.setattr(ino, None, None, None, None, None, Some(t + 1), Some(t))
            .await
            .expect("explicit mtime-change setattr must commit");
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

    // (a) Backend shape (no kernel echo): 1 entry/op — D4.b's one-tx shape
    // (dentry surgery + parent Δtimes + moved-inode Δctime) grows the
    // RECORDS, never the entry count.
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
        (N - 1..=(N - 1) + (N - 1) / 32).contains(&backend_delta),
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

    // (b) Mount shape (rename + the kernel ctime-flush echo): G4 — the
    // echo is ABSORBED (zero setattr-site commits, absorbed counter moves
    // 1:1), so entries/op ≤ 1.02. The background drain task is
    // asynchronous and every sandbox echo advances ctime (the test clock
    // is fine-grained, unlike the real kernel's coarse clock), so a slow
    // box stretches the loop across more cadence ticks than a 100 k-op
    // storm would amortize — subtract the measured drain commits (bounded
    // separately) and let the acceptance table carry the at-scale
    // drains-included number (1.003/op measured).
    let e1 = entries_now();
    let s1 = sites_now();
    let absorbed0 = META_KV_TIMES_ECHO_ABSORBED.load(Ordering::Relaxed);
    let drains0 = META_KV_TIMES_ECHO_DRAIN_COMMITS.load(Ordering::Relaxed);
    for i in 1..N {
        let dst = format!("r{i:07}");
        let src = format!("q{i:07}");
        backend
            .rename(1, &dst, 1, &src, 0)
            .await
            .expect("storm rename back");
        let ino = backend.lookup(1, &src).await.expect("renamed file").ino;
        kernel_ctime_flush_echo(&backend, ino).await;
    }
    let mount_delta = entries_now() - e1;
    let drain_delta = META_KV_TIMES_ECHO_DRAIN_COMMITS.load(Ordering::Relaxed) - drains0;
    let mount_sites = site_deltas(&s1, &sites_now());
    assert!(
        drain_delta <= N / 16,
        "drain commits must stay a small amortized term: {drain_delta} for {N} ops"
    );
    let per_op = (mount_delta - drain_delta) as f64 / (N - 1) as f64;
    assert!(
        per_op <= 1.02,
        "G4: mount-shaped rename (rename + ctime-flush SETATTR echo) must \
         land ≤ 1.02 entries/op — got {per_op:.3} (+ {drain_delta} batched \
         drain commits; sites moved: {mount_sites:?})"
    );
    assert_eq!(
        mount_sites.get(&rename_site).copied().unwrap_or(0),
        N - 1,
        "the rename tx at {rename_site} stays the ONE committer per op"
    );
    assert_eq!(
        mount_sites.get(&setattr_site).copied().unwrap_or(0),
        0,
        "the ctime-flush echo must not commit at the setattr tx site \
         {setattr_site} — it is absorbed into the pending-times map"
    );
    assert_eq!(
        META_KV_TIMES_ECHO_ABSORBED.load(Ordering::Relaxed) - absorbed0,
        N - 1,
        "every echo counts one absorption"
    );
}

/// PR M6 / G4, unlink half (was M2's `1 + 1 + 1/fill` pin — the echo term
/// is now absorbed). The model, decomposed and pinned term by term:
///   - the unlink tx itself: 1 entry/op (child Δctime already in-tx);
///   - the kernel ctime-flush SETATTR echo: **absorbed** — ≤ drain-commit
///     noise instead of the old whole-second-entry, with the absorbed
///     counter moving 1:1 and zero commits at the setattr tx site;
///   - the FORGET-side `destroy_inodes` batches: 1 entry per BATCH — with
///     the gather healthy (fill = cap = 64, the M2-measured shape), that
///     is 1/64 ≈ 0.016/op.
/// Total: **G4 ≤ 1.05 entries/op** (was 2.020).
#[tokio::test]
async fn unlink_entry_economy_meets_g4_via_echo_absorption() {
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

    // Calibrate the setattr committer site with a COMMITTED shape (explicit
    // mtime change — post-M6 a pure echo commits nothing).
    let b = backend.clone();
    let probe_ino = inos[0];
    let setattr_site = calibrate_single_site("mtime-change setattr", || async move {
        let t = clock_ns();
        b.setattr(
            probe_ino,
            None,
            None,
            None,
            None,
            None,
            Some(t + 1),
            Some(t),
        )
        .await
        .expect("explicit mtime-change setattr must commit");
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
    assert!(
        unlink_sites.len() <= 2,
        "the unlink storm commits through the unlink site (+ at most the \
         pending-times drain): {unlink_sites:?}"
    );

    // Term 2: the kernel ctime-flush echo — ABSORBED (the unlinked-but-not-
    // yet-destroyed inode record is still writable: FUSE keeps it alive
    // until FORGET, and the kernel may getattr it — so the refinement must
    // be READABLE, just not per-op-journaled).
    let e1 = entries_now();
    let s1 = sites_now();
    let absorbed0 = META_KV_TIMES_ECHO_ABSORBED.load(Ordering::Relaxed);
    let mut echoed = Vec::with_capacity(inos.len());
    for &ino in &inos {
        echoed.push((ino, kernel_ctime_flush_echo(&backend, ino).await));
    }
    let echo_delta = entries_now() - e1;
    let echo_sites = site_deltas(&s1, &sites_now());
    assert!(
        echo_delta <= N / 32,
        "the ctime-flush echo must be absorbed, not committed: {echo_delta} \
         entries for {N} echoes (sites: {echo_sites:?})"
    );
    assert_eq!(
        echo_sites.get(&setattr_site).copied().unwrap_or(0),
        0,
        "zero echo commits at the setattr tx site {setattr_site}"
    );
    assert_eq!(
        META_KV_TIMES_ECHO_ABSORBED.load(Ordering::Relaxed) - absorbed0,
        N,
        "every echo counts one absorption"
    );
    // The refinement is READ-VISIBLE while pending (kernel getattr of an
    // unlinked-open inode must see the refreshed ctime).
    for &(ino, kctime) in &echoed {
        let got = backend.getattr(ino).await.expect("unlinked-open getattr");
        assert!(
            got.ctime >= kctime,
            "absorbed echo invisible to getattr: ino {ino} ctime {} < echoed {kctime}",
            got.ctime
        );
    }

    // Term 3: FORGET-side destroys at healthy fill — 1 entry per BATCH,
    // recorded in the destroy-fill histogram's ≤64 bucket (index 7 of the
    // QueueDepthHistogram labels: 0,1,2,≤4,≤8,≤16,≤32,≤64,…). The
    // background drain task is asynchronous — a cadence tick landing
    // inside this window commits its own (counted) entries, so the
    // mechanism assertion is exact MODULO the measured drain commits.
    let fills_le64_before = METRICS.meta_reclaim_batch_size.buckets[7].load(Ordering::Relaxed);
    let drains2 = META_KV_TIMES_ECHO_DRAIN_COMMITS.load(Ordering::Relaxed);
    let e2 = entries_now();
    for chunk in inos.chunks(FILL) {
        backend
            .destroy_inodes(chunk)
            .await
            .expect("batched FORGET-side destroy");
    }
    let drain_d3 = META_KV_TIMES_ECHO_DRAIN_COMMITS.load(Ordering::Relaxed) - drains2;
    let destroy_delta = entries_now() - e2;
    let n_batches = inos.len().div_ceil(FILL) as u64;
    assert_eq!(
        destroy_delta - drain_d3,
        n_batches,
        "destroys amortize to ONE entry per batch ({n_batches} batches for \
         {N} corpses; {drain_d3} concurrent drain commits excluded)"
    );
    let fills_le64_after = METRICS.meta_reclaim_batch_size.buckets[7].load(Ordering::Relaxed);
    assert_eq!(
        fills_le64_after - fills_le64_before,
        n_batches,
        "every destroy batch records its fill in the ≤64 bucket (fill = cap, not 1)"
    );

    // The model, assembled: 1 (unlink tx) + echo term (absorbed ⇒ ≤ the
    // N/32 drain-noise bound asserted above) + exactly 1/fill destroys.
    // At-scale amortization (drains INCLUDED) is the acceptance table's
    // job — 100 k-op storms measure 1.018/op; this sandbox pin proves the
    // per-term mechanism at N = 320.
    let total = unlink_delta + echo_delta + (destroy_delta - drain_d3);
    let per_op = total as f64 / N as f64;
    assert!(
        per_op <= 1.05,
        "G4: unlink economy must close to 1 + 1/fill + drain ≤ 1.05 \
         entries/op (was the measured 2.020) — got {per_op:.3}"
    );
}

/// PR M6 absorber semantics, the POSIX-visibility half: an absorbed echo
/// must be indistinguishable from a committed one to every reader — the
/// mission's non-negotiable "stat after unlink-of-hardlink must show
/// updated ctime" case — and a drain must make it durable across a real
/// remount with ONE batched entry, leaving the read view byte-identical.
#[tokio::test]
async fn times_echo_absorption_is_read_visible_and_drains_durable() {
    let tmp = NamedTempFile::new().unwrap();
    let kv = open_v3_meta(tmp.path(), 256 * 1024 * 1024).await;
    let backend = Arc::new(RoutedMetaBackend::new(vec![kv]));

    // unlink-of-hardlink: victim name dies, the inode survives via the
    // second link — the echo lands on the surviving inode.
    let f = backend
        .create(1, "hl_a", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create");
    backend.link(f.ino, 1, "hl_b").await.expect("hardlink");
    let pre = backend.getattr(f.ino).await.expect("pre-unlink getattr");
    backend.unlink(1, "hl_a").await.expect("unlink one link");

    let in_tx = backend.getattr(f.ino).await.expect("post-unlink getattr");
    assert!(
        in_tx.ctime > pre.ctime,
        "the unlink tx itself must advance the surviving inode's ctime \
         (in-tx Δctime): {} !> {}",
        in_tx.ctime,
        pre.ctime
    );

    let kctime = kernel_ctime_flush_echo(&backend, f.ino).await;
    let folded = backend.getattr(f.ino).await.expect("folded getattr");
    assert!(
        folded.ctime >= kctime,
        "stat after unlink-of-hardlink must show the echoed ctime: {} < {kctime}",
        folded.ctime
    );
    let via_lookup = backend.lookup(1, "hl_b").await.expect("lookup survivor");
    assert_eq!(
        via_lookup.ctime, folded.ctime,
        "lookup and getattr must serve the same folded ctime"
    );

    // Drain: ONE batched entry, refinements durable, view unchanged.
    let drained0 = META_KV_TIMES_ECHO_DRAINED.load(Ordering::Relaxed);
    let commits0 = META_KV_TIMES_ECHO_DRAIN_COMMITS.load(Ordering::Relaxed);
    let e0 = entries_now();
    backend.volumes[0]
        .drain_pending_times_now()
        .await
        .expect("explicit drain");
    assert_eq!(
        backend.volumes[0].pending_times_len(),
        0,
        "drain must leave no pending refinements"
    );
    assert!(
        entries_now() - e0 <= 1,
        "one drain = at most one journal entry"
    );
    assert!(
        META_KV_TIMES_ECHO_DRAINED.load(Ordering::Relaxed) > drained0
            || META_KV_TIMES_ECHO_DRAIN_COMMITS.load(Ordering::Relaxed) > commits0
            || folded.ctime == in_tx.ctime,
        "a pending refinement existed, so the drain counters must move \
         (unless the background drain already took it)"
    );
    let post_drain = backend.getattr(f.ino).await.expect("post-drain getattr");
    assert_eq!(
        post_drain.ctime, folded.ctime,
        "draining must not change the served ctime"
    );

    // Durability: clean shutdown + remount serves the refined ctime from
    // the trees alone (no pending map on a fresh mount).
    backend.volumes[0].shutdown().await.expect("clean shutdown");
    let reopened = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(tmp.path())
        .await
        .expect("remount");
    assert_eq!(reopened.pending_times_len(), 0, "fresh mount, empty map");
    let durable = reopened.getattr(f.ino).await.expect("remounted getattr");
    assert_eq!(
        durable.ctime, folded.ctime,
        "the drained refinement must survive remount byte-exact"
    );
    reopened.shutdown().await.expect("second shutdown");
}

/// PR M6 absorber semantics, the monotonicity half: the kernel's coarse
/// clock (`ktime_get_coarse_real_ts64`, jiffy resolution) can stamp an
/// echo ctime BEHIND the daemon's fine-grained in-tx ctime. Absorption
/// must never move the served ctime backwards — the old committed path
/// happily regressed it — and a stale refinement must not survive a
/// fresher committed write (the drain skips it).
#[tokio::test]
async fn times_echo_never_regresses_ctime() {
    let (_t, backend) = routed_sandbox().await;
    let f = backend
        .create(1, "mono", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create");
    let stored = backend.getattr(f.ino).await.expect("getattr");

    // A coarse-clock echo: times-only, mtime unchanged, ctime BEHIND the
    // stored value (the jiffy-lag shape).
    let e0 = entries_now();
    let absorbed0 = META_KV_TIMES_ECHO_ABSORBED.load(Ordering::Relaxed);
    backend
        .setattr(
            f.ino,
            None,
            None,
            None,
            None,
            None,
            Some(stored.mtime),
            Some(stored.ctime.saturating_sub(3_000_000)), // 3 ms behind
        )
        .await
        .expect("regressive echo");
    assert_eq!(
        entries_now() - e0,
        0,
        "a regressive echo must not commit a journal entry"
    );
    assert_eq!(
        META_KV_TIMES_ECHO_ABSORBED.load(Ordering::Relaxed) - absorbed0,
        1,
        "a regressive echo still counts one absorption"
    );
    let after = backend.getattr(f.ino).await.expect("getattr");
    assert_eq!(
        after.ctime, stored.ctime,
        "ctime must never move backwards through the absorber"
    );

    // A stale pending refinement never clobbers a fresher committed write:
    // absorb a forward echo, then chmod (commits ctime = now > echo) —
    // the drain must not regress it.
    let kctime = kernel_ctime_flush_echo(&backend, f.ino).await;
    backend
        .setattr(f.ino, Some(0o600), None, None, None, None, None, None)
        .await
        .expect("chmod");
    let post_chmod = backend.getattr(f.ino).await.expect("getattr");
    assert!(
        post_chmod.ctime >= kctime,
        "chmod's committed ctime must supersede the pending echo"
    );
    backend.volumes[0]
        .drain_pending_times_now()
        .await
        .expect("drain");
    let post_drain = backend.getattr(f.ino).await.expect("getattr");
    assert_eq!(
        post_drain.ctime, post_chmod.ctime,
        "a stale refinement must not fold over a fresher committed ctime"
    );
    assert!(
        post_drain.mode & 0o777 == 0o600,
        "the committed chmod stands"
    );
}

/// PR M6 absorber eligibility: only the echo shape (times-only, mtime
/// unchanged) absorbs. Real times news — a CHANGED mtime (the
/// buffered-write `fuse_flush_times` shape carrying kernel-authored write
/// mtime), an explicit utimensat (atime present), a truncate — must keep
/// committing: absorbing those would lose data-visible metadata.
#[tokio::test]
async fn real_times_news_still_commits() {
    let (_t, backend) = routed_sandbox().await;
    let f = backend
        .create(1, "news", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create");

    // (a) mtime CHANGED + ctime: the buffered-write flush — commits.
    let t = clock_ns();
    let e0 = entries_now();
    backend
        .setattr(f.ino, None, None, None, None, None, Some(t), Some(t))
        .await
        .expect("write-mtime flush");
    assert_eq!(
        entries_now() - e0,
        1,
        "a changed-mtime times flush must commit exactly one entry"
    );
    let got = backend.getattr(f.ino).await.expect("getattr");
    assert_eq!(got.mtime, t, "the flushed mtime must persist exactly");
    assert_eq!(got.ctime, t, "the flushed ctime must persist exactly");

    // (b) explicit utimensat (atime + mtime + ctime): exact-set semantics,
    // commits — even setting times BACKWARDS, and even with a live pending
    // refinement parked on the inode (the commit must retire it: a dead
    // refinement folding over an intentional backwards utimes would
    // resurrect the newer ctime).
    kernel_ctime_flush_echo(&backend, f.ino).await;
    let past = t - 86_400_000_000_000; // a day earlier
    let e1 = entries_now();
    backend
        .setattr(
            f.ino,
            None,
            None,
            None,
            None,
            Some(past),
            Some(past),
            Some(past + 1),
        )
        .await
        .expect("explicit utimes");
    let utimes_delta = entries_now() - e1;
    assert!(
        (1..=2).contains(&utimes_delta),
        "explicit utimes must commit exactly one entry (+ at most one \
         background drain of the planted echo): got {utimes_delta}"
    );
    let got = backend.getattr(f.ino).await.expect("getattr");
    assert_eq!(got.atime, past, "explicit atime is exact");
    assert_eq!(got.mtime, past, "explicit backwards mtime is exact");
    assert_eq!(
        got.ctime,
        past + 1,
        "explicit ctime is exact — never max-clamped by a dead refinement"
    );

    // (c) truncate (size) with times: commits.
    let e2 = entries_now();
    backend
        .setattr(f.ino, None, None, None, Some(0), None, None, None)
        .await
        .expect("truncate");
    assert_eq!(
        entries_now() - e2,
        1,
        "truncate must commit exactly one entry"
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
/// (`get_fuse_timeout`) — zero per-op env reads, and default OFF.
///
/// PR M4 (D1.b) evolves the disabled-cost contract: `OpProf::begin`
/// ALWAYS claims a watchdog registry slot (the scan surface that
/// replaced the per-op `timeout()` wrappers — a CAS + relaxed stores,
/// no clock reads), but with the rig disabled it takes NO profile
/// stamps: the phase histograms must not move and the marks are no-ops.
#[test]
fn op_profile_gate_is_memoized_and_default_off() {
    assert!(
        !op_profile_enabled(),
        "SQUEEZEFS_OP_PROFILE unset ⇒ the rig is OFF by default"
    );
    let phases_before = op_profile_phase_json();
    let inflight0 = op_profile_inflight();
    let p = OpProf::begin(FuseOpKind::Create, 1);
    assert_eq!(
        op_profile_inflight(),
        inflight0 + 1,
        "begin always registers with the watchdog (D1.b), rig on or off"
    );
    p.mark_backend_start();
    p.mark_backend_done();
    drop(p);
    assert_eq!(
        op_profile_inflight(),
        inflight0,
        "op exit clears the watchdog slot"
    );
    assert_eq!(
        phase_total(&op_profile_phase_json(), "create", "total"),
        phase_total(&phases_before, "create", "total"),
        "disabled rig records NO phase samples (no stamps, no clock reads)"
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
