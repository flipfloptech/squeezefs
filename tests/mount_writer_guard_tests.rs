//! D0 — the single-writer mount guard (PR M1,
//! `docs/design-metadata-throughput.md` §5.0).
//!
//! The baseline's incident 4: two daemons mounted ONE metadata volume
//! concurrently — both ran RAM-authoritative node caches and journal heads
//! over the same ring, and **nothing refused**. The v3 engine is
//! single-writer *by construction*; these tests define the guard that
//! enforces it:
//!
//! - **Layer A** — same-host exclusivity via `flock(LOCK_EX | LOCK_NB)` on a
//!   **dedicated daemon-lifetime guard fd** (never a `uring_fs` cache fd:
//!   the `FdCache` evicts by LRU and would silently drop the lock
//!   mid-session — pinned by the fd-cache-churn case). Probes stay
//!   lock-free. Crash reclaim is kernel-instant.
//! - **Layer B1** — cross-host **enforcement** via NVMe Persistent
//!   Reservations (Write Exclusive) on `RESCAP`-capable namespaces,
//!   exercised here against the in-memory `ReservationClient` fake:
//!   acquire-conflict arbitration (fresh claim ⇒ refuse, TTL-stale ⇒
//!   PREEMPT), the **fence signal at the barrier layer** (reservation-
//!   conflict errno at `fdatasync` ⇒ immediate `failed` latch +
//!   `writer_guard_fenced` — asserted separately for the strict
//!   `commit_tx` path and the checkpoint tick path, Issue 14), the
//!   consecutive-generic-barrier-failure rung, host-identity stability,
//!   and the PTPL-lapse heartbeat Report re-check.
//! - **Layer B2** — the `writer_claim` record (identity + mount-time
//!   detection): fresh-foreign refusal naming the holder, same-host
//!   dead-pid-proof instant reclaim, NO automatic cross-host takeover on
//!   non-PR volumes (stale-foreign refuses naming `squeezefs claim
//!   clear`), the claim committed + barriered **before** the checkpoint
//!   task exists, clean-unmount deletion, and the operator-attested
//!   `claim clear` verb.
//!
//! Two-process cases reuse the crash-harness re-exec pattern
//! (`SQUEEZEFS_GUARD_CHILD*` env selects a child branch of THIS binary —
//! no production CLI surface added). Env-knob mutation follows the
//! repo's serial-gate convention (`cargo test -- --test-threads=1`).

use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use squeezefs::fuse_client::{CLIENT_HEARTBEAT_INTERVAL_SECS, CLIENT_STALE_TTL_SECS};
use squeezefs::meta_backend::kv::backend::{
    test_conveyor_hold_release, ClaimClearOutcome, KvMetaBackend, WriterClaim,
    TEST_CONVEYOR_EMPTY_TAIL_PARKED, TEST_CONVEYOR_HOLD_EMPTY_DRAIN_TAIL, TEST_CONVEYOR_HOLD_STAGE,
    WRITER_CLAIM_XATTR,
};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::reservation::{self, FakeNvmeNamespace, FakeReservationClient};
use squeezefs::meta_backend::Metadata;
use squeezefs::uring_fs;
use tempfile::{tempdir, NamedTempFile};

const VOL_LEN: u64 = 64 * 1024 * 1024;

fn opts() -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: Some(1024 * 1024),
        force: false,
        full_wipe: false,
        format_config_xattr: None,
    }
}

async fn fresh_volume() -> NamedTempFile {
    let f = NamedTempFile::new().unwrap();
    f.as_file().set_len(VOL_LEN).unwrap();
    format_v3(f.path(), VOL_LEN, &opts()).await.unwrap();
    f
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn our_boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .expect("boot_id readable on Linux")
        .trim()
        .to_string()
}

/// A pid that provably cannot be alive: spawn `/bin/true`, reap it, use
/// its pid (reuse within a test's lifetime is astronomically unlikely and
/// the dead-pid proof additionally requires the boot id to match).
fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().expect("spawn /bin/true");
    let pid = child.id();
    child.wait().expect("reap /bin/true");
    pid
}

/// Plant a `writer_claim` on a freshly-formatted volume and leave it
/// behind: open (fresh volume — nothing refuses), overwrite the record
/// with the forged value, barrier it, and drop WITHOUT a clean shutdown
/// (a clean shutdown deletes the claim). The journal carries the forged
/// value into the next mount's replay — exactly the residue a crashed
/// holder leaves.
async fn forge_claim(path: &std::path::Path, claim: &WriterClaim) {
    let be = KvMetaBackend::open(path).await.expect("forge open");
    // VAL-2: `writer_claim` is an internal record — the generic
    // `Metadata::setxattr` mirrors the FUSE allowlist and refuses it.
    // The guard's own writers (which this forge impersonates) ride the
    // unscreened internal entry point.
    be.setxattr_internal(1, WRITER_CLAIM_XATTR, &claim.encode())
        .await
        .expect("forge setxattr");
    be.sync_device().await.expect("forge barrier");
    drop(be);
}

/// Read the stored claim through a read-only probe (never blocked, never
/// writes).
async fn probe_claim(path: &std::path::Path) -> Option<WriterClaim> {
    let probe = KvMetaBackend::open_probe(path).await.expect("probe mount");
    probe.read_writer_claim().await
}

/// Panic-safe fault disarm: the uring_fs fault shim is process-global, so
/// a failing assertion between `arm_*` and `clear_faults` must not leak an
/// armed fault into the next test (the crash-suite Cleanup precedent).
struct FaultGuard;

impl Drop for FaultGuard {
    fn drop(&mut self) {
        uring_fs::clear_faults();
    }
}

/// Scoped `SQUEEZEFS_META_FLUSH_INTERVAL_MS` override (strict mode = "0");
/// restores on drop. Tests run under the repo's serial gate.
struct FlushEnv {
    prior: Option<String>,
}

impl FlushEnv {
    fn set(val: &str) -> Self {
        let prior = std::env::var("SQUEEZEFS_META_FLUSH_INTERVAL_MS").ok();
        std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", val);
        Self { prior }
    }
}

impl Drop for FlushEnv {
    fn drop(&mut self) {
        match &self.prior {
            Some(v) => std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", v),
            None => std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS"),
        }
    }
}

// ===========================================================================
// Layer A — dedicated-fd flock (same-host exclusivity).
// ===========================================================================

/// The measured incident, exactly: a second concurrent write-mode open of
/// one metadata volume must be refused loudly while the first holds, and
/// the refusal names the single-writer guard. The first mount keeps
/// serving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_second_open_refused_while_first_holds() {
    let vol = fresh_volume().await;
    let first = KvMetaBackend::open(vol.path()).await.expect("first mount");

    let second = KvMetaBackend::open(vol.path()).await;
    let err = match second {
        Ok(_) => panic!(
            "second concurrent open of one metadata volume must be refused \
             (single-writer guard) — incident 4 reproduced"
        ),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("single-writer") || err.contains("writer lock"),
        "refusal must name the single-writer guard: {err}"
    );

    // The refused attempt must not have perturbed the holder.
    Metadata::create(first.as_ref(), 1, "after", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("holder keeps serving after a refused second open");
    first.shutdown().await.unwrap();

    // With the holder gone (clean unmount), the volume mounts again.
    let re = KvMetaBackend::open(vol.path()).await.expect("remount");
    re.shutdown().await.unwrap();
}

/// Layer A teardown race (2026-07-26 flake class): `drop`-without-
/// `shutdown` releases the guard flock only when the LAST `Arc` dies, and
/// the detached checkpoint / times-drain / conveyor pass tasks each hold
/// an upgraded `Arc` for the duration of a pass — so an INSTANT
/// same-process reopen can find the flock still held by a backend that is
/// provably in teardown and refuse `Busy … age=0s`. That was the
/// `kv_backend_tests::v3_strict_mode_commits_barrier_per_commit` ~1/8
/// standalone flake (reported 2026-07-25), and the
/// `kv_smo_crash_completeness_tests` harness papers over the same window
/// with a poll-retry loop.
///
/// The contract pinned here: when the flock holder's on-volume claim
/// names THIS process (pid + boot) and the caller no longer holds a live
/// handle, `open` waits out the teardown bounded instead of refusing —
/// crash-equivalent drop→reopen is a sanctioned product shape (remount
/// paths, the strict-mode replay check), not harness plumbing. A
/// genuinely live same-process double mount still refuses (the wait
/// expires — `test_second_open_refused_while_first_holds` above), and
/// foreign holders never wait at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_reopen_waits_out_same_process_teardown_pin() {
    let vol = fresh_volume().await;
    let be = KvMetaBackend::open(vol.path()).await.expect("first mount");
    Metadata::create(be.as_ref(), 1, "pinned", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create under the first mount");

    // Deterministic stand-in for the mid-pass background-task pin: a
    // second Arc outlives the caller's drop by ~300 ms (exactly what a
    // checkpoint tick's upgraded Weak does, without the timing luck).
    let pin = be.clone();
    drop(be);
    let unpin = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        drop(pin);
    });

    // The instant reopen must wait out the pin, not refuse: the claim on
    // the volume names our own pid+boot and the flock frees the moment
    // the pinned Arc dies.
    let re = KvMetaBackend::open(vol.path()).await.expect(
        "instant same-process reopen must wait out a teardown-pinned \
         holder (bounded), never refuse Busy at age=0s",
    );
    assert_eq!(
        Metadata::lookup(re.as_ref(), 1, "pinned")
            .await
            .expect("reopened volume serves the pre-drop create")
            .mode
            & libc::S_IFMT,
        libc::S_IFREG,
        "replayed create survives the drop→reopen"
    );
    unpin.join().unwrap();
    re.shutdown().await.unwrap();
}

/// The dedicated-fd requirement (design §5.0 Layer A): `uring_fs`'s
/// per-worker `FdCache` evicts by LRU, and a flock taken on a cache-owned
/// fd would be **silently released at eviction**, reopening the
/// double-mount window mid-session. Storm enough distinct paths through
/// `uring_fs` to cycle every worker's cache several times over, then
/// assert the refusal still holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_flock_survives_uring_fd_cache_churn() {
    let vol = fresh_volume().await;
    let first = KvMetaBackend::open(vol.path()).await.expect("first mount");

    // Upper-bound the pool's aggregate fd-cache capacity from its own
    // formula (soft-NOFILE/4, split across clamp(nproc,4,8) workers, each
    // clamped to 16..=1024) and storm 2x that many distinct paths.
    let mut rl = libc::rlimit {
        rlim_cur: 1024,
        rlim_max: 1024,
    };
    // SAFETY: plain getrlimit into a stack struct.
    let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } == 0 {
        rl.rlim_cur as usize
    } else {
        1024
    };
    let workers = std::env::var("SQUEEZEFS_URING_FS_WORKERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|n| n.clamp(1, 64))
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .clamp(4, 8)
        });
    let cap_per_worker = (soft / 4 / workers.max(1)).clamp(16, 1024);
    let storm = (workers * cap_per_worker * 2).min(20_000);

    let dir = tempdir().unwrap();
    for i in 0..storm {
        uring_fs::write_at(dir.path().join(format!("churn-{i}")), 0, vec![1u8])
            .await
            .expect("churn write");
    }

    // Every worker's cache has cycled far past its capacity; if the guard
    // lock rode a cache fd it is gone now.
    let second = KvMetaBackend::open(vol.path()).await;
    assert!(
        second.is_err(),
        "the writer flock must survive uring_fs FdCache churn ({storm} distinct \
         paths) — a cache-owned lock fd would have been LRU-evicted"
    );

    first.shutdown().await.unwrap();
}

/// Read-only probes (format preflight, status, config bootstrap) take no
/// lock and must keep working against a live-mounted volume — that is the
/// preflight's whole point.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_open_probe_never_blocked_by_held_mount() {
    let vol = fresh_volume().await;
    let held = KvMetaBackend::open(vol.path()).await.expect("mount");
    Metadata::create(held.as_ref(), 1, "seen", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    held.sync_device().await.unwrap();

    let probe = KvMetaBackend::open_probe(vol.path())
        .await
        .expect("probe of a live-mounted volume must never be blocked");
    assert!(
        probe.lookup(1, "seen").await.is_ok(),
        "probe serves the replayed state"
    );
    drop(probe);
    held.shutdown().await.unwrap();
}

// ===========================================================================
// Layer B2 — the writer_claim record: ordering, lifecycle, detection.
// ===========================================================================

/// The claim is the volume's first post-replay mutation *by construction*:
/// committed AND barriered before `spawn_checkpoint_task` exists (the
/// task-spawn hook records the event order). This retires the review's
/// point (iii): no maintenance record can precede the claim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_claim_committed_and_barriered_before_checkpoint_task() {
    let vol = fresh_volume().await;
    let be = KvMetaBackend::open(vol.path()).await.expect("mount");
    let trace = be.open_trace();
    let pos = |ev: &str| {
        trace
            .iter()
            .position(|e| *e == ev)
            .unwrap_or_else(|| panic!("open trace missing event {ev:?}: {trace:?}"))
    };
    let flock = pos("flock_acquired");
    let committed = pos("claim_committed");
    let barriered = pos("claim_barriered");
    let spawned = pos("checkpoint_task_spawned");
    assert!(
        flock < committed && committed < barriered && barriered < spawned,
        "pinned order flock -> claim commit -> claim barrier -> checkpoint task, got {trace:?}"
    );
    be.shutdown().await.unwrap();
}

/// A write mount commits a claim naming this process; a clean unmount
/// deletes it (beside the client-registration removal in teardown).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_claim_written_at_mount_and_deleted_on_clean_unmount() {
    let vol = fresh_volume().await;
    let be = KvMetaBackend::open(vol.path()).await.expect("mount");
    let claim = be
        .read_writer_claim()
        .await
        .expect("a write mount must commit a writer_claim");
    assert_eq!(claim.pid, std::process::id(), "claim names this process");
    assert_eq!(claim.boot, our_boot_id(), "claim names this boot");
    assert!(
        claim.age_secs(now_secs()) <= CLIENT_HEARTBEAT_INTERVAL_SECS,
        "freshly-committed claim carries a fresh heartbeat"
    );
    be.shutdown().await.unwrap();

    assert!(
        probe_claim(vol.path()).await.is_none(),
        "a clean unmount must delete the writer_claim"
    );
}

/// A fresh foreign claim (age <= TTL) refuses the mount LOUDLY, naming the
/// holder `{id, pid, boot, age}`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fresh_foreign_claim_refused_names_holder() {
    let vol = fresh_volume().await;
    let foreign = WriterClaim {
        id: "foreign-mount-1".into(),
        ts: now_secs(),
        pid: 4_000_000, // beyond pid_max: kill(pid,0) can never name a live process
        boot: "11111111-2222-3333-4444-555555555555".into(),
    };
    forge_claim(vol.path(), &foreign).await;

    let err = match KvMetaBackend::open(vol.path()).await {
        Ok(_) => panic!("a fresh foreign writer_claim must refuse the mount"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("foreign-mount-1"),
        "refusal names the holder id: {err}"
    );
    assert!(
        err.contains("4000000"),
        "refusal names the holder pid: {err}"
    );
    // Never auto-taken: the claim is intact after the refusal.
    assert_eq!(probe_claim(vol.path()).await, Some(foreign));
}

/// Same-host crash recovery: a claim whose boot id matches THIS boot and
/// whose pid is provably dead (`kill(pid,0) == ESRCH`) is reclaimed
/// automatically and instantly — no TTL wait, even on a heartbeat-fresh
/// claim (kill -9 leaves one).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_dead_pid_proof_same_host_instant_reclaim() {
    let vol = fresh_volume().await;
    let crashed = WriterClaim {
        id: "crashed-same-host".into(),
        ts: now_secs(), // FRESH: the proof, not the TTL, is what reclaims
        pid: dead_pid(),
        boot: our_boot_id(),
    };
    forge_claim(vol.path(), &crashed).await;

    let started = std::time::Instant::now();
    let be = KvMetaBackend::open(vol.path())
        .await
        .expect("dead-pid-proven same-host claim must reclaim automatically");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "reclaim is instant (no TTL wait)"
    );
    let claim = be.read_writer_claim().await.expect("re-claimed");
    assert_eq!(claim.pid, std::process::id(), "the claim now names us");
    be.shutdown().await.unwrap();
}

/// A live same-host pid (boot matches, `kill(pid,0)` succeeds) is NOT a
/// dead-pid proof: a fresh claim refuses naming the holder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_live_pid_same_host_fresh_claim_refused() {
    let vol = fresh_volume().await;
    let alive = WriterClaim {
        id: "alive-same-host".into(),
        ts: now_secs(),
        pid: 1, // init: always alive (kill(1,0) = EPERM, not ESRCH)
        boot: our_boot_id(),
    };
    forge_claim(vol.path(), &alive).await;

    let err = match KvMetaBackend::open(vol.path()).await {
        Ok(_) => panic!("a fresh claim with a live pid must refuse"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("alive-same-host"), "names the holder: {err}");
}

/// The boot-id-spoof case (design §5.0 B2): a TTL-stale claim from
/// ANOTHER boot ("cross-host") on a non-PR volume is **never auto-taken**
/// — automatic cross-host takeover is disabled where no device
/// enforcement exists; the refusal names the `squeezefs claim clear`
/// remedy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_stale_cross_host_claim_on_non_pr_volume_refused_names_claim_clear() {
    let vol = fresh_volume().await;
    let stale_foreign = WriterClaim {
        id: "other-host-crashed".into(),
        ts: now_secs().saturating_sub(CLIENT_STALE_TTL_SECS + 120),
        pid: std::process::id(), // spoofed: OUR pid, alive — but the boot differs
        boot: "99999999-8888-7777-6666-555555555555".into(),
    };
    forge_claim(vol.path(), &stale_foreign).await;

    let err = match KvMetaBackend::open(vol.path()).await {
        Ok(_) => panic!(
            "a TTL-stale cross-host claim on a non-PR volume must NOT be auto-taken \
             (paused-holder usurpation is undetectable there — design §5.0)"
        ),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("claim clear"),
        "refusal names the operator remedy `squeezefs claim clear`: {err}"
    );
    assert_eq!(
        probe_claim(vol.path()).await,
        Some(stale_foreign),
        "the claim is untouched after the refusal (never auto-taken)"
    );
}

/// Same-process re-open after a drop-without-shutdown (test churn, admin
/// tooling): our own residual claim — pid == us, boot == ours — reclaims
/// instantly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_own_process_residual_claim_reclaimed_on_reopen() {
    let vol = fresh_volume().await;
    {
        let be = KvMetaBackend::open(vol.path()).await.expect("first mount");
        be.sync_device().await.unwrap();
        drop(be); // no shutdown: claim stays behind, flock releases
    }
    let be = KvMetaBackend::open(vol.path())
        .await
        .expect("our own residual claim must reclaim instantly on re-open");
    be.shutdown().await.unwrap();
}

/// The heartbeat refreshes the claim timestamp (one staleness law with the
/// client registrations: CLIENT_HEARTBEAT_INTERVAL_SECS cadence).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_guard_heartbeat_refreshes_claim_timestamp() {
    let vol = fresh_volume().await;
    let be = KvMetaBackend::open(vol.path()).await.expect("mount");

    // Age our own claim far past the TTL, as if the heartbeat had stalled.
    let mut aged = be.read_writer_claim().await.expect("claimed at mount");
    aged.ts = now_secs().saturating_sub(CLIENT_STALE_TTL_SECS + 300);
    be.setxattr_internal(1, WRITER_CLAIM_XATTR, &aged.encode())
        .await
        .unwrap();

    be.guard_heartbeat().await;

    let refreshed = be.read_writer_claim().await.expect("claim present");
    assert!(
        refreshed.age_secs(now_secs()) <= CLIENT_HEARTBEAT_INTERVAL_SECS,
        "guard_heartbeat must re-commit a fresh timestamp (got age {}s)",
        refreshed.age_secs(now_secs())
    );
    be.shutdown().await.unwrap();
}

// ===========================================================================
// `squeezefs claim clear` — the operator-attested recovery verb.
// ===========================================================================

/// The verb refuses a FRESH claim: clearing a live writer's claim is the
/// one thing the attestation must not automate away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_claim_clear_refuses_fresh_claim() {
    let vol = fresh_volume().await;
    let fresh = WriterClaim {
        id: "live-elsewhere".into(),
        ts: now_secs(),
        pid: 4_000_001,
        boot: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
    };
    forge_claim(vol.path(), &fresh).await;

    let out = KvMetaBackend::claim_clear(vol.path()).await;
    assert!(
        out.is_err(),
        "claim clear must refuse a fresh (live) claim, got {out:?}"
    );
    assert_eq!(
        probe_claim(vol.path()).await,
        Some(fresh),
        "refused clear leaves the claim intact"
    );
}

/// The verb clears a TTL-stale claim (re-verified under its own probe),
/// reports the holder it removed, and leaves the volume mountable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_claim_clear_clears_stale_claim() {
    let vol = fresh_volume().await;
    let stale = WriterClaim {
        id: "other-host-crashed".into(),
        ts: now_secs().saturating_sub(CLIENT_STALE_TTL_SECS + 600),
        pid: 4_000_002,
        boot: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
    };
    forge_claim(vol.path(), &stale).await;

    // Sanity: the mount itself refuses this shape (cross-host stale,
    // non-PR) — the verb is the designed way out.
    assert!(KvMetaBackend::open(vol.path()).await.is_err());

    match KvMetaBackend::claim_clear(vol.path())
        .await
        .expect("stale claim clears")
    {
        ClaimClearOutcome::Cleared(holder) => assert_eq!(holder, stale),
        other => panic!("expected Cleared(stale holder), got {other:?}"),
    }
    assert!(
        probe_claim(vol.path()).await.is_none(),
        "the claim record is durably removed"
    );

    let be = KvMetaBackend::open(vol.path())
        .await
        .expect("volume mounts after the attested clear");
    be.shutdown().await.unwrap();
}

/// Clearing nothing is a clean no-op outcome.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_claim_clear_no_claim_is_noop() {
    let vol = fresh_volume().await;
    match KvMetaBackend::claim_clear(vol.path()).await.expect("noop") {
        ClaimClearOutcome::NoClaim => {}
        other => panic!("expected NoClaim, got {other:?}"),
    }
}

/// The preflight-style live-check: the verb refuses while the volume is
/// live-mounted on this host (the flock names the conflict) — an operator
/// cannot clear the claim out from under a serving daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_claim_clear_refuses_live_mounted_volume() {
    let vol = fresh_volume().await;
    let held = KvMetaBackend::open(vol.path()).await.expect("mount");

    let out = KvMetaBackend::claim_clear(vol.path()).await;
    assert!(
        out.is_err(),
        "claim clear must refuse while a live mount holds the volume, got {out:?}"
    );

    held.shutdown().await.unwrap();
}

// ===========================================================================
// Layer B1 — NVMe Persistent Reservations against the in-memory fake.
// ===========================================================================

/// A PR-capable namespace mounts in enforcement mode: register + acquire
/// Write Exclusive, `writer_guard_mode` reports `flock+pr`, and a clean
/// unmount releases the reservation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_pr_acquired_at_mount_released_at_clean_unmount() {
    let vol = fresh_volume().await;
    let ns = FakeNvmeNamespace::new();
    reservation::install_override(
        vol.path(),
        FakeReservationClient::new(ns.clone(), "nqn.2026-07.io.squeezefs:host-a", "hostid-a"),
    );

    let be = KvMetaBackend::open(vol.path()).await.expect("PR mount");
    assert_eq!(be.writer_guard_mode(), "flock+pr");
    let holder = ns.holder().expect("mount acquired the WE reservation");
    assert!(ns.is_registered(holder));

    be.shutdown().await.unwrap();
    assert_eq!(
        ns.holder(),
        None,
        "clean unmount must release the reservation"
    );
    reservation::clear_override(vol.path());
}

/// `RESCAP == 0` (no reservation support) degrades to detection grade —
/// `flock+claim` — and the mount proceeds (never fails on a non-PR
/// namespace).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_rescap_zero_degrades_to_detection_grade() {
    let vol = fresh_volume().await;
    let ns = FakeNvmeNamespace::without_pr_support();
    reservation::install_override(
        vol.path(),
        FakeReservationClient::new(ns.clone(), "nqn.2026-07.io.squeezefs:host-a", "hostid-a"),
    );

    let be = KvMetaBackend::open(vol.path()).await.expect("mount");
    assert_eq!(be.writer_guard_mode(), "flock+claim");
    assert_eq!(ns.holder(), None, "no reservation is taken without RESCAP");
    be.shutdown().await.unwrap();
    reservation::clear_override(vol.path());
}

/// Acquire-conflict arbitration, fresh holder: another registrant holds
/// the reservation and the claim record is heartbeat-fresh ⇒ refuse loud,
/// naming the holder. Never preempted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_pr_acquire_conflict_fresh_claim_refused() {
    let vol = fresh_volume().await;
    let foreign_key = 0xF0F0_F0F0_F0F0_F0F0u64;
    let fresh = WriterClaim {
        id: "pr-holder-live".into(),
        ts: now_secs(),
        pid: 4_000_003,
        boot: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
    };
    forge_claim(vol.path(), &fresh).await;

    let ns = FakeNvmeNamespace::new();
    ns.seed_holder(foreign_key);
    reservation::install_override(
        vol.path(),
        FakeReservationClient::new(ns.clone(), "nqn.2026-07.io.squeezefs:host-b", "hostid-b"),
    );

    let err = match KvMetaBackend::open(vol.path()).await {
        Ok(_) => panic!("PR conflict with a fresh claim must refuse the mount"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("pr-holder-live"), "names the holder: {err}");
    assert_eq!(
        ns.holder(),
        Some(foreign_key),
        "the fresh holder is never preempted"
    );
    assert_eq!(ns.preempt_count(), 0);
    reservation::clear_override(vol.path());
}

/// Acquire-conflict arbitration, TTL-stale holder: preemption is safe
/// *because the device fences* — the mount PREEMPTs the stale key, takes
/// the reservation, and proceeds. (Cross-host takeover is automatic
/// exactly where enforcement exists.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_pr_acquire_conflict_ttl_stale_preempted() {
    let vol = fresh_volume().await;
    let stale_key = 0xDEAD_DEAD_DEAD_DEADu64;
    let stale = WriterClaim {
        id: "pr-holder-crashed".into(),
        ts: now_secs().saturating_sub(CLIENT_STALE_TTL_SECS + 300),
        pid: 4_000_004,
        boot: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
    };
    forge_claim(vol.path(), &stale).await;

    let ns = FakeNvmeNamespace::new();
    ns.seed_holder(stale_key);
    reservation::install_override(
        vol.path(),
        FakeReservationClient::new(ns.clone(), "nqn.2026-07.io.squeezefs:host-b", "hostid-b"),
    );

    let be = KvMetaBackend::open(vol.path())
        .await
        .expect("TTL-stale PR holder must be preempted (device-fenced takeover)");
    assert_eq!(ns.preempt_count(), 1, "exactly one PREEMPT action");
    let holder = ns.holder().expect("we hold the reservation now");
    assert_ne!(holder, stale_key, "the stale key was preempted out");
    assert!(
        !ns.is_registered(stale_key),
        "preempt unregisters the victim key"
    );
    let claim = be.read_writer_claim().await.expect("re-claimed");
    assert_eq!(claim.pid, std::process::id());
    be.shutdown().await.unwrap();
    reservation::clear_override(vol.path());
}

/// Host-identity stability (design §5.0 B1 pt 6): an hostnqn/hostid change
/// against the identity recorded at mount means our registration is *not
/// ours* — the heartbeat re-check fail-stops rather than trust it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_host_identity_mismatch_fail_stops() {
    let vol = fresh_volume().await;
    let ns = FakeNvmeNamespace::new();
    let client =
        FakeReservationClient::new(ns.clone(), "nqn.2026-07.io.squeezefs:host-a", "hostid-a");
    reservation::install_override(vol.path(), client.clone());

    let be = KvMetaBackend::open(vol.path()).await.expect("PR mount");
    client.set_identity("nqn.2026-07.io.squeezefs:host-CHANGED", "hostid-CHANGED");

    be.guard_heartbeat().await;
    assert!(
        be.is_failed(),
        "identity mismatch at the heartbeat re-check must fail-stop \
         (the registration is scoped to the host identity, not the key)"
    );
    reservation::clear_override(vol.path());
}

/// PTPL lapse, benign half: a target power cycle silently cleared the
/// reservation and nothing else took it — the heartbeat Report re-check
/// re-registers + re-acquires and counts it (`writer_guard_pr_reacquires`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ptpl_lapse_reacquire_counted() {
    let vol = fresh_volume().await;
    let ns = FakeNvmeNamespace::new();
    reservation::install_override(
        vol.path(),
        FakeReservationClient::new(ns.clone(), "nqn.2026-07.io.squeezefs:host-a", "hostid-a"),
    );

    let be = KvMetaBackend::open(vol.path()).await.expect("PR mount");
    assert_eq!(be.writer_guard_pr_reacquires(), 0);

    ns.power_cycle(); // PTPL-less target: reservation + registrations gone

    be.guard_heartbeat().await;
    assert_eq!(
        be.writer_guard_pr_reacquires(),
        1,
        "the lapse re-check must re-acquire and count it"
    );
    assert!(ns.holder().is_some(), "holdership re-established");
    assert!(!be.is_failed(), "benign lapse is not a fail-stop");
    be.shutdown().await.unwrap();
    reservation::clear_override(vol.path());
}

/// PTPL lapse, hostile half: the re-check finds a FOREIGN holder — a
/// second mount acquired inside the lapse window. Fail-stop; never write
/// past a foreign reservation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ptpl_lapse_foreign_holder_fail_stops() {
    let vol = fresh_volume().await;
    let ns = FakeNvmeNamespace::new();
    reservation::install_override(
        vol.path(),
        FakeReservationClient::new(ns.clone(), "nqn.2026-07.io.squeezefs:host-a", "hostid-a"),
    );

    let be = KvMetaBackend::open(vol.path()).await.expect("PR mount");
    ns.power_cycle();
    ns.seed_holder(0xBEEF_BEEF_BEEF_BEEFu64);

    be.guard_heartbeat().await;
    assert!(
        be.is_failed(),
        "a foreign holder at the re-check must fail-stop the volume"
    );
    assert_eq!(
        ns.holder(),
        Some(0xBEEF_BEEF_BEEF_BEEFu64),
        "the foreign reservation is never contested from the fenced side"
    );
    reservation::clear_override(vol.path());
}

// ===========================================================================
// The register ladder — spec-strict targets (SPDK v26.05 measured,
// `.benchmarks/2026-07-17-spdk-target-scoping.md` §4/§7 Q1): after kill -9, the
// dead incarnation's registration persists under the SAME host identity;
// kernel nvmet lets the guard's IEKEY register replace it silently, but a
// spec-strict target returns Reservation Conflict — bricking remount.
// The ladder: Report → identify OWN stale registration (association wire
// host id) → unregister exactly those keys → register fresh. NEVER
// touches a foreign registration (that stays preempt/claim territory).
// ===========================================================================

use squeezefs::meta_backend::reservation::{
    register_ladder, RegisterOutcome, ReservationClient, ReservationReport,
};

/// A client wrapper forcing a specific `register` outcome while
/// delegating everything else to the fake — models strict-target corners
/// the fake cannot reach organically (a Register conflict whose report
/// shows no registration of ours; a non-conflict register error).
#[derive(Debug)]
struct RegisterFailingClient {
    inner: std::sync::Arc<FakeReservationClient>,
    errno: i32,
}

impl ReservationClient for RegisterFailingClient {
    fn rescap(&self) -> std::io::Result<u8> {
        self.inner.rescap()
    }
    fn host_identity(&self) -> std::io::Result<squeezefs::meta_backend::reservation::HostIdentity> {
        self.inner.host_identity()
    }
    fn wire_host_id(&self) -> std::io::Result<Vec<u8>> {
        self.inner.wire_host_id()
    }
    fn register(&self, _key: u64) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(self.errno))
    }
    fn unregister(&self, key: u64) -> std::io::Result<()> {
        self.inner.unregister(key)
    }
    fn acquire_write_exclusive(&self, key: u64) -> std::io::Result<()> {
        self.inner.acquire_write_exclusive(key)
    }
    fn acquire_write_exclusive_registrants_only(&self, key: u64) -> std::io::Result<()> {
        self.inner.acquire_write_exclusive_registrants_only(key)
    }
    fn preempt(&self, key: u64, victim_key: u64) -> std::io::Result<()> {
        self.inner.preempt(key, victim_key)
    }
    fn preempt_registrants_only(&self, key: u64, victim_key: u64) -> std::io::Result<()> {
        self.inner.preempt_registrants_only(key, victim_key)
    }
    fn release(&self, key: u64) -> std::io::Result<()> {
        self.inner.release(key)
    }
    fn release_registrants_only(&self, key: u64) -> std::io::Result<()> {
        self.inner.release_registrants_only(key)
    }
    fn report(&self) -> std::io::Result<ReservationReport> {
        self.inner.report()
    }
}

/// Fast path pin: no stale state ⇒ plain register, no report/unregister
/// detour — today's behavior byte-identical.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_register_ladder_fast_path_clean_namespace() {
    let ns = FakeNvmeNamespace::new(); // spec-strict
    let c = FakeReservationClient::new(ns.clone(), "nqn.2026-07.io.squeezefs:host-a", "hostid-a");
    let out = register_ladder(c.as_ref(), 0x1111).expect("clean register");
    assert_eq!(out, RegisterOutcome::Registered, "fast path outcome");
    assert!(ns.is_registered(0x1111));
    assert_eq!(
        ns.unregister_count(),
        0,
        "the fast path must issue no unregister"
    );
}

/// THE finding (scoping §4): our own stale registration (same host,
/// crashed incarnation's key) conflicts on a spec-strict target — the
/// ladder must recover it: report → unregister OUR stale key (dropping
/// the WE reservation held under it) → register fresh; the new key can
/// then acquire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_register_ladder_recovers_own_stale_registration_after_kill9() {
    let ns = FakeNvmeNamespace::new(); // spec-strict
    let host = "36ba36ac-5c0e-4ee1-8000-000000000001"; // uuid form (fabrics)
    let dead = FakeReservationClient::new(ns.clone(), "nqn.2026-07.io.squeezefs:host-a", host);
    dead.register(0xDEAD_0001).expect("incarnation 1 registers");
    dead.acquire_write_exclusive(0xDEAD_0001)
        .expect("incarnation 1 holds WE");
    drop(dead); // kill -9: device state persists

    let fresh = FakeReservationClient::new(ns.clone(), "nqn.2026-07.io.squeezefs:host-a", host);
    let out = register_ladder(fresh.as_ref(), 0xF00D_0002).expect(
        "the register ladder must recover from our own stale registration \
         on a spec-strict target (kill-9 remount — the SPDK P0 finding)",
    );
    assert_eq!(
        out,
        RegisterOutcome::RecoveredOwnStale {
            unregistered: vec![0xDEAD_0001]
        },
        "ladder names the recovered stale key"
    );
    assert!(
        !ns.is_registered(0xDEAD_0001),
        "the stale own key is unregistered"
    );
    assert!(ns.is_registered(0xF00D_0002), "the fresh key is registered");
    assert_eq!(
        ns.holder(),
        None,
        "unregistering the stale holder key released the WE reservation"
    );
    fresh
        .acquire_write_exclusive(0xF00D_0002)
        .expect("the fresh key can acquire after recovery");
    assert_eq!(ns.holder(), Some(0xF00D_0002));
}

/// Recovery is registration-scoped: with a FOREIGN holder on the
/// namespace, the ladder still recovers our own stale registration but
/// never touches the foreign holder's state — acquire arbitration stays
/// the existing conflict/claim/preempt path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_register_ladder_own_stale_with_foreign_holder_leaves_holder_alone() {
    let ns = FakeNvmeNamespace::new(); // spec-strict
    let foreign_key = 0xF0F0_F0F0u64;
    ns.seed_holder(foreign_key);
    let host = "hostid-a";
    let dead = FakeReservationClient::new(ns.clone(), "nqn.2026-07.io.squeezefs:host-a", host);
    dead.register(0xDEAD_0003)
        .expect("incarnation 1 registers (non-holder)");
    drop(dead);

    let fresh = FakeReservationClient::new(ns.clone(), "nqn.2026-07.io.squeezefs:host-a", host);
    let out = register_ladder(fresh.as_ref(), 0xF00D_0004)
        .expect("own-stale recovery works beside a foreign holder");
    assert_eq!(
        out,
        RegisterOutcome::RecoveredOwnStale {
            unregistered: vec![0xDEAD_0003]
        }
    );
    assert_eq!(
        ns.holder(),
        Some(foreign_key),
        "the foreign holder's reservation is untouched"
    );
    assert!(
        ns.is_registered(foreign_key),
        "the foreign registration is untouched"
    );
    assert!(ns.is_registered(0xF00D_0004));
}

/// The foreign-registration law: a Register conflict whose report shows
/// NO registration of ours falls through fail-closed with the conflict
/// error — the ladder must not unregister anything (foreign keys are
/// preempt/TTL territory, never the ladder's).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_register_ladder_never_unregisters_foreign_registration() {
    let ns = FakeNvmeNamespace::new();
    let foreign_key = 0xBEEF_0001u64;
    ns.seed_holder(foreign_key); // the only registration: a foreign host's
    let inner =
        FakeReservationClient::new(ns.clone(), "nqn.2026-07.io.squeezefs:host-b", "hostid-b");
    let paranoid = RegisterFailingClient {
        inner,
        errno: libc::EBADE, // a strict target conflicting for its own reasons
    };
    let err = register_ladder(&paranoid, 0xF00D_0005)
        .expect_err("a conflict with no own registration must fail closed");
    assert!(
        squeezefs::meta_backend::reservation::is_reservation_conflict(&err),
        "the original conflict class is preserved: {err:?}"
    );
    assert!(
        ns.is_registered(foreign_key),
        "the foreign registration is NEVER unregistered by the ladder"
    );
    assert_eq!(ns.holder(), Some(foreign_key), "foreign holder untouched");
    assert_eq!(ns.unregister_count(), 0, "no unregister was issued");
}

/// Error classification: a NON-conflict register error (EIO-class device
/// trouble) propagates untouched — no report, no unregister, no retry
/// masquerade.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_register_ladder_non_conflict_register_error_propagates() {
    let ns = FakeNvmeNamespace::new();
    let inner =
        FakeReservationClient::new(ns.clone(), "nqn.2026-07.io.squeezefs:host-a", "hostid-a");
    let broken = RegisterFailingClient {
        inner,
        errno: libc::EIO,
    };
    let err = register_ladder(&broken, 0xF00D_0006)
        .expect_err("a non-conflict register error must propagate");
    assert_eq!(
        err.raw_os_error(),
        Some(libc::EIO),
        "the error class is preserved verbatim: {err:?}"
    );
    assert_eq!(ns.unregister_count(), 0, "no recovery rung fired");
}

/// Fail-closed identity corner: when our own association host id is
/// unreadable/empty, the ladder must NOT match anything (an empty-to-
/// empty match would unregister a registration it cannot prove is ours)
/// — the conflict falls through loud.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_register_ladder_unreadable_own_identity_fails_closed() {
    let ns = FakeNvmeNamespace::new(); // spec-strict
    let dead = FakeReservationClient::new(ns.clone(), "nqn.2026-07.io.squeezefs:host-a", "");
    dead.register(0xDEAD_0007)
        .expect("an empty-hostid incarnation registered");
    drop(dead);

    let fresh = FakeReservationClient::new(ns.clone(), "nqn.2026-07.io.squeezefs:host-a", "");
    let err = register_ladder(fresh.as_ref(), 0xF00D_0008)
        .expect_err("an empty own host id must fail closed, never match");
    assert!(
        squeezefs::meta_backend::reservation::is_reservation_conflict(&err),
        "conflict class preserved: {err:?}"
    );
    assert!(
        ns.is_registered(0xDEAD_0007),
        "nothing was unregistered under an unprovable identity"
    );
    assert_eq!(ns.unregister_count(), 0);
}

/// The P0 finding end to end at the mount gate: kill -9 (drop without
/// shutdown — flock releases, claim + PR registration + WE reservation
/// persist), then remount against a SPEC-STRICT namespace. Today this
/// fails with "reservation register failed"; the ladder must recover:
/// dead-pid claim reclaim + own-stale unregister + fresh register +
/// acquire — `flock+pr` again, old key gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_kill9_remount_recovers_on_spec_strict_target() {
    let vol = fresh_volume().await;
    let ns = FakeNvmeNamespace::new(); // spec-strict
    reservation::install_override(
        vol.path(),
        FakeReservationClient::new(
            ns.clone(),
            "nqn.2026-07.io.squeezefs:host-a",
            "36ba36ac-5c0e-4ee1-8000-00000000000a",
        ),
    );

    let be1 = KvMetaBackend::open(vol.path()).await.expect("first mount");
    assert_eq!(be1.writer_guard_mode(), "flock+pr");
    let stale_key = ns.holder().expect("incarnation 1 holds the WE");
    drop(be1); // kill -9: no shutdown, no release — PR state persists

    let be2 = KvMetaBackend::open(vol.path()).await.expect(
        "remount after kill -9 must succeed on a spec-strict target \
         (the SPDK P0 finding: register ladder recovers our own stale \
         registration)",
    );
    assert_eq!(be2.writer_guard_mode(), "flock+pr", "enforcement regained");
    let new_key = ns
        .holder()
        .expect("the remount re-acquired the WE reservation");
    assert_ne!(new_key, stale_key, "a fresh key holds now");
    assert!(
        !ns.is_registered(stale_key),
        "the crashed incarnation's registration was recovered (unregistered)"
    );
    let claim = be2.read_writer_claim().await.expect("re-claimed");
    assert_eq!(claim.pid, std::process::id());
    be2.shutdown().await.unwrap();
    assert_eq!(ns.holder(), None, "clean unmount releases");
    reservation::clear_override(vol.path());
}

/// The lenient regression pin: on a kernel-nvmet-class target the
/// IEKEY register replaces our stale key in place — the ladder's fast
/// path — so the kill-9 remount keeps working EXACTLY as today, with
/// zero unregister commands (no double-behavior divergence).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_kill9_remount_on_lenient_target_stays_fast_path() {
    let vol = fresh_volume().await;
    let ns = FakeNvmeNamespace::lenient_register(); // kernel-nvmet model
    reservation::install_override(
        vol.path(),
        FakeReservationClient::new(
            ns.clone(),
            "nqn.2026-07.io.squeezefs:host-a",
            "36ba36ac-5c0e-4ee1-8000-00000000000b",
        ),
    );

    let be1 = KvMetaBackend::open(vol.path()).await.expect("first mount");
    let stale_key = ns.holder().expect("incarnation 1 holds the WE");
    drop(be1); // kill -9

    let be2 = KvMetaBackend::open(vol.path())
        .await
        .expect("kill-9 remount on a lenient target keeps working (M1 behavior)");
    let new_key = ns.holder().expect("re-acquired");
    assert_ne!(new_key, stale_key);
    assert_eq!(
        ns.unregister_count(),
        0,
        "lenient targets take the plain-register fast path — the ladder \
         rungs must not fire there"
    );
    be2.shutdown().await.unwrap();
    reservation::clear_override(vol.path());
}

// ===========================================================================
// The fence signal at the BARRIER layer (Issue 14): reservation-conflict
// errno at fdatasync ⇒ immediate failed latch + writer_guard_fenced, on
// BOTH barrier paths. Generic barrier errors take the consecutive rung.
// ===========================================================================

/// Strict-mode path: `commit_tx`'s own `sync_device` meets the
/// reservation-conflict errno ⇒ the volume latches `failed` immediately
/// (one barrier = the detection bound), the guard fence counter trips,
/// and subsequent mutations refuse.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fence_at_barrier_fail_stop_strict_commit_path() {
    let _strict = FlushEnv::set("0");
    let _faults = FaultGuard;
    let vol = fresh_volume().await;
    let be = KvMetaBackend::open(vol.path()).await.expect("strict mount");
    assert_eq!(be.writer_guard_fenced(), 0);

    uring_fs::arm_barrier_error(vol.path(), libc::EBADE);

    let res = Metadata::create(be.as_ref(), 1, "fenced", libc::S_IFREG | 0o644, 0, 0).await;
    assert!(
        res.is_err(),
        "a fenced strict commit must fail (its barrier carries the conflict)"
    );
    assert!(
        be.is_failed(),
        "reservation-conflict at the strict barrier must latch failed IMMEDIATELY \
         (not after {JOURNAL_FAILURE_LATCH_DOC} consecutive failures)"
    );
    assert!(
        be.writer_guard_fenced() >= 1,
        "writer_guard_fenced must count the fence"
    );

    uring_fs::clear_faults();
    let after = Metadata::create(be.as_ref(), 1, "after", libc::S_IFREG | 0o644, 0, 0).await;
    assert!(
        after.is_err(),
        "the failed latch holds until remount (EIO for mutations)"
    );
    drop(be);
}

/// Doc-name for the assertion message above (kept in sync with the
/// backend's JOURNAL_FAILURE_LATCH = 3).
const JOURNAL_FAILURE_LATCH_DOC: u64 = 3;

/// Deferred-mode path: the checkpoint TICK's barrier meets the
/// reservation-conflict errno ⇒ fail-stop within one flush cadence + one
/// barrier (the §5.0 detection bound) — today's log-and-retry-forever is
/// exactly what this pins away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fence_at_barrier_fail_stop_checkpoint_tick_path() {
    let _deferred = FlushEnv::set("50");
    let _faults = FaultGuard;
    let vol = fresh_volume().await;
    let be = KvMetaBackend::open(vol.path())
        .await
        .expect("deferred mount");

    uring_fs::arm_barrier_error(vol.path(), libc::EBADE);

    // A deferred commit acks from RAM and flags the flusher; the fence
    // surfaces at the next tick's barrier.
    Metadata::create(be.as_ref(), 1, "deferred", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("deferred commit acks before the barrier");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !be.is_failed() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        be.is_failed(),
        "the checkpoint tick's barrier must escalate the reservation conflict \
         to a fail-stop within ~one cadence — not warn every 50 ms forever"
    );
    assert!(be.writer_guard_fenced() >= 1);

    uring_fs::clear_faults();
    drop(be);
}

/// Generic (non-reservation-class) barrier errors take the consecutive-
/// failure rung: JOURNAL_FAILURE_LATCH consecutive failing barriers latch
/// the volume; a success in between resets the count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_consecutive_generic_barrier_failures_escalate_with_success_reset() {
    let _strict = FlushEnv::set("0");
    let _faults = FaultGuard;
    let vol = fresh_volume().await;
    let be = KvMetaBackend::open(vol.path()).await.expect("strict mount");

    // Two consecutive generic failures: not latched yet.
    uring_fs::arm_barrier_error(vol.path(), libc::EIO);
    for i in 0..2 {
        let r = Metadata::create(
            be.as_ref(),
            1,
            &format!("g{i}"),
            libc::S_IFREG | 0o644,
            0,
            0,
        )
        .await;
        assert!(r.is_err(), "barrier failure propagates to the committer");
    }
    assert!(
        !be.is_failed(),
        "two generic barrier failures stay below the latch"
    );

    // A successful barrier resets the consecutive count.
    uring_fs::disarm_barrier_error(vol.path());
    Metadata::create(be.as_ref(), 1, "ok", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("healthy barrier commits");
    assert!(!be.is_failed());

    // Three consecutive failures now: the volume latches failed.
    uring_fs::arm_barrier_error(vol.path(), libc::EIO);
    for i in 0..3 {
        let _ = Metadata::create(
            be.as_ref(),
            1,
            &format!("h{i}"),
            libc::S_IFREG | 0o644,
            0,
            0,
        )
        .await;
    }
    assert!(
        be.is_failed(),
        "3 consecutive generic barrier failures must latch the volume failed \
         (the JOURNAL_FAILURE_LATCH semantics, with success-reset)"
    );
    assert_eq!(
        be.writer_guard_fenced(),
        0,
        "generic escalation is NOT the reservation-fence counter"
    );

    uring_fs::clear_faults();
    drop(be);
}

// ===========================================================================
// Multi-volume mounts: claim every volume in set order; failure on volume
// k releases guards 0..k.
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_volume_set_open_releases_guards_on_partial_failure() {
    let vol_a = fresh_volume().await;
    let vol_b = fresh_volume().await;

    // Volume B is held by "another daemon" (a live backend in this
    // process — same refusal class).
    let holder_b = KvMetaBackend::open(vol_b.path()).await.expect("hold B");

    let paths = vec![
        vol_a.path().to_string_lossy().to_string(),
        vol_b.path().to_string_lossy().to_string(),
    ];
    let res = squeezefs::meta_backend::open_meta_volume_set(&paths).await;
    assert!(
        res.is_err(),
        "the set open must fail when any volume's guard refuses"
    );

    // Volume A's guard state was released: no residual claim, and the
    // volume mounts standalone immediately.
    assert!(
        probe_claim(vol_a.path()).await.is_none(),
        "failure on volume k must release claims 0..k"
    );
    let re_a = KvMetaBackend::open(vol_a.path())
        .await
        .expect("volume A re-mounts after the failed set open");
    re_a.shutdown().await.unwrap();

    holder_b.shutdown().await.unwrap();
}

// ===========================================================================
// Crash shapes: torn claim entry; two-process refusal (incident 4).
// ===========================================================================

/// A crash mid-claim-write (the claim's own journal entry torn) is the
/// standard §4.1 recovery: the failed mount reports the device error, the
/// next mount replays past the torn entry (never loud) and re-claims.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_torn_claim_entry_recovers_and_reclaims() {
    let _faults = FaultGuard;
    let vol = fresh_volume().await;

    // The claim is the volume's FIRST post-replay mutation by
    // construction, so its entry starts at the recovered ring head.
    let (journal_start, head_page, head_off) = {
        let probe = KvMetaBackend::open_probe(vol.path()).await.unwrap();
        let geo = *probe.journal_ring().core().geometry();
        let head = probe.journal_ring().core().head();
        (
            probe.superblock().journal.start,
            geo.page_index(head),
            geo.in_page_off(head),
        )
    };
    let claim_entry_phys = journal_start + head_page * 4096 + 24 + head_off;

    // Tear the claim entry a few bytes in: the write reports EIO and the
    // "device dies" (path poisoned) — the mount must fail loud.
    uring_fs::arm_torn_write(claim_entry_phys + 8, 4);
    let torn = KvMetaBackend::open(vol.path()).await;
    assert!(
        torn.is_err(),
        "a torn claim write is a failed mount (device error), never a silent arm"
    );
    uring_fs::clear_faults();

    // Remount: replay drops the torn entry sound-and-silent and the mount
    // re-claims.
    let be = KvMetaBackend::open(vol.path())
        .await
        .expect("remount after a torn claim entry must recover (torn-write immunity)");
    let claim = be.read_writer_claim().await.expect("re-claimed");
    assert_eq!(claim.pid, std::process::id());
    Metadata::create(be.as_ref(), 1, "post-tear", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("volume serves after recovery");
    be.shutdown().await.unwrap();
}

/// Panic-safe conveyor hold-seam arm/disarm (the FaultGuard precedent:
/// the seam is process-global, so a failing assertion between arm and
/// disarm must not leak a parked pass into the next test).
struct ConveyorSeamGuard;

impl ConveyorSeamGuard {
    fn arm(stage: u64) -> Self {
        TEST_CONVEYOR_HOLD_STAGE.store(stage, Ordering::SeqCst);
        ConveyorSeamGuard
    }
}

impl Drop for ConveyorSeamGuard {
    fn drop(&mut self) {
        TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
        test_conveyor_hold_release();
    }
}

/// The 2026-07-27 full-suite flake (P3 write-side-economy §7), pinned
/// deterministically: a mount whose claim COMMIT fails (torn claim entry
/// — the §4.1 device-error shape) has already spawned the conveyor pass
/// task, and the pass's next-iteration backend upgrade races the failed
/// `open`'s own `Arc` drop. When the pass wins and its worker thread is
/// descheduled inside the empty-drain tail (an OS-quantum event — why the
/// flake fired ~once per full-suite run and never isolated), it pins the
/// backend struct and its Layer A writer flock past `open`'s error
/// return. The volume carries NO readable claim (the torn entry never
/// replays), so the same-process teardown absorption cannot attribute the
/// holder and the instant remount refused Busy ("another squeezefs
/// process holds the writer lock") — on a volume nobody held.
///
/// Contract: a gate-refused `open` releases Layer A **before** returning,
/// so the remount can never race our own teardown. The
/// `TEST_CONVEYOR_HOLD_EMPTY_DRAIN_TAIL` seam turns the scheduler quantum
/// into a certainty by parking the pass tail while it holds the upgrade.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_failed_claim_commit_releases_flock_despite_pinned_pass_tail() {
    let _faults = FaultGuard;
    let seam = ConveyorSeamGuard::arm(TEST_CONVEYOR_HOLD_EMPTY_DRAIN_TAIL);
    let vol = fresh_volume().await;

    // The claim entry starts at the recovered ring head (the torn test's
    // geometry math, verbatim).
    let (journal_start, head_page, head_off) = {
        let probe = KvMetaBackend::open_probe(vol.path()).await.unwrap();
        let geo = *probe.journal_ring().core().geometry();
        let head = probe.journal_ring().core().head();
        (
            probe.superblock().journal.start,
            geo.page_index(head),
            geo.in_page_off(head),
        )
    };
    let claim_entry_phys = journal_start + head_page * 4096 + 24 + head_off;

    let parked_before = TEST_CONVEYOR_EMPTY_TAIL_PARKED.load(Ordering::SeqCst);
    uring_fs::arm_torn_write(claim_entry_phys + 8, 4);
    let torn = KvMetaBackend::open(vol.path()).await;
    assert!(
        torn.is_err(),
        "a torn claim write is a failed mount (device error), never a silent arm"
    );
    uring_fs::clear_faults();

    // Barrier: the failed open's pass task parks its empty-drain tail
    // HOLDING the backend upgrade — the teardown pin is now a fact, not
    // a race. (Bounded: if the pass lost the upgrade race there is no
    // pin and the remount below is trivially unpinned — the scenario
    // degenerates to the plain torn-claim test, never to a false red.)
    let pin_deadline = std::time::Instant::now() + Duration::from_secs(2);
    while TEST_CONVEYOR_EMPTY_TAIL_PARKED.load(Ordering::SeqCst) == parked_before
        && std::time::Instant::now() < pin_deadline
    {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    // The remount must succeed WHILE the pin is held: the failed open
    // released Layer A before returning, so our own dying teardown can
    // never masquerade as "another squeezefs process".
    let be = KvMetaBackend::open(vol.path()).await.expect(
        "remount after a failed claim commit must never be refused by our own \
         teardown-pinned writer flock (the 2026-07-27 full-suite flake)",
    );
    let claim = be.read_writer_claim().await.expect("re-claimed");
    assert_eq!(claim.pid, std::process::id());

    // Disarm BEFORE shutdown: the remount's own pass parks on the seam
    // after its claim batch, and a clean shutdown's claim-delete commit
    // would otherwise queue behind a parked leader forever.
    drop(seam);
    Metadata::create(be.as_ref(), 1, "post-pin", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("volume serves after the pinned-teardown remount");
    be.shutdown().await.unwrap();
}

/// M1 acceptance harness — the bounded root nvmet session (design §5.0
/// B1 pt 5 / OQ 4a): exercises the REAL passthru `ReservationClient`
/// against the namespace named by `SQUEEZEFS_M1_ROOT_DEV` (the baseline's
/// nvmet-loop recipe). Inert without the env var; run as root:
///
/// ```text
/// sudo env SQUEEZEFS_M1_ROOT_DEV=/dev/nvmeXnY \
///   <test-binary> --exact root_session_real_nvme_reservations --nocapture
/// ```
///
/// Prints the RESCAP probe (settles OQ 4a for the target under test) and,
/// when reservations are supported, validates register → acquire-WE →
/// report → release end to end through our ioctl encoding.
#[test]
fn root_session_real_nvme_reservations() {
    let Ok(dev) = std::env::var("SQUEEZEFS_M1_ROOT_DEV") else {
        return;
    };
    use squeezefs::meta_backend::reservation::{NvmeReservationClient, ReservationClient};
    let client = NvmeReservationClient::open(std::path::Path::new(&dev))
        .expect("SQUEEZEFS_M1_ROOT_DEV must be an NVMe namespace block device");
    let rescap = client.rescap().expect("Identify Namespace (RESCAP probe)");
    println!("[m1-root] {dev}: RESCAP = {rescap:#04x}");
    println!(
        "[m1-root] host identity: {:?}",
        client.host_identity().expect("host identity")
    );
    if rescap == 0 {
        println!(
            "[m1-root] target advertises NO reservation support — the guard degrades \
             to detection grade on this namespace (design §5.0 table); the in-tree \
             barrier-layer fence-injection tests carry the enforcement assertion"
        );
        return;
    }
    let key = 0x5155_4545_5A45_0001u64; // "QUEEZE" + 1
    client.register(key).expect("reservation register");

    // Optional preempt leg: `SQUEEZEFS_M1_ROOT_VICTIM_KEY=<hex>` names a
    // foreign registrant (a second controller association with a distinct
    // hostnqn/hostid) that already holds the WE reservation — our acquire
    // must CONFLICT, the report must name the victim, and the PREEMPT
    // action must transfer holdership (the §5.0 B1 pt 3 arbitration,
    // against real device state).
    if let Ok(victim_hex) = std::env::var("SQUEEZEFS_M1_ROOT_VICTIM_KEY") {
        let victim = u64::from_str_radix(victim_hex.trim_start_matches("0x"), 16)
            .expect("SQUEEZEFS_M1_ROOT_VICTIM_KEY is hex");
        let conflict = client
            .acquire_write_exclusive(key)
            .expect_err("acquire against a foreign holder must conflict");
        assert!(
            squeezefs::meta_backend::reservation::is_reservation_conflict(&conflict),
            "conflict maps to the reservation-conflict errno class, got {conflict:?}"
        );
        let rep = client.report().expect("pre-preempt report");
        println!("[m1-root] pre-preempt report: {rep:?}");
        assert_eq!(rep.holder_key, Some(victim), "report names the victim");
        client.preempt(key, victim).expect("reservation preempt");
        let rep = client.report().expect("post-preempt report");
        println!("[m1-root] post-preempt report: {rep:?}");
        assert_eq!(rep.holder_key, Some(key), "preempt transferred the WE");
        assert!(
            !rep.registered(victim),
            "preempt unregistered the victim key"
        );
        println!("[m1-root] conflict/report/preempt validated against {dev}");
        // Hold (do NOT release): the session script fence-checks the
        // preempted association's writes before tearing down.
        return;
    }

    client
        .acquire_write_exclusive(key)
        .expect("reservation acquire (Write Exclusive)");
    let rep = client.report().expect("reservation report");
    println!("[m1-root] post-acquire report: {rep:?}");
    assert_eq!(rep.holder_key, Some(key), "we hold the WE reservation");
    assert!(rep.registered(key));
    println!(
        "[m1-root] wire host id: {:02x?}",
        client.wire_host_id().expect("Get Features Host Identifier")
    );
    client.release(key).expect("reservation release");
    let rep = client.report().expect("post-release report");
    println!("[m1-root] post-release report: {rep:?}");
    assert_eq!(rep.holder_key, None, "release cleared the reservation");
    println!("[m1-root] register/acquire/report/release validated against {dev}");
}

/// Child branch for the two-process case: attempt a second write-mount of
/// the volume named by `SQUEEZEFS_GUARD_VOL`. Exit code 3 = refused (the
/// guard message is written to `SQUEEZEFS_GUARD_OUT`); exit 0 = mounted
/// (the incident-4 failure mode).
#[test]
fn guard_child_second_open() {
    let Ok(vol) = std::env::var("SQUEEZEFS_GUARD_VOL") else {
        return;
    };
    let out = std::env::var("SQUEEZEFS_GUARD_OUT").expect("SQUEEZEFS_GUARD_OUT");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let res = rt.block_on(async { KvMetaBackend::open(std::path::Path::new(&vol)).await });
    match res {
        Ok(_) => std::process::exit(0),
        Err(e) => {
            std::fs::write(&out, e.to_string()).expect("write refusal message");
            std::process::exit(3);
        }
    }
}

/// Incident 4's shape, end to end: a second DAEMON PROCESS attempting to
/// mount the held volume exits nonzero with the guard message while the
/// first daemon's storm is unaffected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_two_process_second_daemon_refused() {
    let vol = fresh_volume().await;
    let first = KvMetaBackend::open(vol.path()).await.expect("first daemon");

    let dir = tempdir().unwrap();
    let out = dir.path().join("refusal.txt");
    let exe = std::env::current_exe().expect("test binary path");
    let status = Command::new(&exe)
        .args([
            "--exact",
            "guard_child_second_open",
            "--test-threads=1",
            "--nocapture",
        ])
        .env("SQUEEZEFS_GUARD_VOL", vol.path())
        .env("SQUEEZEFS_GUARD_OUT", &out)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn second daemon");

    assert_eq!(
        status.code(),
        Some(3),
        "the second daemon must exit nonzero, refused by the guard \
         (exit 0 = it mounted = incident 4)"
    );
    let msg = std::fs::read_to_string(&out).expect("refusal message file");
    assert!(
        msg.contains("single-writer") || msg.contains("writer lock"),
        "the second daemon's error names the guard: {msg}"
    );

    // First daemon's storm is unaffected.
    for i in 0..8 {
        Metadata::create(
            first.as_ref(),
            1,
            &format!("storm{i}"),
            libc::S_IFREG | 0o644,
            0,
            0,
        )
        .await
        .expect("holder's storm proceeds through the refused attempt");
    }
    first.shutdown().await.unwrap();
}
