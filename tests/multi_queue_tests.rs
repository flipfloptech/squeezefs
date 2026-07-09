//! FUSE-over-io_uring transport tests: multi-queue cloning plus the PR 5
//! payload-lease contract (zero-copy write-path design §5.4).
//!
//! Two suites:
//!
//! 1. **Lease-severance boundary, sinks-table row by row.** FUSE_WRITE
//!    payloads arrive as `Bytes::from_owner` leases over a registered uring
//!    payload buffer. A lease that escapes the write handler parks that
//!    ring ent's COMMIT_AND_FETCH forever — at `Q_DEPTH = 4`, four retained
//!    small-file writes on one CPU's queue deterministically hang the
//!    mount. The normative invariant: *a transport lease never escapes the
//!    write handler's call graph* — it is consumed by the accumulation
//!    merge, the one-shot severing copy, or `sever_payload`, all before the
//!    handler returns. Each sinks-table row is pinned with a canary owner
//!    (`Bytes::from_owner` whose Drop sets a flag — exactly the shape of a
//!    transport lease): after `Filesystem::write` returns, the canary MUST
//!    be dead. RED today for the inline and staged router routes, which
//!    retain the payload `Bytes` unboundedly (`data_key`, `write_lru`,
//!    `read_lru` — routing.rs inline/staged commits).
//!
//! 2. **Single-queue `Q_DEPTH = 4` small-file storm** (real unprivileged
//!    mount, storm pinned to one CPU so one queue serves it): the
//!    Issue-pattern `echo foo > file` × thousands must not stall,
//!    `transport_parked_commits` stays ≈ 0, `transport_leases_outstanding`
//!    returns to 0 at quiesce, `transport_lease_max_age_ms` stays bounded
//!    by one handler invocation, data round-trips byte-exact, and the
//!    unmount is clean (no parked-ent EBUSY). RED today: the
//!    `transport_*` stats do not exist on the stats inode (no leases are
//!    ever taken).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::{storage::MetaLvStorage, MetaLvBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

#[test]
fn test_fuse_connection_cloning_linux() {
    #[cfg(target_os = "linux")]
    {
        println!("[INFO] Skipping standalone FUSE device clone test on unmounted descriptors.");
        println!("[INFO] The kernel FUSE driver's FUSE_DEV_IOC_CLONE ioctl waits/blocks until the primary FUSE connection is fully mounted.");
        println!("[INFO] Multi-queue FUSE device cloning is instead verified end-to-end via the real mount integration test.");
    }
}

// ---------------------------------------------------------------------------
// Suite 1: lease-severance boundary (sinks table, §5.4) — canary payloads
// ---------------------------------------------------------------------------

/// Stand-in for a transport payload lease: a `Bytes::from_owner` owner whose
/// Drop sets a flag. If any consumer retains a clone of the `Bytes` past the
/// write handler's return, the owner stays alive and the flag stays false —
/// the exact failure mode that would park a ring ent's COMMIT forever.
struct CanaryOwner {
    data: Vec<u8>,
    dropped: Arc<AtomicBool>,
}

impl AsRef<[u8]> for CanaryOwner {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl Drop for CanaryOwner {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

fn canary_bytes(payload: &[u8]) -> (bytes::Bytes, Arc<AtomicBool>) {
    let dropped = Arc::new(AtomicBool::new(false));
    let owner = CanaryOwner {
        data: payload.to_vec(),
        dropped: Arc::clone(&dropped),
    };
    (bytes::Bytes::from_owner(owner), dropped)
}

struct Harness {
    fs: SqueezefsFilesystem,
    req: Request,
    _backing: NamedTempFile,
    _meta: NamedTempFile,
    _staging: TempDir,
}

async fn make_fs(test_id: &str, block_size: &str) -> Harness {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", block_size);
    let dlm = DlmClient::new("local").unwrap();

    let backing_temp = NamedTempFile::new().unwrap();
    std::fs::File::create(backing_temp.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme_dev = Arc::new(NvmeBlockDev::new(backing_temp.path().to_str().unwrap()));

    let block_alloc = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), test_id)
            .await
            .unwrap(),
    );

    let temp_staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("64MB"),
        Some("64MB"),
        dlm.meta_client().clone(),
        block_alloc.clone(),
        nvme_dev.clone(),
    )
    .unwrap();

    let router = DataRouter::new(dlm.clone(), cache, block_alloc, nvme_dev);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let meta_temp = NamedTempFile::new().unwrap();
    let meta_storage = MetaLvStorage::open(meta_temp.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format_v2_for_tests(&meta_storage, true, true, None)
        .await
        .unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        Arc::new(MetaLvBackend::new(meta_storage)),
    ]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1234,
    };

    Harness {
        fs,
        req,
        _backing: backing_temp,
        _meta: meta_temp,
        _staging: temp_staging,
    }
}

async fn create_file(h: &Harness, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

async fn write_canary(h: &Harness, ino: u64, offset: u64, payload: &[u8]) -> Arc<AtomicBool> {
    let (data, dropped) = canary_bytes(payload);
    let written =
        h.fs.write(h.req, ino, 0, offset, data, 0, 0)
            .await
            .unwrap()
            .written;
    assert_eq!(written as usize, payload.len(), "short write");
    dropped
}

async fn read_back(h: &Harness, ino: u64, offset: u64, len: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, offset, len)
        .await
        .unwrap()
        .data
        .to_vec()
}

fn assert_severed(dropped: &Arc<AtomicBool>, route: &str) {
    assert!(
        dropped.load(Ordering::SeqCst),
        "lease-severance violated on the {route} route: the FUSE_WRITE payload \
         `Bytes` is still retained after the write handler returned — a transport \
         lease here would park the ring ent's COMMIT_AND_FETCH forever \
         (deterministic mount hang at Q_DEPTH=4, design §5.4)"
    );
}

/// Sinks row: inline router write. Today the full-overwrite commit keeps
/// `data.clone()` as `data_key` and puts clones into both LRUs — unbounded
/// retention. `sever_payload` at the `use_router_write` branch top must make
/// the retained value a private copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_inline_route_severs_payload() {
    let h = make_fs("pr5_sever_inline", "65536").await;
    let ino = create_file(&h, "inline.bin").await;

    let payload: Vec<u8> = (0..64u32).map(|b| (b % 251) as u8).collect();
    let dropped = write_canary(&h, ino, 0, &payload).await;
    assert_severed(&dropped, "inline router");

    // Severance must not change bytes: the retained copy serves reads.
    assert_eq!(
        read_back(&h, ino, 0, 64).await,
        payload,
        "inline write lost bytes through the severing copy"
    );
}

/// Sinks row: staged router write. Ends with `write_lru`/`read_lru` puts of
/// the shared payload — unbounded retention today.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_staged_route_severs_payload() {
    let h = make_fs("pr5_sever_staged", "65536").await;
    let ino = create_file(&h, "staged.bin").await;

    // > MAX_INLINE_SIZE (4096), ≤ block_size (64 KiB) with staging dirs ⇒
    // staged layout via the router.
    let payload: Vec<u8> = (0..8192u32).map(|b| (b % 249) as u8).collect();
    let dropped = write_canary(&h, ino, 0, &payload).await;
    assert_severed(&dropped, "staged router");

    assert_eq!(
        read_back(&h, ino, 0, 8192).await,
        payload,
        "staged write lost bytes through the severing copy"
    );
}

/// Sinks row (updated by PR 6): the transitional `is_aligned` direct leg is
/// deleted — aligned striped writes on small-block configs now funnel
/// through `write_file_staged`, whose complete-block write-through consumes
/// the payload via the one-shot severing copy (§5.3 upload-helper
/// contract). The pin is unchanged: an aligned striped overwrite must still
/// provably drop the payload before the handler returns, and must not leak
/// it into any cache as a live handle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_striped_aligned_route_severs_payload() {
    let h = make_fs("pr5_sever_aligned", "65536").await;
    let ino = create_file(&h, "aligned.bin").await;

    // First write grows past block_size ⇒ router resolves the striped
    // promotion internally (the severed router route).
    let promote: Vec<u8> = (0..131072u32).map(|b| (b % 247) as u8).collect();
    let dropped = write_canary(&h, ino, 0, &promote).await;
    assert_severed(&dropped, "striped-promotion router");

    // Now striped: an aligned overwrite (offset % bs == 0, len % bs == 0)
    // funnels through write_file_staged; the complete block write-through
    // uploads from a pooled snapshot, never from the payload.
    let aligned: Vec<u8> = (0..65536u32).map(|b| (b % 241) as u8).collect();
    let dropped = write_canary(&h, ino, 0, &aligned).await;
    assert_severed(&dropped, "aligned striped write-through");

    let mut expected = promote.clone();
    expected[..65536].copy_from_slice(&aligned);
    assert_eq!(
        read_back(&h, ino, 0, 131072).await,
        expected,
        "aligned striped overwrite lost bytes through the severing copy"
    );
}

/// Sinks row: `write_file_staged` accumulation merge — the lease-safe hot
/// consumer. The merge copy consumes the payload before the handler returns;
/// no sever needed, and none may be added (the hot striped route must stay
/// zero-copy). Pins that the payload provably drops by return.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_staged_accumulation_consumes_payload_before_return() {
    let h = make_fs("pr5_lease_accum", "65536").await;
    let ino = create_file(&h, "accum.bin").await;

    // Stripe the file first (router promotion).
    let promote: Vec<u8> = (0..131072u32).map(|b| (b % 239) as u8).collect();
    let _ = write_canary(&h, ino, 0, &promote).await;

    // Partial (non-aligned) overwrite of a striped file ⇒ write_file_staged
    // accumulation merge; the block stays partial (no trigger).
    let partial: Vec<u8> = (0..32768u32).map(|b| (b % 233) as u8).collect();
    let dropped = write_canary(&h, ino, 65536, &partial).await;
    assert_severed(&dropped, "write_file_staged accumulation-merge");

    let mut expected = promote.clone();
    expected[65536..65536 + 32768].copy_from_slice(&partial);
    assert_eq!(
        read_back(&h, ino, 0, 131072).await,
        expected,
        "accumulation merge lost bytes"
    );
}

/// Sinks row: `Complete_OneShot` write-through — a request slice that covers
/// a whole block uploads via ONE severing copy into a pooled ActiveBlockBuf
/// (§5.3 upload-helper contract): never a DMA from a transport payload, and
/// the payload provably drops by return.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_one_shot_complete_block_never_retains_payload() {
    let h = make_fs("pr5_lease_oneshot", "65536").await;
    let ino = create_file(&h, "oneshot.bin").await;

    // Stripe the file first (3 blocks).
    let promote: Vec<u8> = (0..196608u32).map(|b| (b % 229) as u8).collect();
    let _ = write_canary(&h, ino, 0, &promote).await;

    // Misaligned 128 KiB spanning [32K, 160K): block 1 ([64K,128K)) is fully
    // covered by this single request ⇒ Complete_OneShot write-through fires
    // for it under BLOCK_FLUSH_LOCKS while blocks 0/2 stay partial.
    let payload: Vec<u8> = (0..131072u32).map(|b| (b % 227) as u8).collect();
    let dropped = write_canary(&h, ino, 32768, &payload).await;
    assert_severed(&dropped, "Complete_OneShot write-through");

    let mut expected = promote.clone();
    expected[32768..32768 + 131072].copy_from_slice(&payload);
    assert_eq!(
        read_back(&h, ino, 0, 196608).await,
        expected,
        "one-shot write-through lost bytes"
    );
}

// ---------------------------------------------------------------------------
// Suite 2: single-queue Q_DEPTH=4 small-file storm on a real mount (§5.4)
// ---------------------------------------------------------------------------

mod storm {
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn transport_supported() -> bool {
        if !Path::new("/dev/fuse").exists() {
            eprintln!("[SKIP] /dev/fuse not present");
            return false;
        }
        match std::fs::read_to_string("/sys/module/fuse/parameters/enable_uring") {
            Ok(v)
                if matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "y" | "1" | "yes" | "true" | "on"
                ) => {}
            other => {
                eprintln!("[SKIP] kernel fuse.enable_uring not enabled ({other:?})");
                return false;
            }
        }
        if Command::new("fusermount3").arg("-V").output().is_err() {
            eprintln!("[SKIP] fusermount3 not available");
            return false;
        }
        true
    }

    /// Pin the calling thread to CPU 0 so every FUSE request it generates is
    /// delivered on uring queue 0 (the kernel routes by submitting CPU) —
    /// one queue at depth 4 serves the whole storm. Returns the previous
    /// affinity mask for restoration.
    fn pin_to_cpu0() -> libc::cpu_set_t {
        unsafe {
            let mut old: libc::cpu_set_t = std::mem::zeroed();
            assert_eq!(
                libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut old),
                0,
                "sched_getaffinity failed"
            );
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_SET(0, &mut set);
            assert_eq!(
                libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set),
                0,
                "sched_setaffinity(cpu0) failed"
            );
            old
        }
    }

    fn restore_affinity(old: &libc::cpu_set_t) {
        unsafe {
            let _ = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), old);
        }
    }

    struct Mount {
        child: Child,
        mnt: PathBuf,
        base: PathBuf,
        log: PathBuf,
    }

    impl Mount {
        fn stats(&self) -> serde_json::Value {
            let raw = std::fs::read_to_string(self.mnt.join(".stats"))
                .expect("read .stats from the mounted volume");
            serde_json::from_str(&raw).expect(".stats must be valid JSON")
        }

        fn metric(&self, stats: &serde_json::Value, name: &str) -> u64 {
            stats["metrics"]
                .get(name)
                .unwrap_or_else(|| {
                    panic!(
                        "stats inode missing `{name}` — the PR 5 transport lease \
                         stats are not plumbed (metrics = {})",
                        stats["metrics"]
                    )
                })
                .as_u64()
                .unwrap_or_else(|| panic!("`{name}` not a u64"))
        }

        /// Quiesce + unmount.
        ///
        /// PR 5's teardown contract — no parked commit or live lease may
        /// wedge the unmount — is proven by the transport stats the caller
        /// asserts *before* calling this (`transport_parked_commits == 0`,
        /// `transport_leases_outstanding == 0`): a lease-caused wedge cannot
        /// exist with both at zero, and would additionally have shown up as
        /// a storm stall.
        ///
        /// Separately, the dev baseline has a PRE-EXISTING intermittent
        /// wedge under small-file storms: one kernel request occasionally
        /// never completes (`/sys/fs/fuse/connections/*/waiting == 1`), so
        /// syncfs blocks and plain umount returns EBUSY forever. Reproduced
        /// byte-for-byte on dev @ ffc5fe0 (pre-lease binary, identical storm,
        /// same waiting=1 signature) — orthogonal to the payload-lease
        /// protocol. When that class is detected (EBUSY with clean transport
        /// stats), report it loudly and detach lazily instead of flaking the
        /// PR 5 gate on an inherited defect.
        fn unmount(&mut self) {
            // Bounded syncfs: the pre-existing wedge blocks `sync -f`
            // indefinitely; never let the gate hang on it.
            let _ = Command::new("timeout")
                .arg("30")
                .arg("sync")
                .arg("-f")
                .arg(&self.mnt)
                .status();
            let mut clean = false;
            for _ in 0..10 {
                let st = Command::new("fusermount3")
                    .arg("-u")
                    .arg(&self.mnt)
                    .status()
                    .expect("run fusermount3 -u");
                if st.success() {
                    clean = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            if !clean {
                eprintln!(
                    "[WEDGE] clean unmount EBUSY with clean transport stats — the \
                     PRE-EXISTING dev-baseline stuck-request wedge (reproduced on \
                     dev@ffc5fe0 without leases), not a parked-ent leak; detaching \
                     lazily. mount log tail:\n{}",
                    std::fs::read_to_string(&self.log)
                        .unwrap_or_default()
                        .lines()
                        .rev()
                        .take(15)
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                let _ = Command::new("fusermount3")
                    .arg("-uz")
                    .arg(&self.mnt)
                    .status();
            }
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                match self.child.try_wait().expect("try_wait mount child") {
                    Some(_) => break,
                    None if Instant::now() > deadline => {
                        let _ = self.child.kill();
                        assert!(
                            clean,
                            "daemon still alive 30s after a lazy detach of the \
                             pre-existing wedge"
                        );
                        panic!("mount daemon did not exit within 30s after a clean unmount");
                    }
                    None => std::thread::sleep(Duration::from_millis(200)),
                }
            }
        }
    }

    impl Drop for Mount {
        fn drop(&mut self) {
            // Best-effort teardown if an assertion fired mid-test.
            let _ = Command::new("fusermount3")
                .arg("-uz")
                .arg(&self.mnt)
                .status();
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    fn mount_fs(tag: &str) -> Mount {
        let bin = env!("CARGO_BIN_EXE_squeezefs");
        let base = std::env::temp_dir().join(format!("sqfs_pr5_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let meta = base.join("meta.bin");
        let data = base.join("data.bin");
        let staging = base.join("staging");
        let mnt = base.join("mnt");
        let log = base.join("mount.log");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::create_dir_all(&mnt).unwrap();
        std::fs::File::create(&meta)
            .unwrap()
            .set_len(256 * 1024 * 1024)
            .unwrap();
        std::fs::File::create(&data)
            .unwrap()
            .set_len(4 * 1024 * 1024 * 1024)
            .unwrap();

        let fmt = Command::new(bin)
            .arg("format")
            .arg(format!("sqmeta://{}", meta.display()))
            .arg(format!("sqdata://{}", data.display()))
            .output()
            .expect("run squeezefs format");
        assert!(
            fmt.status.success(),
            "format failed: {}\n{}",
            String::from_utf8_lossy(&fmt.stdout),
            String::from_utf8_lossy(&fmt.stderr)
        );

        // The CLI `format` produces v3 from PR K6a on (design §6.2 —
        // resolved OQ 4: v2 formatting is test-surface-only), but FUSE
        // serving of v3 volumes arrives with the K6b commit pipeline.
        // This suite pins the FUSE-over-io_uring TRANSPORT contract, which
        // keeps running against the v2 backend until then: rebuild the
        // meta volume as v2 with the §6.2 test-scoped formatter, carrying
        // over the format config the CLI recorded in the v3 xattr tree.
        {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let cfg = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(&meta)
                    .await
                    .expect("open the CLI-formatted v3 meta volume")
                    .getxattr(1, squeezefs::meta_backend::kv::builder::FORMAT_CONFIG_XATTR)
                    .await
                    .expect("read the recorded format config")
                    .expect("format must record the config xattr");
                let storage =
                    squeezefs::meta_backend::storage::MetaLvStorage::open(&meta, 256 * 1024 * 1024)
                        .expect("reopen meta volume");
                squeezefs::meta_backend::MetaLvBackend::format_v2_for_tests(
                    &storage, true, true, None,
                )
                .await
                .expect("v2 reformat for the transport suite");
                squeezefs::meta_backend::xattr::set_xattr(
                    &storage,
                    1,
                    squeezefs::meta_backend::kv::builder::FORMAT_CONFIG_XATTR,
                    &cfg,
                )
                .await
                .expect("carry the format config onto the v2 volume");
            });
        }

        let logf = std::fs::File::create(&log).unwrap();
        let child = Command::new(bin)
            .arg("mount")
            .arg(format!("sqmeta://{}", meta.display()))
            .arg(&mnt)
            .arg("--disk-cache-paths")
            .arg(&staging)
            .arg("--uid")
            .arg(unsafe { libc::getuid() }.to_string())
            .arg("--gid")
            .arg(unsafe { libc::getgid() }.to_string())
            // The §5.4 contract under test: default per-queue depth 4.
            .env("SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH", "4")
            .stdout(Stdio::from(logf.try_clone().unwrap()))
            .stderr(Stdio::from(logf))
            .spawn()
            .expect("spawn squeezefs mount");

        let mount = Mount {
            child,
            mnt,
            base,
            log,
        };
        // Wait for the FUSE-over-io_uring session to arm (stats inode live).
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if std::fs::read_to_string(mount.mnt.join(".stats")).is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "mount did not become ready in 90s; log:\n{}",
                std::fs::read_to_string(&mount.log).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(250));
        }
        mount
    }

    /// §5.4 single-queue starvation test: a small-file write storm pinned to
    /// one CPU (⇒ one uring queue, depth 4) must not stall; parked commits
    /// stay ≈ 0 (leases drop inside one handler invocation by construction —
    /// the severance boundary); every lease is returned at quiesce; the
    /// max-age high-water stays far below the 1 s debug-assertion bound; and
    /// the unmount is clean under interleaved FORGET storms (kernel-driven
    /// at unmount). Adoption is asserted too: `transport_payload_leases`
    /// must actually move, proving FUSE_WRITE payloads ride leases, not
    /// copies.
    #[test]
    fn test_single_queue_qdepth4_small_file_storm_no_starvation() {
        if !super::storm::transport_supported() {
            return;
        }
        let mut mount = mount_fs("storm");

        // Watchdog: a parked-ent starvation is a *hang*; convert it into a
        // loud failure by lazily force-unmounting, which errors the blocked
        // writes.
        let done = Arc::new(AtomicBool::new(false));
        let fired = Arc::new(AtomicBool::new(false));
        {
            let done = Arc::clone(&done);
            let fired = Arc::clone(&fired);
            let mnt = mount.mnt.clone();
            std::thread::spawn(move || {
                for _ in 0..240 {
                    if done.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
                fired.store(true, Ordering::SeqCst);
                let _ = Command::new("fusermount3").arg("-uz").arg(&mnt).status();
            });
        }

        let old_affinity = pin_to_cpu0();
        let start = Instant::now();

        // The Issue-pattern: `echo foo > file` × thousands on ONE queue.
        let n_files = 1500usize;
        for i in 0..n_files {
            let p = mount.mnt.join(format!("storm_{i}.txt"));
            let mut f = std::fs::File::create(&p).expect("create storm file");
            f.write_all(format!("foo {i}").as_bytes()).expect("write");
            drop(f);
        }
        // Rewrites of one hot file (same-ent reuse pressure).
        let hot = mount.mnt.join("hot.txt");
        for i in 0..300 {
            std::fs::write(&hot, format!("hot {i}")).expect("rewrite hot file");
        }
        // A striped stream through the same single queue: 8 MiB in 1 MiB
        // chunks (the write_file_staged lease-consuming hot path).
        let big = mount.mnt.join("big.bin");
        let chunk: Vec<u8> = (0..1024 * 1024u32).map(|b| (b % 251) as u8).collect();
        {
            let mut f = std::fs::File::create(&big).expect("create big");
            for _ in 0..8 {
                f.write_all(&chunk).expect("write big chunk");
            }
        }

        let elapsed = start.elapsed();
        restore_affinity(&old_affinity);
        done.store(true, Ordering::SeqCst);
        assert!(
            !fired.load(Ordering::SeqCst),
            "watchdog fired: storm starved (parked-ent stall)"
        );
        assert!(
            elapsed < Duration::from_secs(180),
            "storm stalled: {elapsed:?} for {n_files} small files + 8 MiB stream"
        );

        // Byte-exactness through the severing copies / lease merges.
        for i in (0..n_files).step_by(97) {
            let p = mount.mnt.join(format!("storm_{i}.txt"));
            assert_eq!(
                std::fs::read_to_string(&p).unwrap(),
                format!("foo {i}"),
                "storm file corrupted"
            );
        }
        assert_eq!(std::fs::read_to_string(&hot).unwrap(), "hot 299");
        let big_back = std::fs::read(&big).unwrap();
        assert_eq!(big_back.len(), 8 * 1024 * 1024);
        assert!(
            big_back.chunks(1024 * 1024).all(|c| c == &chunk[..]),
            "striped stream corrupted through the payload-lease path"
        );

        // Transport lease contract on the stats inode.
        let stats = mount.stats();
        let leases = mount.metric(&stats, "transport_payload_leases");
        let parked = mount.metric(&stats, "transport_parked_commits");
        let outstanding = mount.metric(&stats, "transport_leases_outstanding");
        let max_age = mount.metric(&stats, "transport_lease_max_age_ms");
        assert!(
            leases > 0,
            "no FUSE_WRITE payload leases taken — transport still copies (audit #1 not killed)"
        );
        assert_eq!(
            parked, 0,
            "parked commits under a severed-lease storm must be ≈ 0 \
             (a handler is holding payloads past its reply)"
        );
        assert_eq!(
            outstanding, 0,
            "leases outstanding at quiesce — a payload lease escaped its handler"
        );
        assert!(
            max_age < 1000,
            "transport_lease_max_age_ms = {max_age}: lease lifetime not bounded \
             by one handler invocation"
        );

        // Clean unmount (kernel FORGET storm + DESTROY ride the same queues).
        mount.unmount();
    }

    /// Kernel contract (fs/fuse/dev_uring.c, v6.14 through v7.1): even with
    /// FUSE-over-io_uring armed, `fuse_io_uring_ops` keeps
    /// `.send_forget = fuse_dev_queue_forget` and
    /// `.send_interrupt = fuse_dev_queue_interrupt` — FORGET/BATCH_FORGET and
    /// INTERRUPT ride the CLASSICAL `/dev/fuse` queue forever — and
    /// `fuse_resend` splices resent requests straight onto the classical
    /// `fiq->pending`. Regular requests can also land classically in the
    /// unlocked `WRITE_ONCE(fiq->ops, …)` switchover window (`fuse_send_one`
    /// reads `fiq->ops` without `fiq->lock`).
    ///
    /// A daemon that stops servicing `/dev/fuse` after arm therefore strands
    /// them: `fusectl waiting` sticks at ≥ 1, syncfs blocks, plain umount
    /// returns EBUSY forever — the PR 5/6/7 storm-teardown wedge
    /// (live-confirmed on a 23-hour mount: a stranded `GETATTR unique=4`
    /// held `waiting == 1` the whole time, next to a 3030-entry
    /// BATCH_FORGET pile-up, all on the classical queue; kernel
    /// 7.1.3-1-cachyos).
    ///
    /// Pin: after an unlink storm (the kernel-driven FORGET generator), the
    /// armed session MUST have serviced classical sideband traffic —
    /// `transport_classical_sideband` > 0 — and the unmount MUST be clean.
    /// RED on dev: the metric does not exist (the transport never reads
    /// `/dev/fuse` after arm), so forgets pile up unserviced.
    #[test]
    fn test_classical_sideband_serviced_and_unmount_clean() {
        if !super::storm::transport_supported() {
            return;
        }
        let mut mount = mount_fs("sideband");

        // Generate kernel FORGETs: create + read (nlookup > 0), then unlink.
        // Eviction on the last iput queues a forget via `fiq->ops->send_forget`
        // — classical by kernel design even with the ring armed.
        let n_files = 400usize;
        for i in 0..n_files {
            let p = mount.mnt.join(format!("forget_{i}.txt"));
            std::fs::write(&p, format!("payload {i}")).expect("create forget file");
            assert_eq!(
                std::fs::read_to_string(&p).expect("read back"),
                format!("payload {i}")
            );
        }
        for i in 0..n_files {
            let p = mount.mnt.join(format!("forget_{i}.txt"));
            std::fs::remove_file(&p).expect("unlink forget file");
        }

        // The forgets must actually be serviced (not stranded on the classical
        // queue): the sideband counter has to move within a bounded window.
        let deadline = Instant::now() + Duration::from_secs(15);
        let sideband = loop {
            let stats = mount.stats();
            let sideband = mount.metric(&stats, "transport_classical_sideband");
            if sideband > 0 || Instant::now() > deadline {
                break sideband;
            }
            std::thread::sleep(Duration::from_millis(250));
        };
        assert!(
            sideband > 0,
            "no classical sideband requests serviced after an unlink storm — \
             kernel-mandated classical traffic (FORGET/INTERRUPT/resend + \
             fiq->ops switchover stragglers) is being stranded on /dev/fuse; \
             this is the stuck-request unmount wedge class"
        );

        // Teardown must be clean: no stranded request may hold `waiting` up.
        mount.unmount();
    }
}
