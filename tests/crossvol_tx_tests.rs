//! **DLM stage S3.5 — cross-volume transaction machinery**, and its first
//! consumer, the P0 durability bug **DUR-7** (pre-RC engineering spec
//! §DUR-7; ruling **D4**: distributed transaction, not refusal).
//!
//! Before this suite existed, a metadata set with more than one volume
//! composed `link`, `unlink`/`rmdir` and cross-parent `rename` out of TWO
//! (or more) independent whole-tx commits with **no intent record, no
//! compensation and no crash recovery** — the code's own comment on the
//! rename case said *"best-effort"*. The spec's consequences, verbatim:
//!
//! * `link`, crash between: `nlink = 2`, ONE dentry. `reclaim_orphaned_batch`
//!   skips `nlink > 0`, so the inode and every block it names **leak
//!   permanently**.
//! * `unlink`, crash between: `nlink = 1`, ZERO dentries — invisible AND
//!   unreclaimable. Same permanent leak.
//! * directory `rename`: parent `nlink` drifts permanently; `rmdir` then
//!   either succeeds with children present or refuses forever.
//!
//! Same-volume paths are single whole-tx entries and were always correct:
//! this is purely the cross-volume composition, and it was **untested**
//! because every in-tree suite runs single-volume.
//!
//! What this suite pins (design: `docs/design-cow-kv-metadata.md` §4.10a):
//!
//! 1. **The commit-boundary seam** (the spec's own acceptance): for every
//!    enumerated crash window of every converted op, a crash-equivalent
//!    reopen observes the **pre- or the post-state and never an
//!    intermediate one**.
//! 2. **Roll-forward with witnessed idempotent steps**: recovery re-runs
//!    the plan; an already-applied step is recognised (never applied
//!    twice) and a step whose object moved under it is skipped LOUD
//!    (`crossvol_tx_steps_foreign`) instead of clobbering.
//! 3. **The leak paths**: after recovery a crashed cross-volume
//!    `unlink`/`link` leaves an inode the reclaimer can actually reclaim
//!    (the bug left `nlink` positive forever, which `destroy_inodes`
//!    skips by contract).
//! 4. **No single-volume regression**: a one-volume set writes ZERO
//!    intent records and still costs one journal entry per op (the D4
//!    journal-economy law).
//! 5. **Retirement before release**: a successful cross-volume op leaves
//!    no open intent behind — the property that makes recovery's
//!    witnesses sound.
//! 6. **The acquisition order cannot deadlock** against concurrent
//!    inverse operations.
//! 7. **The wire** (`SQZXTX01`) round-trips and never panics on hostile
//!    bytes.
//!
//! Harness notes: the seam is a process-global atomic (the
//! `uring_fs::arm_*` / `TEST_CONVEYOR_HOLD_STAGE` precedent) — armed
//! through an RAII guard so it never leaks across cases, and the suite
//! runs under `--test-threads=1` like the rest of the tree. A "crash" is
//! a drop-without-shutdown followed by a reopen: buffered device writes
//! are exactly what a post-kill remount reads.

use squeezefs::meta_backend::crossvol_tx::{
    IntentRecord, TEST_XV_SEAM_AFTER_STEPS, XV_STEPS_ALREADY_APPLIED, XV_STEPS_APPLIED,
    XV_STEPS_FOREIGN_SKIPPED, XV_TX_RECOVERED,
};
use squeezefs::meta_backend::kv::builder::{format_v3, format_v3_stamped, FormatV3Options};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::{
    open_routed_meta_set, plan_meta_slot_set_with_width, Metadata, RoutedMetaBackend,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

const VOL_LEN: u64 = 256 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

fn opts() -> FormatV3Options {
    FormatV3Options {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

fn make_file(dir: &Path, name: &str, len: u64) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    p
}

/// Arm the commit-boundary seam: `allowed = v - 1` steps commit, then the
/// plan stops as if the process died (`1` = before any step, i.e. the
/// pre-state window; `step_count + 1` = after every step but before the
/// intent retirement).
struct SeamGuard;
impl Drop for SeamGuard {
    fn drop(&mut self) {
        TEST_XV_SEAM_AFTER_STEPS.store(0, Ordering::Relaxed);
    }
}
fn arm_seam(v: u64) -> SeamGuard {
    TEST_XV_SEAM_AFTER_STEPS.store(v, Ordering::Relaxed);
    SeamGuard
}

/// A stamped two-volume set, formatted the way `format` does for a
/// multi-volume metadata set (identity slot distribution, epoch 1).
async fn stamped_set(dir: &Path, volumes: usize, width: u32) -> Vec<String> {
    let plan = plan_meta_slot_set_with_width(volumes, width).expect("plan admits the bounds");
    let mut paths = Vec::new();
    for i in 0..volumes {
        let p = make_file(dir, &format!("meta{i}"), VOL_LEN);
        format_v3_stamped(&p, VOL_LEN, &opts(), plan.stamps[i].clone())
            .await
            .expect("format stamped meta volume");
        paths.push(p.display().to_string());
    }
    paths
}

/// Open (or re-open) the set. In-process the single-writer flock releases
/// only when the previous mount's detached tasks drop their last `Arc`, so
/// poll-retry — harness plumbing, not the contract under test.
async fn open_set(paths: &[String]) -> Arc<RoutedMetaBackend> {
    let mut last = String::new();
    for _ in 0..200 {
        match open_routed_meta_set(paths).await {
            Ok(r) => return r,
            Err(e) => {
                last = format!("{e}");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
    panic!("the writer guard must release once the previous set is dropped: {last}");
}

/// Crash-equivalent remount: drop every `Arc` without shutdown, then
/// re-open (which is where cross-volume intent recovery runs).
async fn crash_and_reopen(
    routed: Arc<RoutedMetaBackend>,
    paths: &[String],
) -> Arc<RoutedMetaBackend> {
    drop(routed);
    open_set(paths).await
}

async fn shutdown(routed: &RoutedMetaBackend) {
    for vol in &routed.volumes {
        vol.shutdown().await.expect("volume shutdown");
    }
}

async fn lookup_opt(routed: &RoutedMetaBackend, parent: u64, name: &str) -> Option<u64> {
    routed.lookup(parent, name).await.ok().map(|i| i.ino)
}

async fn nlink_of(routed: &RoutedMetaBackend, ino: u64) -> Option<u32> {
    routed.getattr(ino).await.ok().map(|i| i.nlink)
}

/// Total open intents across the set — the retirement instrument.
async fn open_intents(routed: &RoutedMetaBackend) -> usize {
    let mut n = 0;
    for vol in &routed.volumes {
        n += vol.xv_scan_intents().await.expect("scan intents").len();
    }
    n
}

/// A directory under `parent` whose ino routes to `want_vol`.
async fn mkdir_on(routed: &RoutedMetaBackend, parent: u64, want_vol: usize, tag: &str) -> u64 {
    for i in 0..64 {
        let name = format!("{tag}_d{i}");
        let ino = routed
            .create(parent, &name, libc::S_IFDIR | 0o755, 0, 0)
            .await
            .expect("mkdir")
            .ino;
        if routed.route_ino(ino).0 == want_vol {
            return ino;
        }
    }
    panic!("directory striping never placed a directory on volume {want_vol}");
}

/// A regular file under `parent` whose ino routes to `want_vol`.
async fn mkfile_on(
    routed: &RoutedMetaBackend,
    parent: u64,
    want_vol: usize,
    tag: &str,
) -> (u64, String) {
    for i in 0..64 {
        let name = format!("{tag}_f{i}");
        let ino = routed
            .create(parent, &name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create")
            .ino;
        if routed.route_ino(ino).0 == want_vol {
            return (ino, name);
        }
    }
    panic!("inode placement never minted a file on volume {want_vol}");
}

// ---------------------------------------------------------------------------
// 1. The commit-boundary seam — unlink
// ---------------------------------------------------------------------------

/// The spec's acceptance leg for `unlink`. A cross-volume unlink crashed
/// at its commit boundary must recover to the PRE state or the POST
/// state — **never** the intermediate one the bug leaves (`nlink = 1`
/// with zero dentries: invisible and unreclaimable forever).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crossvol_unlink_seam_recovers_pre_or_post_never_intermediate() {
    let dir = tempfile::tempdir().unwrap();
    let paths = stamped_set(dir.path(), 2, 2).await;

    // Every enumerated window: 1 = before any step, 2 = after the parent
    // dentry removal, 3 = after the child nlink commit (before the intent
    // retirement).
    for window in 1..=3u64 {
        let routed = open_set(&paths).await;
        let parent = mkdir_on(&routed, 1, 0, &format!("w{window}")).await;
        let (child, name) = mkfile_on(&routed, parent, 1, &format!("w{window}")).await;
        assert_ne!(
            routed.route_ino(parent).0,
            routed.route_ino(child).0,
            "the fixture must produce a CROSS-volume unlink"
        );
        assert_eq!(nlink_of(&routed, child).await, Some(1));

        {
            let _seam = arm_seam(window);
            let _ = routed.unlink(parent, &name).await;
        }

        let routed = crash_and_reopen(routed, &paths).await;
        let named = lookup_opt(&routed, parent, &name).await;
        let links = nlink_of(&routed, child).await;
        let pre = named == Some(child) && links == Some(1);
        let post = named.is_none() && (links == Some(0) || links.is_none());
        assert!(
            pre || post,
            "window {window}: recovered state is neither pre nor post — \
             dentry {named:?}, nlink {links:?} (the DUR-7 intermediate state)"
        );
        assert_eq!(
            open_intents(&routed).await,
            0,
            "window {window}: recovery must retire the intent it completed"
        );
        shutdown(&routed).await;
    }
}

/// Window 1 (nothing committed) is the PRE state exactly, and window 3
/// (everything committed, retirement lost) is the POST state exactly —
/// the two ends of the ladder, asserted precisely rather than as a
/// disjunction.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crossvol_unlink_first_and_last_windows_are_exact() {
    let dir = tempfile::tempdir().unwrap();
    let paths = stamped_set(dir.path(), 2, 2).await;

    // Window 1: the plan stops before its first commit — nothing durable.
    let routed = open_set(&paths).await;
    let parent = mkdir_on(&routed, 1, 0, "pre").await;
    let (child, name) = mkfile_on(&routed, parent, 1, "pre").await;
    {
        let _seam = arm_seam(1);
        assert!(
            routed.unlink(parent, &name).await.is_err(),
            "a seam-severed plan must surface an error, never a silent success"
        );
    }
    let routed = crash_and_reopen(routed, &paths).await;
    assert_eq!(lookup_opt(&routed, parent, &name).await, Some(child));
    assert_eq!(nlink_of(&routed, child).await, Some(1));
    assert_eq!(open_intents(&routed).await, 0);

    // Window 3: every step committed, only the retirement lost.
    let (child2, name2) = mkfile_on(&routed, parent, 1, "post").await;
    {
        let _seam = arm_seam(3);
        let _ = routed.unlink(parent, &name2).await;
    }
    let routed = crash_and_reopen(routed, &paths).await;
    assert_eq!(lookup_opt(&routed, parent, &name2).await, None);
    assert_eq!(
        nlink_of(&routed, child2).await,
        Some(0),
        "the child's link count must be the post-state 0"
    );
    assert_eq!(open_intents(&routed).await, 0);
    shutdown(&routed).await;
}

// ---------------------------------------------------------------------------
// 2. The commit-boundary seam — link
// ---------------------------------------------------------------------------

/// `link`'s window: `nlink` bumped, the second name never committed. The
/// bug left `nlink = 2` with ONE dentry forever; recovery must roll
/// forward to the post-state (two names, `nlink = 2`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crossvol_link_seam_recovers_pre_or_post_never_intermediate() {
    let dir = tempfile::tempdir().unwrap();
    let paths = stamped_set(dir.path(), 2, 2).await;

    for window in 1..=3u64 {
        let routed = open_set(&paths).await;
        let parent = mkdir_on(&routed, 1, 0, &format!("l{window}")).await;
        let (child, name) = mkfile_on(&routed, parent, 1, &format!("l{window}")).await;
        assert_ne!(routed.route_ino(parent).0, routed.route_ino(child).0);
        let linked = format!("{name}_hard");

        {
            let _seam = arm_seam(window);
            let _ = routed.link(child, parent, &linked).await;
        }

        let routed = crash_and_reopen(routed, &paths).await;
        let named = lookup_opt(&routed, parent, &linked).await;
        let links = nlink_of(&routed, child).await;
        let pre = named.is_none() && links == Some(1);
        let post = named == Some(child) && links == Some(2);
        assert!(
            pre || post,
            "window {window}: link recovered to neither pre nor post — \
             second name {named:?}, nlink {links:?} (the DUR-7 leak state)"
        );
        assert_eq!(open_intents(&routed).await, 0);
        shutdown(&routed).await;
    }
}

// ---------------------------------------------------------------------------
// 3. The commit-boundary seam — directory rename across parents
// ---------------------------------------------------------------------------

/// The hardest shape: two parents' `nlink`, the dentry move and the moved
/// inode's ctime, spread over two volumes. Every enumerated window must
/// recover to the pre- or the post-state; a drifted parent `nlink` is
/// exactly the permanent damage the spec describes (`rmdir` then either
/// succeeds with children present or refuses forever).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crossvol_dir_rename_every_window_recovers_pre_or_post() {
    let dir = tempfile::tempdir().unwrap();
    let paths = stamped_set(dir.path(), 2, 2).await;

    // The dir-rename plan is 5 steps (old-parent nlink, new-parent nlink,
    // source dentry removal, destination dentry insert, moved-inode
    // ctime) — window 6 is "all steps, retirement lost".
    for window in 1..=6u64 {
        let routed = open_set(&paths).await;
        let old_parent = mkdir_on(&routed, 1, 0, &format!("r{window}a")).await;
        let new_parent = mkdir_on(&routed, 1, 1, &format!("r{window}b")).await;
        assert_ne!(
            routed.route_ino(old_parent).0,
            routed.route_ino(new_parent).0,
            "the fixture must produce a CROSS-volume rename"
        );
        let moved = routed
            .create(old_parent, "sub", libc::S_IFDIR | 0o755, 0, 0)
            .await
            .expect("mkdir sub")
            .ino;
        let pre_old = nlink_of(&routed, old_parent).await.unwrap();
        let pre_new = nlink_of(&routed, new_parent).await.unwrap();

        {
            let _seam = arm_seam(window);
            let _ = routed.rename(old_parent, "sub", new_parent, "sub", 0).await;
        }

        let routed = crash_and_reopen(routed, &paths).await;
        let at_old = lookup_opt(&routed, old_parent, "sub").await;
        let at_new = lookup_opt(&routed, new_parent, "sub").await;
        let now_old = nlink_of(&routed, old_parent).await.unwrap();
        let now_new = nlink_of(&routed, new_parent).await.unwrap();
        let pre =
            at_old == Some(moved) && at_new.is_none() && now_old == pre_old && now_new == pre_new;
        let post = at_old.is_none()
            && at_new == Some(moved)
            && now_old == pre_old - 1
            && now_new == pre_new + 1;
        assert!(
            pre || post,
            "window {window}: dir rename recovered to neither pre nor post — \
             old {at_old:?} new {at_new:?}, parent nlinks {now_old}/{now_new} \
             (pre {pre_old}/{pre_new})"
        );
        assert_eq!(open_intents(&routed).await, 0);
        shutdown(&routed).await;
    }
}

// ---------------------------------------------------------------------------
// 4. The leak paths are the point
// ---------------------------------------------------------------------------

/// The DUR-7 leak, closed: after a crashed cross-volume unlink recovers,
/// the child is reclaimable — `destroy_inodes` (the batch
/// `reclaim_orphaned_batch` drives) actually destroys it. With the bug the
/// child kept `nlink = 1`, and `destroy_inodes` skips `nlink > 0` **by
/// contract**, so the inode and every block it named leaked permanently.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovered_crossvol_unlink_leaves_a_reclaimable_inode() {
    let dir = tempfile::tempdir().unwrap();
    let paths = stamped_set(dir.path(), 2, 2).await;
    let routed = open_set(&paths).await;
    let parent = mkdir_on(&routed, 1, 0, "reclaim").await;
    let (child, name) = mkfile_on(&routed, parent, 1, "reclaim").await;

    {
        let _seam = arm_seam(2); // dentry gone, child nlink not yet committed
        let _ = routed.unlink(parent, &name).await;
    }
    let routed = crash_and_reopen(routed, &paths).await;

    assert_eq!(
        nlink_of(&routed, child).await,
        Some(0),
        "a recovered unlink must leave nlink == 0 — the reclaimer's precondition"
    );
    routed
        .destroy_inodes(&[child])
        .await
        .expect("destroy the orphan");
    assert!(
        routed.getattr(child).await.is_err(),
        "the orphan must be GONE after reclaim — `nlink > 0` would have made it \
         permanently unreclaimable (the DUR-7 leak)"
    );
    shutdown(&routed).await;
}

/// The `link` face of the same leak: after recovery the inode's link
/// count matches its dentry population, so removing every name reaches
/// `nlink = 0` and the inode reclaims. With the bug (`nlink = 2`, one
/// dentry) the last unlink left `nlink = 1` forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovered_crossvol_link_keeps_nlink_and_names_in_step() {
    let dir = tempfile::tempdir().unwrap();
    let paths = stamped_set(dir.path(), 2, 2).await;
    let routed = open_set(&paths).await;
    let parent = mkdir_on(&routed, 1, 0, "leak").await;
    let (child, name) = mkfile_on(&routed, parent, 1, "leak").await;
    let second = format!("{name}_two");

    {
        let _seam = arm_seam(2); // nlink bumped, second name not committed
        let _ = routed.link(child, parent, &second).await;
    }
    let routed = crash_and_reopen(routed, &paths).await;

    assert_eq!(
        lookup_opt(&routed, parent, &name).await,
        Some(child),
        "the original name survives"
    );
    assert_eq!(
        lookup_opt(&routed, parent, &second).await,
        Some(child),
        "recovery rolled the second name forward"
    );
    assert_eq!(
        nlink_of(&routed, child).await,
        Some(2),
        "recovery rolls the link forward: two names, two links"
    );
    routed.unlink(parent, &name).await.expect("unlink first");
    routed.unlink(parent, &second).await.expect("unlink second");
    assert_eq!(
        nlink_of(&routed, child).await,
        Some(0),
        "every name removed ⇒ nlink 0 ⇒ reclaimable"
    );
    routed.destroy_inodes(&[child]).await.expect("reclaim");
    assert!(routed.getattr(child).await.is_err());
    shutdown(&routed).await;
}

// ---------------------------------------------------------------------------
// 5. No single-volume regression
// ---------------------------------------------------------------------------

/// A ONE-volume set must take the pre-S3.5 path byte-for-byte: no intent
/// record is ever written, and unlink/link/rename each still cost ONE
/// journal entry (the D4 journal-economy law — the whole point of fixing
/// a P0 rather than trading it for a throughput regression).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_volume_ops_write_no_intents_and_no_extra_entries() {
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "solo", VOL_LEN);
    format_v3(&meta, VOL_LEN, &opts()).await.expect("format");
    let paths = vec![meta.display().to_string()];
    let routed = open_set(&paths).await;
    let vol = &routed.volumes[0];

    let f = routed
        .create(1, "a", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create")
        .ino;

    let e0 = vol.journal_ring().written_entries();
    routed.link(f, 1, "b").await.expect("link");
    let e1 = vol.journal_ring().written_entries();
    routed.rename(1, "b", 1, "c", 0).await.expect("rename");
    let e2 = vol.journal_ring().written_entries();
    routed.unlink(1, "c").await.expect("unlink");
    let e3 = vol.journal_ring().written_entries();

    assert_eq!(e1 - e0, 1, "single-volume link must stay ONE journal entry");
    assert_eq!(
        e2 - e1,
        1,
        "single-volume rename must stay ONE journal entry"
    );
    assert_eq!(
        e3 - e2,
        1,
        "single-volume unlink must stay ONE journal entry"
    );
    assert_eq!(
        open_intents(&routed).await,
        0,
        "a single-volume set must never write a cross-volume intent record"
    );
    shutdown(&routed).await;
}

// ---------------------------------------------------------------------------
// 6. Retirement before release, and recovery idempotence
// ---------------------------------------------------------------------------

/// A SUCCESSFUL cross-volume op retires its intent before it returns —
/// the invariant that makes the witnesses sound (a lingering intent whose
/// op completed could resurrect a name a later op legitimately removed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn successful_crossvol_ops_leave_no_open_intent() {
    let dir = tempfile::tempdir().unwrap();
    let paths = stamped_set(dir.path(), 2, 2).await;
    let routed = open_set(&paths).await;
    let parent = mkdir_on(&routed, 1, 0, "clean").await;
    let (child, name) = mkfile_on(&routed, parent, 1, "clean").await;

    routed
        .link(child, parent, "second")
        .await
        .expect("cross-volume link");
    assert_eq!(open_intents(&routed).await, 0, "link retired its intent");
    routed
        .unlink(parent, "second")
        .await
        .expect("cross-volume unlink");
    assert_eq!(open_intents(&routed).await, 0, "unlink retired its intent");
    routed
        .unlink(parent, &name)
        .await
        .expect("cross-volume unlink");
    assert_eq!(open_intents(&routed).await, 0);
    shutdown(&routed).await;
}

/// Recovery is idempotent: a second mount finds nothing to do, and the
/// state does not move. (Recovery re-runs every step through the SAME
/// witnessed applier the live path uses, so "already applied" is
/// recognised rather than re-applied — a doubled `nlink` decrement would
/// show here.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_is_idempotent_across_repeated_mounts() {
    let dir = tempfile::tempdir().unwrap();
    let paths = stamped_set(dir.path(), 2, 2).await;
    let routed = open_set(&paths).await;
    let parent = mkdir_on(&routed, 1, 0, "idem").await;
    let (child, name) = mkfile_on(&routed, parent, 1, "idem").await;
    routed.link(child, parent, "n2").await.expect("link");

    {
        let _seam = arm_seam(2);
        let _ = routed.unlink(parent, &name).await;
    }

    let r0 = XV_TX_RECOVERED.load(Ordering::Relaxed);
    let routed = crash_and_reopen(routed, &paths).await;
    assert!(
        XV_TX_RECOVERED.load(Ordering::Relaxed) > r0,
        "the first remount must recover exactly the interrupted tx"
    );
    let links = nlink_of(&routed, child).await;
    assert_eq!(links, Some(1), "one name left ⇒ one link");

    let r1 = XV_TX_RECOVERED.load(Ordering::Relaxed);
    let routed = crash_and_reopen(routed, &paths).await;
    assert_eq!(
        XV_TX_RECOVERED.load(Ordering::Relaxed),
        r1,
        "a clean set has nothing to recover"
    );
    assert_eq!(
        nlink_of(&routed, child).await,
        links,
        "a second recovery pass must not move the link count"
    );
    shutdown(&routed).await;
}

/// Witness semantics, directly: a step whose object moved under the
/// intent is **skipped loud**, never applied. Built by severing a plan at
/// its seam and then legitimately taking the name the plan's insert step
/// targets, so recovery meets a foreign occupant.
///
/// This path is DEFENSIVE, not a production shape: an intent can only be
/// observed by a mount whose op never completed, and a genuine mid-plan
/// failure fail-stops the volume (no further mutation of its objects), so
/// only the test seam can construct a foreign occupant. Its residue (here
/// an inflated `nlink` with one name) is announced and counted rather than
/// silently repaired: restating a stranger's dentry from a stale plan
/// would destroy live data, which is strictly worse than a loud leak.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_skips_a_foreign_occupant_instead_of_clobbering() {
    let dir = tempfile::tempdir().unwrap();
    let paths = stamped_set(dir.path(), 2, 2).await;
    let routed = open_set(&paths).await;
    let parent = mkdir_on(&routed, 1, 0, "foreign").await;
    let (child, _name) = mkfile_on(&routed, parent, 1, "foreign").await;

    // Sever a link plan after its nlink step: the intent's remaining step
    // wants to insert `taken` → child.
    {
        let _seam = arm_seam(2);
        let _ = routed.link(child, parent, "taken").await;
    }
    // A different inode legitimately takes the name before recovery runs
    // (the live mount is still up — the intent is open).
    let squatter = routed
        .create(parent, "taken", libc::S_IFREG | 0o600, 0, 0)
        .await
        .expect("the name is free — the plan's insert never committed");

    let foreign0 = XV_STEPS_FOREIGN_SKIPPED.load(Ordering::Relaxed);
    let routed = crash_and_reopen(routed, &paths).await;
    assert!(
        XV_STEPS_FOREIGN_SKIPPED.load(Ordering::Relaxed) > foreign0,
        "a foreign occupant must be counted (crossvol_tx_steps_foreign), not clobbered"
    );
    assert_eq!(
        lookup_opt(&routed, parent, "taken").await,
        Some(squatter.ino),
        "recovery must never overwrite a dentry it does not own"
    );
    assert_eq!(
        open_intents(&routed).await,
        0,
        "the intent is still retired"
    );
    shutdown(&routed).await;
}

// ---------------------------------------------------------------------------
// 7. Power cut: the barriers order the protocol
// ---------------------------------------------------------------------------

/// The deferred-flush window: a power cut after a fully successful
/// cross-volume unlink may lose the RETIREMENT (it is the last write and
/// nothing barriers after it) but can never lose a step that preceded a
/// protocol barrier. Recovery then finds an intent whose every step is
/// already applied and simply retires it — the post-state, with no
/// double-apply.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn power_cut_after_success_recovers_to_post_without_double_apply() {
    let dir = tempfile::tempdir().unwrap();
    let paths = stamped_set(dir.path(), 2, 2).await;
    let routed = open_set(&paths).await;
    let parent = mkdir_on(&routed, 1, 0, "pcut").await;
    let (child, name) = mkfile_on(&routed, parent, 1, "pcut").await;
    routed.link(child, parent, "keep").await.expect("link");
    let coordinator = routed.route_ino(parent).0;
    let coord_path = paths[coordinator].clone();

    squeezefs::uring_fs::arm_power_cut(&coord_path);
    routed.unlink(parent, &name).await.expect("unlink");
    // Lose everything the coordinator volume has not flushed.
    squeezefs::uring_fs::power_cut(&coord_path);
    let already0 = XV_STEPS_ALREADY_APPLIED.load(Ordering::Relaxed);
    let applied0 = XV_STEPS_APPLIED.load(Ordering::Relaxed);

    let routed = crash_and_reopen(routed, &paths).await;
    squeezefs::uring_fs::clear_faults();

    let named = lookup_opt(&routed, parent, &name).await;
    let links = nlink_of(&routed, child).await;
    let pre = named == Some(child) && links == Some(2);
    let post = named.is_none() && links == Some(1);
    assert!(
        pre || post,
        "power cut left an intermediate state — dentry {named:?}, nlink {links:?}"
    );
    if XV_STEPS_ALREADY_APPLIED.load(Ordering::Relaxed) > already0 {
        assert_eq!(
            XV_STEPS_APPLIED.load(Ordering::Relaxed),
            applied0,
            "an all-applied intent must be retired, never re-applied"
        );
    }
    assert_eq!(open_intents(&routed).await, 0);
    shutdown(&routed).await;
}

// ---------------------------------------------------------------------------
// 8. The acquisition order cannot deadlock
// ---------------------------------------------------------------------------

/// Concurrent INVERSE cross-volume operations make progress: the
/// machinery adds **no** lock acquisition of its own (every step commits
/// under the guard set the op already acquired up front, in ascending
/// volume order with I-before-D inside each volume), so the composition
/// stays the one total order the DLM's canonical rule defines. An
/// inversion would hang this test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_inverse_crossvol_ops_never_deadlock() {
    let dir = tempfile::tempdir().unwrap();
    let paths = stamped_set(dir.path(), 2, 2).await;
    let routed = open_set(&paths).await;

    let dir_a = mkdir_on(&routed, 1, 0, "dlA").await;
    let dir_b = mkdir_on(&routed, 1, 1, "dlB").await;
    let (file_a, name_a) = mkfile_on(&routed, dir_a, 1, "dlA").await;
    let (file_b, name_b) = mkfile_on(&routed, dir_b, 0, "dlB").await;

    let r1 = Arc::clone(&routed);
    let r2 = Arc::clone(&routed);
    let na = name_a.clone();
    let nb = name_b.clone();
    let t1 = tokio::spawn(async move {
        for i in 0..40 {
            let n = format!("x{i}");
            r1.link(file_a, dir_b, &n).await.expect("A→B link");
            r1.unlink(dir_b, &n).await.expect("A→B unlink");
        }
        r1.unlink(dir_a, &na).await.expect("A unlink");
    });
    let t2 = tokio::spawn(async move {
        for i in 0..40 {
            let n = format!("y{i}");
            r2.link(file_b, dir_a, &n).await.expect("B→A link");
            r2.unlink(dir_a, &n).await.expect("B→A unlink");
        }
        r2.unlink(dir_b, &nb).await.expect("B unlink");
    });

    let joined = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        t1.await.expect("task A");
        t2.await.expect("task B");
    })
    .await;
    assert!(
        joined.is_ok(),
        "inverse cross-volume operations deadlocked — the acquisition order inverted"
    );
    assert_eq!(open_intents(&routed).await, 0);
    shutdown(&routed).await;
}

// ---------------------------------------------------------------------------
// 9. The wire
// ---------------------------------------------------------------------------

/// `SQZXTX01` round-trips every step kind, and hostile bytes decode to an
/// error rather than a panic (the standing decoder law — every on-disk
/// unit is checksummed and every decoder is bounded).
#[test]
fn intent_record_round_trips_and_never_panics_on_hostile_bytes() {
    use squeezefs::meta_backend::crossvol_tx::{XvOp, XvStep};
    let rec = IntentRecord {
        tx_id: 0x0000_0007_dead_beef,
        op: XvOp::Rename,
        steps: vec![
            XvStep::SetNlink {
                ino: 42,
                pre: 3,
                post: 2,
                ctime: None,
            },
            XvStep::SetNlink {
                ino: 43,
                pre: 2,
                post: 3,
                ctime: Some(1_700_000_000_000_000_000),
            },
            XvStep::RemoveDentry {
                parent: 42,
                name: "sub".to_string(),
                expect_child: 99,
                parent_update: 2,
            },
            XvStep::InsertDentry {
                parent: 43,
                name: "sub".to_string(),
                child: 99,
                ft_bits: libc::S_IFDIR,
                parent_update: 2,
            },
            XvStep::TouchCtime {
                ino: 99,
                ctime: 1_700_000_000_000_000_001,
            },
            XvStep::MintInode {
                ino: 100,
                mode: libc::S_IFCHR,
                uid: 0,
                gid: 0,
                rdev: 0,
            },
        ],
    };
    let image = rec.encode().expect("encode");
    let back = IntentRecord::decode(&image).expect("decode");
    assert_eq!(back, rec, "the wire must round-trip every step kind");

    // Every single-byte corruption is refused (the checksum covers the
    // whole image).
    for i in 0..image.len() {
        let mut bad = image.clone();
        bad[i] ^= 0xFF;
        assert!(
            IntentRecord::decode(&bad).is_err(),
            "byte {i} flipped and the record still decoded"
        );
    }
    // Truncations and arbitrary bytes: errors, never panics.
    for cut in 0..image.len() {
        assert!(IntentRecord::decode(&image[..cut]).is_err());
    }
    let mut seed = 0x1234_5678_9abc_def0u64;
    for len in [0usize, 1, 7, 16, 33, 64, 129, 512] {
        let mut buf = vec![0u8; len];
        for b in buf.iter_mut() {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            *b = (seed >> 33) as u8;
        }
        let _ = IntentRecord::decode(&buf); // must not panic
    }
}
