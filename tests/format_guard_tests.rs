//! Contract tests for the format guard
//! (`kv::builder::format_preflight` / `kv::builder::format_v3`) —
//! re-pinned against the v3 formatter when v2 support was removed (the
//! contracts predate the format and survive it).
//!
//! Policy:
//! - A volume with a **valid superblock** (already formatted) is refused
//!   without `--force` — even when idle — so a fat-fingered format cannot
//!   destroy a filesystem silently.
//! - A **live** client registration (fresh heartbeat, read from the v3
//!   xattr tree of the root ino) blocks format even WITH `--force`:
//!   reformatting under an active mount is never safe.
//! - **Stale** registrations (crashed clients) never block.
//! - A blank (never formatted) volume formats without `--force`.
//! - Preflight alone has no side effects (it is the CLI's all-volumes
//!   gate run before ANY volume is wiped) — it opens a read-only probe
//!   mount and writes nothing.
//! - A volume whose superblock this binary **refuses to interpret** (a
//!   legacy v2 superblock, a pre-watermark v3 one — Finding A, unknown
//!   future incompat bits, a torn checksum) follows the SAME ladder as a
//!   healthy formatted volume: mount refuses loud, format without
//!   `--force` refuses per the reformat guard, and `format --force`
//!   CLOBBERS it — the refusal message demands a reformat, so `--force`
//!   must be able to deliver one. Such a volume cannot be live-mounted by
//!   this binary, so it cannot have live current clients by construction;
//!   the live-client probe is impossible AND unnecessary there.

use squeezefs::fuse_client::CLIENT_STALE_TTL_SECS;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_preflight, format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, write_superblock_v3, VolumeFormat, FEATURE_INCOMPAT_KV_V3,
    FEATURE_INCOMPAT_NODE_SEQ_WATERMARK,
};
use squeezefs::meta_backend::{open_volume_for_mount, Metadata};
use squeezefs_testkit::{mount_supported, site};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;

const VOL_LEN: u64 = 64 * 1024 * 1024;

fn opts(force: bool) -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: Some(1024 * 1024),
        force,
        full_wipe: false,
        format_config_xattr: None,
    }
}

fn reg_value(ts: u64) -> Vec<u8> {
    format!("{{\"ts\":{},\"pid\":{}}}", ts, 4242).into_bytes()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn blank_volume() -> NamedTempFile {
    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(VOL_LEN).unwrap();
    meta
}

async fn formatted_volume() -> NamedTempFile {
    let meta = blank_volume();
    format_v3(meta.path(), VOL_LEN, &opts(true)).await.unwrap();
    meta
}

/// Forge the pre-watermark shape the Finding-A gate refuses: a real v3
/// volume whose superblock lacks incompat bit 1 (the same forge the §6.1
/// gate suite in `tests/kv_backend_tests.rs` uses — no pre-watermark
/// writer exists anymore).
async fn forge_pre_watermark(path: &std::path::Path) {
    let sb = match classify_volume(path).await.expect("classify the v3 volume") {
        VolumeFormat::V3(sb) => sb,
        other => panic!("expected a v3 volume to forge, got {other:?}"),
    };
    let mut pre = sb.clone();
    pre.features_incompat = FEATURE_INCOMPAT_KV_V3;
    write_superblock_v3(path, &pre)
        .await
        .expect("write the forged pre-watermark superblock");
}

/// Write a `client:{id}` registration into the volume's v3 xattr tree the
/// way a mounted client does (setxattr on the root ino), then shut the
/// backend down cleanly.
async fn register_client(meta: &NamedTempFile, id: &str, ts: u64) {
    let be = KvMetaBackend::open(meta.path()).await.unwrap();
    // `client:{id}` is an INTERNAL record (VAL-2): the generic `Metadata`
    // entry points refuse it — plant it through the daemon's own writer.
    be.setxattr_internal(1, &format!("client:{id}"), &reg_value(ts))
        .await
        .unwrap();
    be.shutdown().await.unwrap();
}

#[tokio::test]
async fn blank_volume_formats_without_force() {
    let meta = blank_volume();
    format_v3(meta.path(), VOL_LEN, &opts(false))
        .await
        .expect("a never-formatted volume must format without --force");
}

#[tokio::test]
async fn formatted_volume_without_force_is_refused() {
    let meta = formatted_volume().await;
    let err = format_v3(meta.path(), VOL_LEN, &opts(false))
        .await
        .expect_err("an already-formatted volume must be refused without --force")
        .to_string();
    assert!(
        err.contains("already formatted") && err.contains("--force"),
        "error must say the volume is formatted and mention --force, got: {err}"
    );
}

#[tokio::test]
async fn formatted_volume_with_force_is_reformatted() {
    let meta = formatted_volume().await;
    format_v3(meta.path(), VOL_LEN, &opts(true))
        .await
        .expect("--force must reformat an idle formatted volume");
}

#[tokio::test]
async fn live_client_blocks_format_even_with_force() {
    let meta = formatted_volume().await;
    register_client(&meta, "live", now_secs()).await;

    let err = format_v3(meta.path(), VOL_LEN, &opts(true))
        .await
        .expect_err("a live mounted client must block format even with --force")
        .to_string();
    assert!(
        err.contains("mounted"),
        "error must call out the live mount, got: {err}"
    );
}

#[tokio::test]
async fn stale_client_does_not_block_forced_format() {
    let meta = formatted_volume().await;
    let stale_ts = now_secs().saturating_sub(CLIENT_STALE_TTL_SECS + 60);
    register_client(&meta, "dead", stale_ts).await;

    format_v3(meta.path(), VOL_LEN, &opts(true))
        .await
        .expect("a stale (crashed) client must not block a forced format");
}

/// Preflight alone must not change anything: it is the CLI's
/// no-side-effect gate run across ALL volumes before ANY volume is wiped,
/// so a refused multi-volume format leaves every volume untouched.
/// Byte-level pin: the probe mount spawns no checkpoint task and writes
/// nothing, so the whole volume image is bit-identical after a refused
/// preflight — a stronger guarantee than the retired v2 gate (which
/// reaped stale registrations in place) ever gave.
#[tokio::test]
async fn preflight_refuses_without_side_effects() {
    let meta = formatted_volume().await;
    // Plant a marker xattr to prove the namespace survives preflight.
    {
        let be = KvMetaBackend::open(meta.path()).await.unwrap();
        be.setxattr(1, "user.marker", b"survives").await.unwrap();
        be.shutdown().await.unwrap();
    }
    let image_before = std::fs::read(meta.path()).unwrap();

    format_preflight(meta.path(), false)
        .await
        .expect_err("preflight must refuse an already-formatted volume without force");

    let image_after = std::fs::read(meta.path()).unwrap();
    assert_eq!(
        image_before, image_after,
        "a refused preflight must leave the volume byte-identical (read-only probe)"
    );

    let be = KvMetaBackend::open(meta.path()).await.unwrap();
    let marker = be
        .getxattr(1, "user.marker")
        .await
        .unwrap()
        .expect("volume must be untouched after a refused preflight");
    assert_eq!(marker, b"survives");
    be.shutdown().await.unwrap();
}

#[tokio::test]
async fn preflight_allows_forced_idle_reformat() {
    let meta = formatted_volume().await;
    format_preflight(meta.path(), true)
        .await
        .expect("preflight with force on an idle formatted volume must pass");
}

/// A legacy v2 superblock (crafted bytes — no v2 writer exists) walks the
/// full unsupported-volume ladder: mount refuses loud, format without
/// `--force` refuses per the reformat guard (side-effect-free), and
/// `--force` reformats it to a mountable current v3 volume. Regression pin
/// that the Finding-A watermark gate did not break the v2 `--force` path.
#[tokio::test]
async fn legacy_v2_volume_guarded_and_reformattable() {
    let meta = blank_volume();
    let mut legacy_sb = Vec::with_capacity(12);
    legacy_sb.extend_from_slice(b"METALV01");
    legacy_sb.extend_from_slice(&2u32.to_le_bytes());
    squeezefs::uring_fs::write_at(meta.path(), 0, bytes::Bytes::from(legacy_sb))
        .await
        .unwrap();

    // 1. Mount refuses loud (the v2-purge contract — keep).
    let err = open_volume_for_mount(meta.path().to_str().unwrap())
        .await
        .expect_err("mounting a legacy v2 volume must refuse loud")
        .to_string();
    assert!(
        err.contains("no longer supported"),
        "the v2 mount refusal must be the precise 'no longer supported' message, got: {err}"
    );

    // 2. format without --force: refused per the reformat guard…
    let image_before = std::fs::read(meta.path()).unwrap();
    let err = format_preflight(meta.path(), false)
        .await
        .expect_err("a legacy v2 volume must be refused without --force")
        .to_string();
    assert!(
        err.contains("--force"),
        "the format refusal must point at --force (the reformat guard), got: {err}"
    );
    // 3. …and byte-identical after the refusal.
    assert_eq!(
        std::fs::read(meta.path()).unwrap(),
        image_before,
        "a refused format must leave the v2 volume byte-identical"
    );

    // 4. --force clobbers it; 5. the result mounts as CURRENT v3
    // (watermark bit present).
    format_v3(meta.path(), VOL_LEN, &opts(true))
        .await
        .expect("--force must reformat a legacy v2 volume to v3");
    let be = open_volume_for_mount(meta.path().to_str().unwrap())
        .await
        .expect("the reformatted volume mounts as v3");
    assert_ne!(
        be.superblock().features_incompat & FEATURE_INCOMPAT_NODE_SEQ_WATERMARK,
        0,
        "the reformatted volume must carry the node-seq watermark incompat bit"
    );
    be.shutdown().await.unwrap();
}

/// The user-hit Finding-A regression: the pre-watermark mount refusal
/// ("reformat required") must NOT also fire inside `format --force` — the
/// message would demand the very operation it blocks. Full ladder: mount
/// refuses loud (keep) → format without `--force` refuses per the
/// reformat guard → the refusal is side-effect-free → `format --force`
/// SUCCEEDS → the fresh volume mounts with the watermark present.
#[tokio::test]
async fn pre_watermark_v3_volume_guarded_and_force_reformattable() {
    let meta = formatted_volume().await;
    forge_pre_watermark(meta.path()).await;

    // 1. Mount refuses loud, naming the watermark gate and the remedy
    //    (the Finding-A contract — keep green).
    let err = open_volume_for_mount(meta.path().to_str().unwrap())
        .await
        .expect_err("mounting a pre-watermark v3 volume must refuse loud")
        .to_string();
    assert!(
        err.contains("watermark") && err.contains("reformat required"),
        "the mount refusal must name the watermark gate and the remedy, got: {err}"
    );

    // 2. format without --force: the reformat guard owns this surface —
    //    the refusal must point at --force, not dead-end on the mount
    //    refusal.
    let image_before = std::fs::read(meta.path()).unwrap();
    let err = format_v3(meta.path(), VOL_LEN, &opts(false))
        .await
        .expect_err("a pre-watermark volume must still be format-guarded without --force")
        .to_string();
    assert!(
        err.contains("--force"),
        "the format refusal must point at --force (the reformat guard), got: {err}"
    );

    // 3. The refusal has no side effects (byte-identical volume).
    assert_eq!(
        std::fs::read(meta.path()).unwrap(),
        image_before,
        "a refused format must leave the pre-watermark volume byte-identical"
    );

    // 4. format WITH --force succeeds: the volume cannot be live-mounted
    //    by this binary, so no live current client can exist — --force
    //    must deliver the reformat its own refusal message demands.
    format_v3(meta.path(), VOL_LEN, &opts(true))
        .await
        .expect("--force must reformat a pre-watermark v3 volume");

    // 5. The reformatted volume mounts as current v3, watermark present.
    let be = open_volume_for_mount(meta.path().to_str().unwrap())
        .await
        .expect("the reformatted volume mounts as current v3");
    assert_ne!(
        be.superblock().features_incompat & FEATURE_INCOMPAT_NODE_SEQ_WATERMARK,
        0,
        "the reformatted volume must carry the node-seq watermark incompat bit"
    );
    be.shutdown().await.unwrap();
}

/// The user's exact multi-volume shape: a 4-volume metadata set where
/// only SOME volumes are pre-watermark. The CLI preflights EVERY volume
/// before ANY volume is wiped, then formats them all — without `--force`
/// every volume refuses and stays byte-identical; with `--force` the
/// whole set (mixed classes included) reformats to mountable current v3.
#[tokio::test]
async fn mixed_pre_watermark_volume_set_force_reformats_all() {
    let mut vols = Vec::new();
    for _ in 0..4 {
        vols.push(formatted_volume().await);
    }
    forge_pre_watermark(vols[0].path()).await;
    forge_pre_watermark(vols[2].path()).await;

    // Without --force: the all-volumes preflight refuses each volume
    // (current v3 and pre-watermark alike) and leaves EVERY volume
    // byte-identical — the CLI's "refused format wipes nothing" pin.
    let images: Vec<Vec<u8>> = vols
        .iter()
        .map(|v| std::fs::read(v.path()).unwrap())
        .collect();
    for v in &vols {
        format_preflight(v.path(), false)
            .await
            .expect_err("every non-blank volume must refuse format without --force");
    }
    for (v, img) in vols.iter().zip(&images) {
        assert_eq!(
            &std::fs::read(v.path()).unwrap(),
            img,
            "a refused preflight must be side-effect-free on every volume"
        );
    }

    // With --force: the CLI loop shape — preflight ALL volumes first,
    // then format them all. The mixed set must pass both phases.
    for v in &vols {
        format_preflight(v.path(), true)
            .await
            .expect("--force preflight must pass on every volume, pre-watermark included");
    }
    for v in &vols {
        format_v3(v.path(), VOL_LEN, &opts(true))
            .await
            .expect("--force must reformat every volume in the mixed set");
    }
    for v in &vols {
        let be = open_volume_for_mount(v.path().to_str().unwrap())
            .await
            .expect("every reformatted volume mounts as current v3");
        assert_ne!(
            be.superblock().features_incompat & FEATURE_INCOMPAT_NODE_SEQ_WATERMARK,
            0,
            "every reformatted volume must carry the watermark incompat bit"
        );
        be.shutdown().await.unwrap();
    }
}

/// The force gate over a REFUSED superblock must bury prior-generation
/// residue exactly like the recognized-current-v3 reformat path — the
/// user's future reformats take this gate (root-daemon EIO takeover,
/// burial-delta audit: `.benchmarks/2026-07-13-rootd-eio-residue-history-
/// exclusion.md`). Mechanistic, per residue class: journal-ring bytes
/// proven present pre-format and all-zero post-format (nothing to
/// replay); heap node frames proven to SURVIVE the quick format while no
/// ghost dentry is served (burial-by-admission via uuid-namespaced node
/// seqs, not erasure); the first create mints ino 2 (fresh `next_ino`
/// watermark — no ledger/allocator residue); fresh generation uuid.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_gate_over_refused_superblock_buries_all_residue_classes() {
    use squeezefs::meta_backend::kv::builder::ROOT_INO;

    let meta = blank_volume();

    // Aged generation: live-path population with enough payload to
    // spread residue through the ring and several heap extents.
    format_v3(meta.path(), VOL_LEN, &opts(true))
        .await
        .expect("gen1 format");
    let be = KvMetaBackend::open(meta.path()).await.expect("gen1 open");
    let big = vec![0xa5u8; 8 * 1024];
    for i in 0..40 {
        let ino = be
            .create(ROOT_INO, &format!("gen1_{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("gen1 create")
            .ino;
        be.setxattr(ino, "user.residue", &big)
            .await
            .expect("gen1 xattr");
    }
    be.shutdown().await.expect("gen1 shutdown");
    drop(be);

    let gen1_sb = match classify_volume(meta.path()).await.unwrap() {
        VolumeFormat::V3(sb) => sb,
        other => panic!("expected v3 before the forge, got {other:?}"),
    };
    let ring = gen1_sb.journal;
    let image = std::fs::read(meta.path()).unwrap();
    assert!(
        image[ring.start as usize..ring.end() as usize]
            .iter()
            .any(|b| *b != 0),
        "aging must leave journal-ring residue or this pin is vacuous"
    );

    // The refused class the user hits: pre-watermark v3. The force
    // ladder: refused without --force (pointing at it), reformatted with
    // it — burial runs downstream of the gate, unconditionally.
    forge_pre_watermark(meta.path()).await;
    let err = format_v3(meta.path(), VOL_LEN, &opts(false))
        .await
        .expect_err("pre-watermark volume must be format-guarded without --force")
        .to_string();
    assert!(
        err.contains("--force"),
        "the refusal must point at --force, got: {err}"
    );
    format_v3(meta.path(), VOL_LEN, &opts(true))
        .await
        .expect("--force must reformat the refused (pre-watermark) volume");

    let gen2_sb = match classify_volume(meta.path()).await.unwrap() {
        VolumeFormat::V3(sb) => sb,
        other => panic!("expected v3 after the force reformat, got {other:?}"),
    };
    assert_ne!(gen2_sb.uuid, gen1_sb.uuid, "fresh generation identity");
    assert_ne!(
        gen2_sb.features_incompat & FEATURE_INCOMPAT_NODE_SEQ_WATERMARK,
        0,
        "the reformatted volume must carry the watermark bit"
    );
    assert_eq!(
        gen2_sb.journal, ring,
        "identical knobs must re-plan identical geometry"
    );

    // Journal burial: the whole fresh ring is zero — nothing to replay.
    let image = std::fs::read(meta.path()).unwrap();
    assert!(
        image[ring.start as usize..ring.end() as usize]
            .iter()
            .all(|b| *b == 0),
        "the force gate over a refused superblock must zero the whole ring"
    );
    // Heap residue survives (burial by admission, not erasure). If a
    // future format full-wipes by default this turns vacuously true; the
    // ghost assertions below still hold.
    let heap = gen2_sb.heap;
    let node = u64::from(gen2_sb.node_size);
    let scan =
        &image[(heap.start + 8 * node) as usize..(heap.start + 64 * node).min(heap.end()) as usize];
    assert!(
        scan.iter().any(|b| *b != 0),
        "expected surviving heap frames (quick format leaves the heap)"
    );

    // Ghost checks + fresh ino watermark.
    let be = KvMetaBackend::open(meta.path()).await.expect("gen2 open");
    let entries = be.readdir(ROOT_INO, 0, 4096).await.expect("readdir");
    let ghosts: Vec<&str> = entries
        .iter()
        .map(|e| e.name.as_str())
        .filter(|n| n.starts_with("gen1_"))
        .collect();
    assert!(
        ghosts.is_empty(),
        "force gate resurrected {} dead dentries, e.g. {:?}",
        ghosts.len(),
        ghosts.first()
    );
    assert!(
        be.lookup(ROOT_INO, "gen1_0").await.is_err(),
        "dead generation's file served through the force-gate reformat"
    );
    let probe = be
        .create(ROOT_INO, "probe", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("probe create");
    assert_eq!(probe.ino, ROOT_INO + 1, "fresh next_ino watermark");
    be.shutdown().await.expect("gen2 shutdown");
}

// ---------------------------------------------------------------------------
// CLI smoke — the user-hit regression end to end, against the real binary:
// `format --force` over a crafted pre-watermark multi-volume sandbox must
// succeed, and the result must mount, take writes, and survive a remount.
// ---------------------------------------------------------------------------

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

/// Scratch under ~/tmp (repo discipline: scratch lives in ~/tmp; unique
/// per-process path so parallel runs never collide).
fn scratch(tag: &str) -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME set");
    let base = std::path::PathBuf::from(home)
        .join("tmp")
        .join(format!("fmtfix_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// Run a CLI invocation with a hard deadline; a hang is converted into a
/// loud failure instead of wedging the suite.
fn run_with_deadline(
    mut cmd: std::process::Command,
    deadline: std::time::Duration,
    what: &str,
) -> std::process::Output {
    let start = std::time::Instant::now();
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {what}: {e}"));
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => break,
            None if start.elapsed() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{what} did not exit within {deadline:?} — must fail fast and loud");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(25)),
        }
    }
    child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("collect {what} output: {e}"))
}

/// A spawned `squeezefs mount` child: unmounted via `fusermount3 -u` and
/// reaped BY PID on drop (never by name).
struct CliMount {
    child: std::process::Child,
    mnt: std::path::PathBuf,
}

impl CliMount {
    fn unmount(&mut self) {
        for _ in 0..10 {
            let st = std::process::Command::new("fusermount3")
                .arg("-u")
                .arg(&self.mnt)
                .status()
                .expect("run fusermount3 -u");
            if st.success() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            if self.child.try_wait().expect("try_wait").is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        let _ = self.child.kill();
        panic!("mount daemon did not exit within 30s of unmount");
    }
}

impl Drop for CliMount {
    fn drop(&mut self) {
        // Drop-time double-unmount: already-unmounted is the EXPECTED case —
        // silence the mtab noise; the explicit unmount path stays loud.
        let _ = std::process::Command::new("fusermount3")
            .arg("-uz")
            .arg(&self.mnt)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn `squeezefs mount <metas...> <mnt>` and wait for the stats inode.
fn spawn_cli_mount(
    metas: &[std::path::PathBuf],
    mnt: &std::path::Path,
    log: &std::path::Path,
) -> CliMount {
    std::fs::create_dir_all(mnt).unwrap();
    let logf = std::fs::File::create(log).unwrap();
    let mut cmd = std::process::Command::new(bin());
    cmd.arg("mount");
    for m in metas {
        cmd.arg(format!("sqmeta://{}", m.display()));
    }
    let child = cmd
        .arg(mnt)
        .arg("--uid")
        .arg(unsafe { libc::getuid() }.to_string())
        .arg("--gid")
        .arg(unsafe { libc::getgid() }.to_string())
        .stdout(std::process::Stdio::from(logf.try_clone().unwrap()))
        .stderr(std::process::Stdio::from(logf))
        .spawn()
        .expect("spawn squeezefs mount");
    let mount = CliMount {
        child,
        mnt: mnt.to_path_buf(),
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        if std::fs::read_to_string(mount.mnt.join(".stats")).is_ok() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "mount did not become ready in 90s; log:\n{}",
            std::fs::read_to_string(log).unwrap_or_default()
        );
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    mount
}

/// The user's exact flow, end to end: seed a 4-meta-volume set, forge two
/// volumes pre-watermark, then (a) mount refuses loud, (b) `format`
/// without `--force` refuses pointing at `--force`, (c) the user's
/// `format --force` command SUCCEEDS, (d) the fresh filesystem mounts,
/// takes a write, and serves it back across a remount.
#[test]
fn cli_format_force_reformats_pre_watermark_set_then_mounts() {
    let base = scratch("cli");
    let metas: Vec<std::path::PathBuf> = (0..4).map(|i| base.join(format!("mds{i}.bin"))).collect();
    for m in &metas {
        std::fs::File::create(m)
            .unwrap()
            .set_len(256 * 1024 * 1024)
            .unwrap();
    }
    let data = base.join("data.bin");
    std::fs::File::create(&data)
        .unwrap()
        .set_len(2 * 1024 * 1024 * 1024)
        .unwrap();

    let format_cmd = |force: bool| {
        let mut cmd = std::process::Command::new(bin());
        cmd.arg("format");
        for m in &metas {
            cmd.arg(format!("sqmeta://{}", m.display()));
        }
        cmd.arg(format!("sqdata://{}", data.display()));
        if force {
            cmd.arg("--force");
        }
        cmd
    };

    // Seed: a current-format 4-volume set.
    let out = run_with_deadline(
        format_cmd(true),
        std::time::Duration::from_secs(180),
        "seed format",
    );
    assert!(
        out.status.success(),
        "seed format failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Forge volumes 0 and 2 pre-watermark (the user's mixed shape).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        forge_pre_watermark(&metas[0]).await;
        forge_pre_watermark(&metas[2]).await;
    });

    // (a) Mount refuses loud, naming the pre-watermark gate.
    let mnt = base.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    let mut refuse_cmd = std::process::Command::new(bin());
    refuse_cmd.arg("mount");
    for m in &metas {
        refuse_cmd.arg(format!("sqmeta://{}", m.display()));
    }
    refuse_cmd.arg(&mnt);
    let out = run_with_deadline(
        refuse_cmd,
        std::time::Duration::from_secs(60),
        "mount of a pre-watermark set",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "mounting a pre-watermark set must fail\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        format!("{stdout}{stderr}").contains("pre-watermark"),
        "the mount refusal must name the pre-watermark gate\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // (b) format WITHOUT --force: refused, pointing at --force.
    let out = run_with_deadline(
        format_cmd(false),
        std::time::Duration::from_secs(60),
        "format without --force",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "format without --force must refuse a non-blank set\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        format!("{stdout}{stderr}").contains("--force"),
        "the refusal must point at --force\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // (c) The user's exact command: format --force over the mixed set.
    let out = run_with_deadline(
        format_cmd(true),
        std::time::Duration::from_secs(180),
        "format --force over the pre-watermark set",
    );
    assert!(
        out.status.success(),
        "format --force must reformat a pre-watermark set (the user-hit regression): {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // (d) Mount + write + remount + read-back (transport-gated).
    // `mount_supported` has already ledgered the class-`mount` record (and
    // failed the test outright under SQUEEZEFS_TEST_REQUIRE_MOUNT=1); the
    // format phases above ran unconditionally.
    if !mount_supported(site!()) {
        let _ = std::fs::remove_dir_all(&base);
        return;
    }
    let payload: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    {
        let mut m = spawn_cli_mount(&metas, &mnt, &base.join("mount1.log"));
        std::fs::write(mnt.join("smoke.bin"), &payload).expect("write through the fresh mount");
        let back = std::fs::read(mnt.join("smoke.bin")).expect("read back");
        assert_eq!(back, payload, "read-back mismatch on the fresh mount");
        m.unmount();
    }
    {
        let mut m = spawn_cli_mount(&metas, &mnt, &base.join("mount2.log"));
        let back = std::fs::read(mnt.join("smoke.bin")).expect("read after remount");
        assert_eq!(back, payload, "read-back mismatch after remount");
        m.unmount();
    }
    let _ = std::fs::remove_dir_all(&base);
}
