//! CLI stub-verb contracts (docs-audit findings F1/F2/F3, 2026-07-17).
//!
//! The `95085ad` docs audit live-verified three lying CLI surfaces:
//!
//! * **F1 `squeezefs clients`** hardcoded "No active clients connected."
//!   and `squeezefs status` reported `"Clients": []` against a live mount
//!   — while the truth already exists on the volume: the mount heartbeat
//!   maintains `client:{id}` registrations and the single-writer guard's
//!   `writer_claim` on the root ino, and the format preflight already
//!   classifies them under the ONE staleness law
//!   ([`CLIENT_STALE_TTL_SECS`]). Both surfaces must serve those records
//!   (id, pid, heartbeat age, staleness) — no new liveness protocol.
//! * **F2 `squeezefs df`** printed "space usage command (df) is offline."
//!   statfs is already honest (formatted capacity, allocator-tracked
//!   usage); `df` must answer the same numbers as an OFFLINE/URI query
//!   (read-only probe of the meta volumes — the `status` access pattern),
//!   aggregate + per-volume, bytes and inodes, human + `--json`.
//! * **F3 `squeezefs defrag`** returned fake success from a no-op engine.
//!   The verb is REMOVED (no dead code, no fake surface): a stale script
//!   must fail loudly with an unknown-subcommand error, and `--help` must
//!   not advertise it. (The dead `jobs.rs` BlockMove machinery was
//!   deleted in VL1 — design-volume-lifecycle §5.0.)
//!
//! All CLI tests drive the real binary (`CARGO_BIN_EXE_squeezefs`);
//! mount-needing tests skip cleanly where FUSE-over-io_uring is
//! unavailable (the statfs/cache-policy suite convention).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use squeezefs::fuse_client::CLIENT_STALE_TTL_SECS;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs_testkit::{mount_supported, site};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

/// Scratch under ~/tmp (repo discipline: scratch lives in ~/tmp).
fn scratch(tag: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME set");
    let base = PathBuf::from(home)
        .join("tmp")
        .join(format!("sqfs_cliverbs_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create scratch dir");
    base
}

fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run squeezefs {args:?}: {e}"))
}

fn stdout_json(out: &Output, what: &str) -> serde_json::Value {
    assert!(
        out.status.success(),
        "{what} failed (exit {:?}):\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{what} stdout must be pure JSON ({e}):\n{}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

/// Format one meta + one data volume through the real binary.
fn format_volume(base: &Path, data_bytes: u64) -> (PathBuf, PathBuf) {
    let meta = base.join("meta.bin");
    let data = base.join("data.bin");
    std::fs::File::create(&meta)
        .expect("create meta file")
        .set_len(256 * MIB)
        .expect("size meta file");
    std::fs::File::create(&data)
        .expect("create data file")
        .set_len(data_bytes)
        .expect("size data file");
    let out = Command::new(bin())
        .arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(format!("sqdata://{}", data.display()))
        .arg("--force")
        .arg("--disk-cache-paths")
        .arg(base.join("staging"))
        .output()
        .expect("run squeezefs format");
    assert!(
        out.status.success(),
        "format failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (meta, data)
}

struct Mount {
    child: Child,
    mnt: PathBuf,
}

impl Mount {
    fn daemon_pid(&self) -> u32 {
        self.child.id()
    }

    /// Clean unmount through the real verb; waits for the daemon to exit.
    fn unmount_clean(&mut self) {
        let out = Command::new(bin())
            .arg("umount")
            .arg(&self.mnt)
            .output()
            .expect("run squeezefs umount");
        assert!(
            out.status.success(),
            "umount failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match self.child.try_wait().expect("try_wait") {
                Some(_) => break,
                None if Instant::now() > deadline => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    panic!("mount daemon did not exit within 30s of umount");
                }
                None => std::thread::sleep(Duration::from_millis(200)),
            }
        }
    }

    /// kill -9 the daemon (crash shape: no unregistration runs), then lazy-
    /// unmount the dead mountpoint so the scratch dir can be removed.
    fn kill9(&mut self) {
        self.child.kill().expect("SIGKILL the daemon");
        let _ = self.child.wait();
        let _ = Command::new("fusermount3")
            .arg("-uz")
            .arg(&self.mnt)
            .status();
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        // Drop-time double-unmount: already-unmounted is the EXPECTED case —
        // silence the mtab noise; the explicit unmount path stays loud.
        let _ = Command::new("fusermount3")
            .arg("-uz")
            .arg(&self.mnt)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_mount(meta: &Path, mnt: &Path, log: &Path) -> Mount {
    std::fs::create_dir_all(mnt).expect("create mountpoint");
    let logf = std::fs::File::create(log).expect("create log");
    let child = Command::new(bin())
        .arg("mount")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(mnt)
        .arg("--uid")
        .arg(unsafe { libc::getuid() }.to_string())
        .arg("--gid")
        .arg(unsafe { libc::getgid() }.to_string())
        .stdout(Stdio::from(logf.try_clone().expect("clone log fd")))
        .stderr(Stdio::from(logf))
        .spawn()
        .expect("spawn squeezefs mount");
    let mount = Mount {
        child,
        mnt: mnt.to_path_buf(),
    };
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if std::fs::read_to_string(mount.mnt.join(".stats")).is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "mount did not become ready in 90s; log:\n{}",
            std::fs::read_to_string(log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    mount
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Poll `clients --json` until `pred` holds or the deadline passes;
/// returns the final report either way.
fn poll_clients(
    meta_uri: &str,
    deadline: Duration,
    pred: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let end = Instant::now() + deadline;
    loop {
        let out = run(&["clients", meta_uri, "--json"]);
        let v = stdout_json(&out, "clients --json");
        if pred(&v) || Instant::now() > end {
            return v;
        }
        // TEST-3: 100 ms, not 2 s. `clients --json` is a cheap offline
        // probe; a 2 s interval charged every miss two whole seconds of
        // wall clock for nothing.
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ===========================================================================
// F3 — the defrag verb: the VL1 fake was REMOVED; PR VL7 shipped the REAL
// one (§5.7). The stale-script pin survives: the fake verb's flag grammar
// (`--nvme-path`) still refuses loud, never a fake success.
// ===========================================================================

/// A stale script running the FAKE verb's grammar must still fail loudly
/// — the real VL7 verb has a different surface (`--data`/`--meta`/
/// `--fold`/`--rebalance`/`--report-only`), so `--nvme-path` is an
/// unknown-argument refusal, never a fake "Starting defragmentation…" +
/// exit 0.
#[test]
fn test_defrag_fake_grammar_still_fails_loud() {
    let out = run(&["defrag", "--nvme-path", "/nonexistent"]);
    assert!(
        !out.status.success(),
        "the fake defrag grammar must exit nonzero, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(
        stderr.contains("unexpected argument"),
        "the refusal must be the CLI's unknown-argument error (stale scripts \
         fail comprehensibly), got:\n{stderr}"
    );
}

/// `--help` advertises the REAL verb (PR VL7 — the AGENTS module map is
/// truthful again), and its own help names the five §5.7 modes.
#[test]
fn test_defrag_verb_listed_in_help_with_axis_modes() {
    let out = run(&["--help"]);
    let help = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        help.to_lowercase().contains("defrag"),
        "--help must list the VL7 defrag verb:\n{help}"
    );
    let out = run(&["defrag", "--help"]);
    let help = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    for flag in ["--data", "--meta", "--fold", "--rebalance", "--report-only"] {
        assert!(
            help.contains(flag),
            "defrag --help must name the {flag} mode:\n{help}"
        );
    }
}

// ===========================================================================
// F1 — `squeezefs clients` serves the real registration records.
// ===========================================================================

/// Classification is the EXISTING staleness law, offline-verifiable: plant
/// one fresh and one expired `client:{id}` registration (the exact records
/// a mount heartbeat maintains) and the verb must classify them live vs
/// stale — with id, pid, and heartbeat age surfaced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_clients_classifies_forged_live_and_stale_registrations() {
    let base = scratch("clients_forge");
    let (meta, _data) = format_volume(&base, 2 * GIB);

    // Plant registrations the way a mounted client's heartbeat writes them
    // (tests/mount_registration_tests.rs pattern), then shut down cleanly
    // (a clean shutdown removes the writer_claim but not foreign client
    // registrations — exactly the residue of other/crashed clients).
    let live_pid = std::process::id();
    let stale_ts = now_secs().saturating_sub(CLIENT_STALE_TTL_SECS + 120);
    {
        let be = KvMetaBackend::open(&meta).await.expect("open for forge");
        // `client:{id}` is an INTERNAL record (VAL-2): the generic
        // `Metadata` entry points refuse it, so plant it the way the
        // daemon's own heartbeat does — the backend-internal writer.
        be.setxattr_internal(
            1,
            "client:11111111-live",
            format!("{{\"ts\":{},\"pid\":{}}}", now_secs(), live_pid).as_bytes(),
        )
        .await
        .expect("plant live registration");
        be.setxattr_internal(
            1,
            "client:22222222-stale",
            format!("{{\"ts\":{},\"pid\":4242}}", stale_ts).as_bytes(),
        )
        .await
        .expect("plant stale registration");
        be.shutdown().await.expect("clean shutdown");
    }

    let uri = format!("sqmeta://{}", meta.display());
    let out = run(&["clients", &uri, "--json"]);
    let v = stdout_json(&out, "clients --json");

    let clients = v["clients"]
        .as_array()
        .expect("`clients` array in the JSON report");
    assert_eq!(
        clients.len(),
        2,
        "exactly the two planted registrations must be reported: {v}"
    );

    let by_id = |needle: &str| {
        clients
            .iter()
            .find(|c| c["id"].as_str().unwrap_or_default().contains(needle))
            .unwrap_or_else(|| panic!("registration '{needle}' missing from {v}"))
    };
    let live = by_id("11111111-live");
    assert_eq!(
        live["state"], "live",
        "fresh heartbeat classifies live: {v}"
    );
    assert_eq!(live["kind"], "client");
    assert_eq!(live["pid"], live_pid, "pid surfaced from the record: {v}");
    assert!(
        live["age_secs"].as_u64().expect("age_secs") <= CLIENT_STALE_TTL_SECS,
        "live entry carries a fresh age: {v}"
    );

    let stale = by_id("22222222-stale");
    assert_eq!(
        stale["state"], "stale",
        "expired heartbeat classifies stale (the format-preflight law): {v}"
    );
    assert_eq!(stale["pid"], 4242);
    assert!(
        stale["age_secs"].as_u64().expect("age_secs") > CLIENT_STALE_TTL_SECS,
        "stale entry carries its true age: {v}"
    );

    assert_eq!(v["live"], 1, "summary live count: {v}");
    assert_eq!(v["stale"], 1, "summary stale count: {v}");

    // Human output names the records too — never the hardcoded lie.
    let human = run(&["clients", &uri]);
    let text = String::from_utf8_lossy(&human.stdout).to_string();
    assert!(
        human.status.success() && text.contains("11111111-live") && text.contains("stale"),
        "human output must list the registrations with states:\n{text}"
    );
    assert!(
        !text.contains("No active clients connected."),
        "the hardcoded stub line must be gone:\n{text}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// A live mounted daemon must show up (client registration + writer claim,
/// correct pid, live state); a clean unmount must leave zero records.
#[test]
fn test_clients_lists_live_mount_then_zero_after_clean_unmount() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("clients_live");
    let (meta, _data) = format_volume(&base, 2 * GIB);
    let uri = format!("sqmeta://{}", meta.display());
    let mnt = base.join("mnt");
    let mut mount = spawn_mount(&meta, &mnt, &base.join("mount.log"));
    let daemon_pid = mount.daemon_pid() as u64;

    // The registration is written at FUSE init + refreshed by the
    // heartbeat; poll briefly for it.
    let v = poll_clients(&uri, Duration::from_secs(30), |v| {
        v["clients"]
            .as_array()
            .map(|c| {
                c.iter()
                    .any(|r| r["kind"] == "client" && r["state"] == "live")
            })
            .unwrap_or(false)
    });
    let clients = v["clients"].as_array().expect("clients array").clone();
    let live_client = clients
        .iter()
        .find(|r| r["kind"] == "client" && r["state"] == "live")
        .unwrap_or_else(|| panic!("a live mount must list a live client registration: {v}"));
    assert_eq!(
        live_client["pid"], daemon_pid,
        "the registration names the daemon pid: {v}"
    );
    let writer = clients
        .iter()
        .find(|r| r["kind"] == "writer")
        .unwrap_or_else(|| panic!("the single-writer claim must be listed: {v}"));
    assert_eq!(
        writer["pid"], daemon_pid,
        "the writer claim names the daemon pid: {v}"
    );
    assert_eq!(writer["state"], "live", "held claim is live: {v}");

    // `status` must serve the same records (the docs audit's second lying
    // surface: `"Clients": []` against a live mount).
    let status = run(&["status", &uri]);
    let sv = stdout_json(&status, "status");
    let sclients = sv["Clients"].as_array().expect("status Clients array");
    assert!(
        sclients
            .iter()
            .any(|r| r["kind"] == "client" && r["pid"] == daemon_pid),
        "status must report the live client registration, got: {sv}"
    );

    // Clean unmount: registration and claim are both removed.
    mount.unmount_clean();
    let after = run(&["clients", &uri, "--json"]);
    let av = stdout_json(&after, "clients --json after unmount");
    assert_eq!(
        av["clients"].as_array().map(Vec::len),
        Some(0),
        "a clean unmount must leave zero registrations: {av}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// kill -9'd daemon: its records must classify as NOT live — the writer
/// claim instantly (the mount guard's same-host dead-pid proof), the
/// client registration once its heartbeat ages past the TTL (the ONE
/// staleness law; kill -9 leaves a fresh timestamp behind).
#[test]
fn test_clients_kill9_daemon_classified_stale_not_active() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("clients_kill9");
    let (meta, _data) = format_volume(&base, 2 * GIB);
    let uri = format!("sqmeta://{}", meta.display());
    let mnt = base.join("mnt");
    let mut mount = spawn_mount(&meta, &mnt, &base.join("mount.log"));

    // Ensure the registration exists before the kill.
    poll_clients(&uri, Duration::from_secs(30), |v| {
        v["clients"]
            .as_array()
            .map(|c| !c.is_empty())
            .unwrap_or(false)
    });

    mount.kill9();

    // The writer claim carries pid+boot: the dead-pid proof classifies it
    // reclaimable ("dead") without any TTL wait.
    let quick = poll_clients(&uri, Duration::from_secs(20), |v| {
        v["clients"]
            .as_array()
            .map(|c| {
                c.iter()
                    .filter(|r| r["kind"] == "writer")
                    .all(|r| r["state"] == "dead")
            })
            .unwrap_or(false)
    });
    let writers: Vec<_> = quick["clients"]
        .as_array()
        .expect("clients array")
        .iter()
        .filter(|r| r["kind"] == "writer")
        .collect();
    assert!(
        !writers.is_empty() && writers.iter().all(|r| r["state"] == "dead"),
        "a kill -9'd same-host writer claim must classify dead (pid proof) \
         without waiting out the TTL: {quick}"
    );

    // The client registration has no boot scope: it goes stale by the TTL
    // (45s law + one heartbeat interval of slack).
    let settled = poll_clients(&uri, Duration::from_secs(90), |v| {
        v["clients"]
            .as_array()
            .map(|c| c.iter().all(|r| r["state"] != "live"))
            .unwrap_or(false)
    });
    let records = settled["clients"].as_array().expect("clients array");
    assert!(
        !records.is_empty(),
        "the crashed daemon's records must still be visible (they expire, \
         not vanish): {settled}"
    );
    assert!(
        records.iter().all(|r| r["state"] != "live"),
        "no record of a kill -9'd daemon may classify live after the TTL: {settled}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

// ===========================================================================
// F2 — `squeezefs df`: offline space/inode accounting from the volume set.
// ===========================================================================

/// Offline `df --json` against a freshly formatted (never mounted) volume:
/// totals reconcile with the formatted capacity, nothing is used, the
/// inode quota is the format default, and the schema is pinned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_df_offline_empty_volume_reconciles_capacity_and_schema() {
    let base = scratch("df_empty");
    let (meta, data) = format_volume(&base, 8 * GIB);
    let uri = format!("sqmeta://{}", meta.display());

    let out = run(&["df", "-g", &uri, "--json"]);
    let v = stdout_json(&out, "df --json");

    // ---- schema pin (top level) ----
    let obj = v.as_object().expect("df JSON is an object");
    for key in [
        "name",
        "block_size",
        "capacity_bytes",
        "used_bytes",
        "free_bytes",
        "inodes",
        "data_volumes",
        "meta_volumes",
    ] {
        assert!(
            obj.contains_key(key),
            "df JSON missing pinned key '{key}': {v}"
        );
    }
    for key in ["total", "used", "free"] {
        assert!(
            v["inodes"]
                .as_object()
                .expect("inodes object")
                .contains_key(key),
            "df JSON inodes missing pinned key '{key}': {v}"
        );
    }

    // ---- totals reconcile with the format ----
    assert_eq!(
        v["capacity_bytes"].as_u64(),
        Some(8 * GIB),
        "capacity must be the formatted capacity (summed physical): {v}"
    );
    assert_eq!(
        v["used_bytes"].as_u64(),
        Some(0),
        "empty volume uses nothing: {v}"
    );
    assert_eq!(
        v["free_bytes"].as_u64(),
        Some(8 * GIB),
        "empty volume free == capacity: {v}"
    );
    assert_eq!(
        v["inodes"]["total"].as_u64(),
        Some(1_000_000),
        "format quota: {v}"
    );
    assert_eq!(
        v["inodes"]["used"].as_u64(),
        Some(1),
        "fresh volume has exactly the root inode: {v}"
    );
    assert_eq!(
        v["inodes"]["free"].as_u64(),
        Some(1_000_000 - 1),
        "inode headroom = quota - used: {v}"
    );

    // ---- per-volume rows ----
    let dv = v["data_volumes"].as_array().expect("data_volumes array");
    assert_eq!(dv.len(), 1, "one data volume row: {v}");
    assert_eq!(dv[0]["path"].as_str(), Some(data.to_str().unwrap()));
    assert_eq!(dv[0]["size_bytes"].as_u64(), Some(8 * GIB));
    assert_eq!(dv[0]["allocated_bytes"].as_u64(), Some(0));

    let mv = v["meta_volumes"].as_array().expect("meta_volumes array");
    assert_eq!(mv.len(), 1, "one meta volume row: {v}");
    assert_eq!(mv[0]["path"].as_str(), Some(meta.to_str().unwrap()));
    assert_eq!(mv[0]["size_bytes"].as_u64(), Some(256 * MIB));
    let heap = mv[0]["kv_heap_bytes"].as_u64().expect("kv_heap_bytes");
    let heap_free = mv[0]["kv_heap_free_bytes"]
        .as_u64()
        .expect("kv_heap_free_bytes");
    let heap_used = mv[0]["kv_heap_used_bytes"]
        .as_u64()
        .expect("kv_heap_used_bytes");
    assert!(
        heap > 0 && heap <= 256 * MIB,
        "meta heap within the volume: {v}"
    );
    assert_eq!(heap_used + heap_free, heap, "heap used+free == heap: {v}");
    assert!(
        heap_free >= heap / 2,
        "a fresh meta volume keeps most of its heap free: {v}"
    );

    // ---- human output exists and is honest ----
    let human = run(&["df", "-g", &uri]);
    assert!(human.status.success(), "human df must succeed");
    let text = String::from_utf8_lossy(&human.stdout).to_string();
    assert!(
        !text.contains("offline") || text.contains("offline query"),
        "the stub line must be gone:\n{text}"
    );
    assert!(
        text.contains("8.00 GiB"),
        "human df names the formatted capacity:\n{text}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// Durable data moves the allocator: write 64 MiB through a real mount,
/// unmount cleanly, and OFFLINE df must show the allocator delta; delete
/// via a second mount and the space must return.
#[test]
fn test_df_reflects_allocator_delta_after_durable_write() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("df_delta");
    let (meta, _data) = format_volume(&base, 8 * GIB);
    let uri = format!("sqmeta://{}", meta.display());
    let mnt = base.join("mnt");

    {
        let mut mount = spawn_mount(&meta, &mnt, &base.join("mount.log"));
        use std::io::Write;
        let mut f = std::fs::File::create(mnt.join("df_probe.bin")).expect("create probe");
        let chunk = vec![0xA5u8; 4 * MIB as usize];
        for _ in 0..16 {
            f.write_all(&chunk).expect("write probe chunk");
        }
        f.sync_all().expect("fsync probe");
        drop(f);
        mount.unmount_clean();
    }

    let out = run(&["df", "-g", &uri, "--json"]);
    let v = stdout_json(&out, "df --json after write");
    let used = v["used_bytes"].as_u64().expect("used_bytes");
    assert!(
        used >= 64 * MIB,
        "64 MiB of durable striped data must show as allocated (got {used}): {v}"
    );
    assert!(
        used <= 64 * MIB + 16 * MIB,
        "allocated must be the ~16 chunks written, not runaway (got {used}): {v}"
    );
    assert_eq!(
        v["free_bytes"].as_u64(),
        Some(8 * GIB - used),
        "free == capacity - used: {v}"
    );
    assert!(
        v["inodes"]["used"].as_u64().expect("inodes.used") >= 2,
        "root + the probe file consumed inode watermark: {v}"
    );
    // The per-volume row carries the same delta (single data volume).
    assert_eq!(
        v["data_volumes"][0]["allocated_bytes"].as_u64(),
        Some(used),
        "single-volume aggregate == the volume row: {v}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// df is a read-only probe (the `status` access pattern): it must answer
/// against a LIVE-mounted volume set too — never blocked, never refusing.
#[test]
fn test_df_answers_against_live_mounted_volume() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("df_live");
    let (meta, _data) = format_volume(&base, 2 * GIB);
    let uri = format!("sqmeta://{}", meta.display());
    let mnt = base.join("mnt");
    let mut mount = spawn_mount(&meta, &mnt, &base.join("mount.log"));

    let out = run(&["df", "-g", &uri, "--json"]);
    let v = stdout_json(&out, "df --json against live mount");
    assert_eq!(
        v["capacity_bytes"].as_u64(),
        Some(2 * GIB),
        "df against a live mount serves the durable snapshot: {v}"
    );

    mount.unmount_clean();
    let _ = std::fs::remove_dir_all(&base);
}

/// An unformatted volume fails LOUD (never fake numbers, never exit 0).
#[test]
fn test_df_unformatted_volume_fails_loud() {
    let base = scratch("df_blank");
    let blank = base.join("blank.bin");
    std::fs::File::create(&blank)
        .expect("create blank")
        .set_len(64 * MIB)
        .expect("size blank");
    let out = run(&["df", "-g", &format!("sqmeta://{}", blank.display())]);
    assert!(
        !out.status.success(),
        "df on an unformatted volume must exit nonzero, got:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let err = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(
        err.contains("not formatted") || err.contains("format"),
        "the refusal must name the cause:\n{err}"
    );
    let _ = std::fs::remove_dir_all(&base);
}
