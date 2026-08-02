//! DLM-1 / spec §6.11 — fencing tokens across remount (the P1 item to fix
//! independent of the DLM program) plus the in-RAM monotonicity pin.
//!
//! The remount law says "stale fencing tokens discard staged work". Spec
//! §6.11 (docs/pre-rc-engineering-spec.md) proves the law CANNOT fire at
//! mount time today: fencing tokens are in-RAM only and restart at 0 in
//! every process, so the recovery check `rec.fencing_token < current`
//! compares against 0 and adopts every pre-crash record — including
//! records from a writer era that was SUPERSEDED before the crash.
//! `extent_records_stale_discarded` is structurally near zero; what
//! actually protects the path is the staging generation stamp.
//!
//! Two contracts here:
//!
//! 1. **The remount contract (GREEN at S2)**: a real process-death
//!    (child process stages records from two fencing eras for one ino,
//!    then `exit()`s without drain) followed by a remount in a FRESH
//!    process must discard the superseded era's record and keep the
//!    newest era's record. Red pre-S2: the fresh process read
//!    `current == 0`, `T1 < 0` was false, and BOTH records were adopted.
//!    DLM stage **S2** (durable `WriterClaim.term` + composed tokens,
//!    incompat bit 7 — spec §6.9) is the green-maker: the remount's term
//!    bump makes every pre-crash stamp a FOREIGN era, and the sweep
//!    classifies a foreign era's records against the newest surviving
//!    stamp FOR THAT INO (per-ino currency — a blanket "discard every
//!    pre-crash record" would fail the `recovered == 1` arm below, which
//!    is exactly the crash residue the W2 recovery contract adopts).
//!
//! 2. **The in-RAM monotonicity pin (non-ignored)**: the SAME two-era
//!    shape inside one process must classify correctly — the superseded
//!    record discarded, the current-era record recovered. This is what
//!    keeps S1 (single global `grant_seq`) honest: whatever replaces the
//!    per-object `FENCING_MAP`, the ino's readable generation must stay
//!    monotone ≥ its own last grant (or the discard arm dies) and must
//!    NOT exceed the newest stamp when nothing newer was minted (or the
//!    recover arm dies).
//!
//! The crash child is spawned via `current_exe()` (the
//! mount_writer_guard/crash_kill precedent): process-global DLM statics
//! genuinely die with the child, which an in-process "session drop" can
//! never simulate.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::nvme::{ExtentRecord, EXTENT_RECORD_VERSION};
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::tempdir;

const BS: u64 = 65536;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn reset_knobs() {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
}

struct H {
    fs: SqueezefsFilesystem,
    dlm: DlmClient,
    req: Request,
}

/// A session over PERSISTENT meta + backing + staging. In the crash test
/// the FIRST session lives in a child process, so all process-global DLM
/// state (the fencing generators) genuinely restarts at zero for the
/// second session — the real remount shape §6.11 describes.
async fn session(meta_path: &Path, backing_path: &Path, staging: &Path) -> H {
    reset_knobs();
    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing_path.to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new("fencing_remount_ns").await.unwrap());
    let cache = TieredCache::new(
        vec![staging.to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba.clone(), nvme);
    router.set_crypto(squeezefs::crypto_compress::CryptoCompressState::new(
        "lz4".to_string(),
        "none".to_string(),
        None,
    ));
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let be = KvMetaBackend::open(meta_path).await.unwrap();
    let routed = Arc::new(RoutedMetaBackend::new(vec![be]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    for kv in &routed.volumes {
        ba.recover_active_blocks_v3(kv, &fs.router.backend_router)
            .await
            .expect("v3 refcount recovery");
    }
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    };
    H { fs, dlm, req }
}

/// Format a fresh v3 meta image at `meta_path` (parent-side; the crash
/// child only ever OPENS it, exactly like a remount).
async fn format_meta(meta_path: &Path) {
    std::fs::File::create(meta_path)
        .unwrap()
        .set_len(128 * 1024 * 1024)
        .unwrap();
    ImageBuilder::new(BuilderConfig {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        hash_seed: 0xC0FF_EE00_6116_0611,
        uuid: *b"fencing-remount1",
    })
    .unwrap()
    .build(meta_path, 128 * 1024 * 1024)
    .await
    .unwrap();
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i % 249) as u8 ^ tag | 1).collect()
}

async fn create(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

async fn write_at(h: &H, ino: u64, off: u64, data: &[u8]) {
    let w =
        h.fs.write(
            h.req,
            ino,
            0,
            off,
            bytes::Bytes::copy_from_slice(data),
            0,
            0,
        )
        .await
        .unwrap();
    assert_eq!(w.written as usize, data.len(), "short write at {off}");
}

async fn fsync(h: &H, ino: u64) {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
}

/// A durable STRIPED file of `blocks` × BS (the extent-record host shape).
async fn durable_striped(h: &H, name: &str, blocks: u64, tag: u8) -> u64 {
    let len = (blocks * BS) as usize;
    let ino = create(h, name).await;
    write_at(h, ino, 0, &pattern(len, tag)).await;
    fsync(h, ino).await;
    let path = squeezefs::keys::inode_path(ino);
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(m.file_type, "striped", "fixture must be STRIPED");
    ino
}

/// Mint one fencing generation for `ino` via a real lease acquisition (a
/// byte-range lock shares the file's fencing generator without contending
/// with the write path's cached whole-file lease) and return the token.
async fn mint_era(dlm: &DlmClient, ino: u64) -> u64 {
    let path = squeezefs::keys::inode_path(ino);
    let lease = dlm
        .acquire_lock(&path, Some((0, 1)), std::time::Duration::from_secs(5))
        .await
        .expect("era mint acquire");
    let token = lease.fencing_token();
    lease.release().await.expect("era mint release");
    token
}

/// Plant a staged `active_block_ext:` record for (ino, block) stamped with
/// `token` — the crash-survivable custody artifact the mount sweep
/// classifies (the extent_record_recovery_tests planting pattern).
fn plant_record(h: &H, ino: u64, block: u32, token: u64, tag: u8) -> String {
    let key = squeezefs::keys::active_block_ext(ino, block as u64).to_string();
    let rec = ExtentRecord {
        version: EXTENT_RECORD_VERSION,
        fencing_token: token,
        block_idx: block,
        base_deferred: true,
        extents: vec![(4096, pattern(2048, tag))],
    };
    assert!(
        h.fs.router
            .cache
            .nvme
            .put_active_block(&key, &rec.serialize(), token),
        "planting record for block {block}"
    );
    key
}

/// Stage the two-era shape: a durable striped file, a block-1 record
/// stamped by era T1, then a NEWER era T2 minted for the same ino and a
/// block-2 record stamped by it. Returns (ino, t1, t2).
async fn stage_two_era_records(h: &H) -> (u64, u64, u64) {
    let ino = durable_striped(h, "two-era.dat", 4, 0x11).await;
    let t1 = mint_era(&h.dlm, ino).await;
    assert!(t1 > 0, "a real mint carries token N > 0 (got {t1})");
    plant_record(h, ino, 1, t1, 0x21);
    let t2 = mint_era(&h.dlm, ino).await;
    assert!(
        t2 > t1,
        "in-RAM per-object monotonicity premise: {t1} -> {t2}"
    );
    plant_record(h, ino, 2, t2, 0x22);
    (ino, t1, t2)
}

const CHILD_DIR_ENV: &str = "SQZ_FENCING_REMOUNT_CHILD_DIR";

/// **Crash child** (env-guarded helper, not a standalone contract): stages
/// the two-era records over the parent-provided persistent paths, writes
/// the manifest, and dies via `process::exit` — no drain, no Drop, and
/// (the point) no survival of any process-global fencing state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "crash-child helper for remount_after_crash_discards_superseded_fencing_records; \
            no-op unless SQZ_FENCING_REMOUNT_CHILD_DIR is set"]
async fn crash_child_stage_two_era_records() {
    let Ok(dir) = std::env::var(CHILD_DIR_ENV) else {
        return; // invoked outside the parent test: nothing to do
    };
    let dir = std::path::PathBuf::from(dir);
    let h = session(
        &dir.join("meta.img"),
        &dir.join("backing.img"),
        &dir.join("staging"),
    )
    .await;
    let (ino, t1, t2) = stage_two_era_records(&h).await;
    std::fs::write(dir.join("manifest.txt"), format!("{ino} {t1} {t2}")).unwrap();
    // Kill-9 equivalent: staging mmap (MAP_SHARED) + the fsync'd meta
    // survive; every in-RAM structure — including the fencing
    // generators — dies with the process.
    std::process::exit(0);
}

/// §6.11 — **the remount contract**: after a real process death, the
/// mount sweep must still enforce the remount law — the record stamped
/// by the SUPERSEDED era (t1, provably older than t2 which is itself
/// staged beside it) is discarded loudly; the newest-era record is
/// recovered.
///
/// RED PRE-S2 (spec §6.11): the fresh process's fencing generators read
/// 0, `t1 < 0` is false, and both records are adopted —
/// `extent_records_stale_discarded` could not move at mount time.
///
/// GREEN AT S2 (durable `WriterClaim.term`, composed tokens
/// `(term << 40) | grant_seq`, incompat bit 7 — spec §6.7 decision 4 +
/// §6.9): the remount bumps the durable term, so the pre-crash stamps
/// are a foreign era and the live read can no longer classify them —
/// the sweep uses the newest surviving stamp for the ino instead, which
/// discriminates *per-ino currency* (t1 discarded, t2 adopted) rather
/// than blanket-discarding crash residue. S0/S1 deliberately did NOT
/// green it: they changed the mint, not its durability.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remount_after_crash_discards_superseded_fencing_records() {
    let _g = serial().await;
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("staging")).unwrap();
    std::fs::File::create(dir.path().join("backing.img"))
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    format_meta(&dir.path().join("meta.img")).await;

    // Session 1 = a REAL child process (fresh statics on both sides of
    // the crash boundary — the only honest §6.11 shape).
    let exe = std::env::current_exe().unwrap();
    let status = tokio::process::Command::new(&exe)
        .arg("--exact")
        .arg("crash_child_stage_two_era_records")
        .arg("--ignored")
        .arg("--nocapture")
        .env(CHILD_DIR_ENV, dir.path())
        .status()
        .await
        .expect("spawn crash child");
    assert!(status.success(), "crash child failed: {status:?}");
    let manifest = std::fs::read_to_string(dir.path().join("manifest.txt"))
        .expect("child must have staged its records before dying");
    let mut it = manifest.split_whitespace();
    let ino: u64 = it.next().unwrap().parse().unwrap();
    let t1: u64 = it.next().unwrap().parse().unwrap();
    let t2: u64 = it.next().unwrap().parse().unwrap();
    assert!(t2 > t1 && t1 > 0, "child premise: 0 < {t1} < {t2}");

    // Session 2 = the remount, in THIS (fresh w.r.t. ino) process.
    let h2 = session(
        &dir.path().join("meta.img"),
        &dir.path().join("backing.img"),
        &dir.path().join("staging"),
    )
    .await;
    let stale_before = METRICS
        .extent_records_stale_discarded
        .load(Ordering::Relaxed);
    let recovered = h2.fs.recover_extent_records().await;
    let stale_after = METRICS
        .extent_records_stale_discarded
        .load(Ordering::Relaxed);

    // THE CONTRACT (the remount law, fencing arm): the superseded era's
    // record is discarded — loudly, counter-visible.
    assert!(
        stale_after > stale_before,
        "remount law: the era-{t1} record (superseded by era {t2} BEFORE \
         the crash) must be DISCARDED at mount — got {} discards \
         (current fencing read 0 in the fresh process: spec §6.11)",
        stale_after - stale_before
    );
    let stale_key = squeezefs::keys::active_block_ext(ino, 1).to_string();
    assert!(
        h2.fs
            .router
            .cache
            .nvme
            .read_extent_record(&stale_key)
            .is_none(),
        "the superseded record must leave the staging population"
    );
    // The newest era's record is legitimate crash residue: recovered.
    assert_eq!(
        recovered, 1,
        "exactly the newest-era record is recoverable custody"
    );
    let current_key = squeezefs::keys::active_block_ext(ino, 2).to_string();
    assert!(
        h2.fs
            .router
            .cache
            .nvme
            .read_extent_record(&current_key)
            .is_some(),
        "the era-{t2} record must survive the sweep"
    );
}

/// **The in-RAM monotonicity pin (non-ignored)** — the same two-era shape
/// WITHOUT a crash: within one process the sweep must discard the
/// superseded record AND recover the current-era record. This pins both
/// directions S1 could break while replacing `FENCING_MAP`:
///
/// - the ino's readable generation stays ≥ its own newest grant even
///   after every lease is released (else the stale arm dies), and
/// - it does not silently exceed the newest grant when nothing newer was
///   minted (else the recover arm dies — an over-approximating read would
///   discard live custody).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_process_sweep_discards_superseded_and_keeps_current_era() {
    let _g = serial().await;
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("staging")).unwrap();
    std::fs::File::create(dir.path().join("backing.img"))
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    format_meta(&dir.path().join("meta.img")).await;

    let h = session(
        &dir.path().join("meta.img"),
        &dir.path().join("backing.img"),
        &dir.path().join("staging"),
    )
    .await;
    let (ino, t1, t2) = stage_two_era_records(&h).await;

    let stale_before = METRICS
        .extent_records_stale_discarded
        .load(Ordering::Relaxed);
    let recovered_before = METRICS.extent_records_recovered.load(Ordering::Relaxed);
    let recovered = h.fs.recover_extent_records().await;
    let stale_after = METRICS
        .extent_records_stale_discarded
        .load(Ordering::Relaxed);
    let recovered_after = METRICS.extent_records_recovered.load(Ordering::Relaxed);

    assert_eq!(
        stale_after - stale_before,
        1,
        "in-RAM monotonicity: the era-{t1} record is superseded by the \
         era-{t2} mint and must be discarded"
    );
    assert_eq!(
        recovered, 1,
        "the era-{t2} record is CURRENT custody and must be recovered \
         (an over-approximating fencing read would discard it)"
    );
    assert_eq!(recovered_after - recovered_before, 1, "counter-visible");
    let stale_key = squeezefs::keys::active_block_ext(ino, 1).to_string();
    let current_key = squeezefs::keys::active_block_ext(ino, 2).to_string();
    assert!(
        h.fs.router
            .cache
            .nvme
            .read_extent_record(&stale_key)
            .is_none(),
        "superseded record leaves staging"
    );
    assert!(
        h.fs.router
            .cache
            .nvme
            .read_extent_record(&current_key)
            .is_some(),
        "current-era record stays composable custody"
    );
}
