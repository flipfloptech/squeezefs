//! PR VL2 — the durable job fabric, red-first
//! (docs/design-volume-lifecycle.md §5.1, gate G-VL-7):
//!
//! - **Reserved-xattr namespace screen** (§5.1.2, KD-2): `job:` records
//!   and every `user.squeezefs.*` internal record live on ino 1 behind
//!   the FUSE layer — invisible to `listxattr`, `EPERM` on get/set/
//!   remove through FUSE (the daemon/probe paths read them via the meta
//!   backend directly). This also CLOSES a pre-existing hole: before
//!   the screen, any user could `removexattr` the format config through
//!   the mount.
//! - **Durable job records** (KD-2): `job:{id}` / shard / progress
//!   records ride v3 whole-tx xattr commits on ino 1; visible to
//!   offline probes; re-scanned at fabric start (crash-resume by plan
//!   regeneration — progress is advisory, correctness is re-planning).
//! - **Throttle law** (KD-3): per-worker duty cycle, a live-retunable
//!   job-record field; adherence ±10 % at 25/50/75 % (G-VL-7).
//! - **Control**: pause/resume/cancel/live-rethrottle; terminal states
//!   are durable.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::jobs::{JobFabric, JobSpec, JobState, JobType, JOB_XATTR_PREFIX};
use squeezefs::meta_backend::Metadata;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use std::ffi::OsStr;
use std::sync::Arc;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile};

const ROOT: u64 = 1;

fn req() -> Request {
    Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 4321,
    }
}

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
            format_config_xattr: Some(b"{\"name\":\"jobfab\"}".to_vec()),
        },
    )
    .await
    .expect("format v3 meta volume");
    squeezefs::meta_backend::kv::backend::KvMetaBackend::open(path)
        .await
        .expect("open v3 meta volume")
}

struct Fx {
    fs: Arc<SqueezefsFilesystem>,
    meta: Arc<squeezefs::meta_backend::RoutedMetaBackend>,
    _meta_file: NamedTempFile,
    _backing: NamedTempFile,
    _staging: tempfile::TempDir,
}

async fn fixture(tag: &str) -> Fx {
    let dlm = DlmClient::new("local").unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(16 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(backing.path().to_str().unwrap()));
    let alloc = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), tag)
            .await
            .expect("allocator"),
    );
    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("16MB"),
        Some("16MB"),
        Some("64MB"),
        Some("64MB"),
        dlm.meta_client().clone(),
        alloc.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, alloc, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    let meta_file = NamedTempFile::new().unwrap();
    let kv = open_v3_meta(meta_file.path(), 256 * 1024 * 1024).await;
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    Fx {
        fs: Arc::new(fs),
        meta: routed,
        _meta_file: meta_file,
        _backing: backing,
        _staging: staging,
    }
}

// ---------------------------------------------------------------------------
// §5.1.2 reserved-xattr namespace screen
// ---------------------------------------------------------------------------

const FORMAT_CONFIG: &str = "user.squeezefs.format_config";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reserved_names_are_invisible_and_eperm_through_fuse() {
    let fx = fixture("screen-basics").await;

    for name in [
        "job:abc123",
        "job:abc123:shard:0",
        FORMAT_CONFIG,
        "user.squeezefs.future",
    ] {
        // setxattr refused (EPERM, not EINVAL — the name is valid, the
        // namespace is reserved).
        let e = fx
            .fs
            .setxattr(req(), ROOT, OsStr::new(name), b"forged", 0, 0)
            .await
            .expect_err("reserved setxattr must refuse");
        assert_eq!(e, libc::EPERM.into(), "setxattr({name}) must be EPERM");

        // getxattr refused (reserved bytes never serve through FUSE).
        let e = fx
            .fs
            .getxattr(req(), ROOT, OsStr::new(name), 4096)
            .await
            .expect_err("reserved getxattr must refuse");
        assert_eq!(e, libc::EPERM.into(), "getxattr({name}) must be EPERM");

        // removexattr refused — the pre-existing tamper hole.
        let e = fx
            .fs
            .removexattr(req(), ROOT, OsStr::new(name))
            .await
            .expect_err("reserved removexattr must refuse");
        assert_eq!(e, libc::EPERM.into(), "removexattr({name}) must be EPERM");
    }

    // Plain user xattrs keep working on the same inode.
    fx.fs
        .setxattr(req(), ROOT, OsStr::new("user.plain"), b"ok", 0, 0)
        .await
        .expect("plain xattr must still work");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reserved_names_filtered_from_listxattr() {
    let fx = fixture("screen-list").await;

    // Seed a job record + rely on the format-config record the format
    // wrote — both live on ino 1 via the BACKEND path (how the daemon
    // itself writes them; the FUSE screen must not block the backend).
    fx.meta
        .setxattr(ROOT, "job:deadbeef", b"{\"schema\":1}")
        .await
        .expect("backend writes job records directly");
    fx.fs
        .setxattr(req(), ROOT, OsStr::new("user.visible"), b"v", 0, 0)
        .await
        .expect("plain xattr");

    let reply = fx.fs.listxattr(req(), ROOT, 4096).await.expect("listxattr");
    let names = match reply {
        fuse3::raw::reply::ReplyXAttr::Data(d) => String::from_utf8_lossy(&d).into_owned(),
        other => panic!("expected Data, got {other:?}"),
    };
    assert!(
        names.contains("user.visible"),
        "plain names stay listed: {names}"
    );
    assert!(
        !names.contains("job:"),
        "job records must be invisible via FUSE: {names}"
    );
    assert!(
        !names.contains("user.squeezefs"),
        "internal records must be invisible via FUSE: {names}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn format_config_removexattr_regression_hole_is_closed() {
    let fx = fixture("screen-hole").await;
    // Before VL2 this SUCCEEDED — deleting the durable format config
    // from an unprivileged shell.
    let e = fx
        .fs
        .removexattr(req(), ROOT, OsStr::new(FORMAT_CONFIG))
        .await
        .expect_err("format-config removexattr must refuse");
    assert_eq!(e, libc::EPERM.into());
    // The record is intact through the backend.
    let v = fx
        .meta
        .getxattr(ROOT, FORMAT_CONFIG)
        .await
        .expect("backend read")
        .expect("format config still present");
    assert!(!v.is_empty());
}

// ---------------------------------------------------------------------------
// KD-2 durable job records + crash-resume; KD-3 throttle; control verbs
// ---------------------------------------------------------------------------

/// Fabric fixture: a JobFabric over the fixture's meta backend with a
/// small local pool.
async fn fabric(fx: &Fx, workers: usize) -> Arc<JobFabric> {
    JobFabric::start(fx.meta.clone(), workers, 100, None)
        .await
        .expect("fabric start")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn noop_job_runs_to_completion_and_records_are_durable() {
    let fx = fixture("fab-noop").await;
    let fab = fabric(&fx, 2).await;

    let job_id = fab
        .submit(JobSpec {
            job_type: JobType::Noop {
                tasks: 64,
                task_ms: 1,
            },
            throttle_pct: 100,
        })
        .await
        .expect("submit");

    fab.wait_terminal(&job_id, Duration::from_secs(30))
        .await
        .expect("job reaches a terminal state");
    let status = fab
        .status(&job_id)
        .await
        .expect("status")
        .expect("known job");
    assert_eq!(status.state, JobState::Completed);
    assert_eq!(status.tasks_done, 64);

    // Durability: the record is a real ino-1 xattr readable through the
    // BACKEND (offline-probe shape), invisible through FUSE (screen).
    let raw = fx
        .meta
        .getxattr(ROOT, &format!("{JOB_XATTR_PREFIX}{job_id}"))
        .await
        .expect("backend read")
        .expect("job record persisted");
    let rec: serde_json::Value = serde_json::from_slice(&raw).expect("job record is JSON");
    assert_eq!(rec["schema"], 1, "schema-versioned record: {rec}");
    assert_eq!(rec["state"], "completed", "terminal state durable: {rec}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn throttle_duty_cycle_adheres_within_ten_percent() {
    // G-VL-7 leg: measured duty cycle (task-active / wall) within ±10
    // points of target at 25/50/75 %. Noop tasks with a known active
    // time make duty cycle = tasks×task_ms / wall directly measurable.
    let fx = fixture("fab-throttle").await;
    let fab = fabric(&fx, 1).await;

    for pct in [25u32, 50, 75] {
        let tasks = 40u64;
        let task_ms = 20u64;
        let started = std::time::Instant::now();
        let job_id = fab
            .submit(JobSpec {
                job_type: JobType::Noop { tasks, task_ms },
                throttle_pct: pct,
            })
            .await
            .expect("submit");
        fab.wait_terminal(&job_id, Duration::from_secs(60))
            .await
            .expect("terminal");
        let wall = started.elapsed().as_secs_f64();
        let active = (tasks * task_ms) as f64 / 1000.0;
        let duty = 100.0 * active / wall;
        assert!(
            (duty - pct as f64).abs() <= 10.0,
            "duty cycle {duty:.1}% must be within ±10 of {pct}% (wall {wall:.2}s, active {active:.2}s)"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_resume_cancel_and_live_rethrottle() {
    let fx = fixture("fab-ctl").await;
    let fab = fabric(&fx, 1).await;

    // Long job so control verbs land mid-flight.
    let job_id = fab
        .submit(JobSpec {
            job_type: JobType::Noop {
                tasks: 10_000,
                task_ms: 5,
            },
            throttle_pct: 100,
        })
        .await
        .expect("submit");

    // Pause: progress stops advancing.
    fab.pause(&job_id).await.expect("pause");
    let s1 = fab.status(&job_id).await.unwrap().expect("known");
    assert_eq!(s1.state, JobState::Paused);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let s2 = fab.status(&job_id).await.unwrap().expect("known");
    assert_eq!(
        s1.tasks_done, s2.tasks_done,
        "paused job must not advance ({} -> {})",
        s1.tasks_done, s2.tasks_done
    );

    // Live rethrottle while paused persists into the record.
    fab.throttle(&job_id, 25).await.expect("throttle");
    let s = fab.status(&job_id).await.unwrap().expect("known");
    assert_eq!(s.throttle_pct, 25, "rethrottle is live");

    // Resume: advances again.
    fab.resume(&job_id).await.expect("resume");
    let before = fab.status(&job_id).await.unwrap().unwrap().tasks_done;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = fab.status(&job_id).await.unwrap().unwrap().tasks_done;
    assert!(
        after > before,
        "resumed job must advance ({before} -> {after})"
    );

    // Cancel: terminal, durable.
    fab.cancel(&job_id).await.expect("cancel");
    fab.wait_terminal(&job_id, Duration::from_secs(10))
        .await
        .expect("terminal");
    let s = fab.status(&job_id).await.unwrap().expect("known");
    assert_eq!(s.state, JobState::Cancelled);
    let raw = fx
        .meta
        .getxattr(ROOT, &format!("{JOB_XATTR_PREFIX}{job_id}"))
        .await
        .unwrap()
        .expect("record");
    let rec: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(rec["state"], "cancelled", "terminal state durable: {rec}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_resume_reconstructs_from_durable_records() {
    // KD-6 crash law at fabric level: a fabric that dies mid-job (here:
    // dropped without terminal state) leaves durable records a NEW
    // fabric instance re-scans at start — the job resumes by plan
    // regeneration and completes.
    let fx = fixture("fab-resume").await;
    let job_id;
    {
        let fab = fabric(&fx, 1).await;
        job_id = fab
            .submit(JobSpec {
                job_type: JobType::Noop {
                    tasks: 5_000,
                    task_ms: 2,
                },
                throttle_pct: 100,
            })
            .await
            .expect("submit");
        // Let it make some progress, then "crash" (drop the fabric —
        // workers abort mid-task; records stay non-terminal).
        tokio::time::sleep(Duration::from_millis(150)).await;
        fab.shutdown_abrupt().await;
    }

    // The durable record survived, non-terminal.
    let raw = fx
        .meta
        .getxattr(ROOT, &format!("{JOB_XATTR_PREFIX}{job_id}"))
        .await
        .unwrap()
        .expect("record survives the crash");
    let rec: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_ne!(rec["state"], "completed", "must not be terminal yet: {rec}");

    // A new fabric adopts and completes it.
    let fab2 = fabric(&fx, 2).await;
    fab2.wait_terminal(&job_id, Duration::from_secs(60))
        .await
        .expect("adopted job completes");
    let s = fab2
        .status(&job_id)
        .await
        .unwrap()
        .expect("known after resume");
    assert_eq!(s.state, JobState::Completed);
    assert_eq!(
        s.tasks_done, 5_000,
        "plan regeneration finishes the full plan"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn job_list_is_offline_probe_shaped() {
    // `squeezefs job list` reads job: records straight off the meta
    // volume (the clients/df access pattern) — the fabric's list API
    // over the backend must see everything the records carry.
    let fx = fixture("fab-list").await;
    let fab = fabric(&fx, 1).await;
    let a = fab
        .submit(JobSpec {
            job_type: JobType::Noop {
                tasks: 4,
                task_ms: 1,
            },
            throttle_pct: 100,
        })
        .await
        .unwrap();
    fab.wait_terminal(&a, Duration::from_secs(10))
        .await
        .unwrap();

    let listed = JobFabric::list_records(&fx.meta)
        .await
        .expect("list records");
    assert!(
        listed
            .iter()
            .any(|r| r.job_id == a && r.state == JobState::Completed),
        "offline-shaped list must carry the completed job: {listed:?}"
    );
}

// ---------------------------------------------------------------------------
// §5.1.4 admin lane (KD-4): control-plane sessions on the IPC host
// ---------------------------------------------------------------------------

use squeezefs::ipc_host::{abstract_connect, recv_ctl, send_ctl, IpcHost, IpcHostConfig};
use squeezefs::ipc_service::FabricAdminSink;
use squeezefs_ipc::layout::Geometry;
use squeezefs_ipc::wire::CtlMsg;

fn admin_host_cfg(name: &str, data_plane: bool, owner_uid: u32) -> IpcHostConfig {
    IpcHostConfig {
        socket_name: format!("sqz-il0-jobadmin-{}-{}", std::process::id(), name),
        socket_dir: None,
        build_commit: "b".repeat(40),
        allow_dev: false,
        geometry: Geometry {
            ring_entries: 16,
            slots: 16,
            arena_bytes: 1024 * 1024,
            max_op_bytes: 64 * 1024,
            _pad: 0,
        },
        arena_cap_bytes: 16 * 1024 * 1024,
        per_uid_session_cap: 8,
        idle_secs: 0,
        data_plane,
        owner_uid,
    }
}

/// Refusing data-plane sink: control-plane-only hosts must never reach it.
struct NoDataPlane;
impl squeezefs::ipc_host::SessionSink for NoDataPlane {
    fn serve_data(
        &self,
        _op: squeezefs::ipc_host::DataOp,
        _completion: squeezefs::ipc_host::SlotCompletion,
    ) {
        panic!("control-plane-only host must never dispatch data ops");
    }
}

fn admin_hello(sock: &std::os::unix::net::UnixStream) -> CtlMsg {
    // SAFETY: getpid/getuid are trivially safe.
    let (pid, uid) = unsafe { (libc::getpid() as u32, libc::getuid()) };
    send_ctl(sock, &CtlMsg::AdminHello { pid, uid }, None).expect("send AdminHello");
    let (reply, fd) = recv_ctl(sock).expect("recv AdminHello reply");
    assert!(fd.is_none(), "admin replies carry no fd");
    reply
}

fn admin_req(sock: &std::os::unix::net::UnixStream, verb: &str, arg: &str) -> (bool, String) {
    send_ctl(
        sock,
        &CtlMsg::AdminReq {
            verb: verb.to_string(),
            arg: arg.to_string(),
        },
        None,
    )
    .expect("send AdminReq");
    let (reply, _) = recv_ctl(sock).expect("recv AdminReply");
    match reply {
        CtlMsg::AdminReply { ok, body } => (ok, body),
        other => panic!("expected AdminReply, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_lane_controls_the_fabric_over_the_ctl_socket() {
    let fx = fixture("admin-lane").await;
    let fab = fabric(&fx, 1).await;

    // Control-plane-only host (the every-mount posture): data-plane
    // HELLOs refuse; ADMIN sessions from the owning uid work.
    // SAFETY: getuid is trivially safe.
    let owner = unsafe { libc::getuid() };
    let cfg = admin_host_cfg("ctl", false, owner);
    let host = IpcHost::spawn(cfg.clone(), Arc::new(NoDataPlane)).expect("host");
    host.set_admin_sink(Arc::new(FabricAdminSink::new(fab.clone())));

    // A data-plane HELLO on a control-plane-only host refuses (Flags).
    let sock = abstract_connect(&cfg.socket_name).expect("connect");
    // SAFETY: getpid/getuid trivially safe.
    let (pid, uid) = unsafe { (libc::getpid() as u32, libc::getuid()) };
    send_ctl(
        &sock,
        &CtlMsg::Hello {
            abi: squeezefs_ipc::layout::IPC_ABI,
            pid,
            uid,
            build_commit: cfg.build_commit.clone(),
            nonce: host.current_nonce(),
        },
        None,
    )
    .expect("send");
    // (No fd credential attached — a control-plane host must refuse
    // before ever screening one.)
    let (reply, _) = recv_ctl(&sock).expect("recv");
    assert!(
        matches!(reply, CtlMsg::Refuse { .. }),
        "data-plane HELLO must refuse on a control-plane-only mount, got {reply:?}"
    );
    drop(sock);

    // ADMIN session: hello → ok → job verbs round-trip.
    let sock = abstract_connect(&cfg.socket_name).expect("connect");
    let reply = admin_hello(&sock);
    assert!(
        matches!(reply, CtlMsg::AdminOk),
        "owner-uid AdminHello must be admitted, got {reply:?}"
    );

    let job_id = fab
        .submit(JobSpec {
            job_type: JobType::Noop {
                tasks: 20_000,
                task_ms: 2,
            },
            throttle_pct: 100,
        })
        .await
        .expect("submit");

    let (ok, body) = admin_req(&sock, "job-list", "");
    assert!(ok, "job-list must succeed: {body}");
    assert!(body.contains(&job_id), "list carries the live job: {body}");

    let (ok, _) = admin_req(&sock, "job-pause", &job_id);
    assert!(ok, "pause over the lane");
    let (ok, body) = admin_req(&sock, "job-status", &job_id);
    assert!(ok && body.contains("paused"), "status shows paused: {body}");

    let (ok, _) = admin_req(&sock, "job-throttle", &format!("{job_id} 25"));
    assert!(ok, "throttle over the lane");
    let (ok, body) = admin_req(&sock, "job-status", &job_id);
    assert!(
        ok && body.contains("25"),
        "status shows the new throttle: {body}"
    );

    let (ok, _) = admin_req(&sock, "job-resume", &job_id);
    assert!(ok, "resume over the lane");
    let (ok, _) = admin_req(&sock, "job-cancel", &job_id);
    assert!(ok, "cancel over the lane");
    fab.wait_terminal(&job_id, Duration::from_secs(10))
        .await
        .expect("terminal after cancel");

    // Unknown verb: a refusal, not a hang or a lie.
    let (ok, body) = admin_req(&sock, "job-frobnicate", "x");
    assert!(!ok, "unknown verb refuses: {body}");
    host.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_lane_refuses_foreign_uids() {
    let fx = fixture("admin-uid").await;
    let fab = fabric(&fx, 1).await;
    // Owner uid deliberately NOT ours (and not root when running as a
    // user): our peercred uid must be refused.
    // SAFETY: getuid trivially safe.
    let my_uid = unsafe { libc::getuid() };
    if my_uid == 0 {
        // Running as root: root is always admitted by design — the
        // foreign-uid refusal cannot be observed from here.
        return;
    }
    let cfg = admin_host_cfg("uidgate", false, my_uid.wrapping_add(12345));
    let host = IpcHost::spawn(cfg.clone(), Arc::new(NoDataPlane)).expect("host");
    host.set_admin_sink(Arc::new(FabricAdminSink::new(fab)));

    let sock = abstract_connect(&cfg.socket_name).expect("connect");
    let reply = admin_hello(&sock);
    assert!(
        matches!(reply, CtlMsg::Refuse { .. }),
        "foreign-uid AdminHello must refuse, got {reply:?}"
    );
    host.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_req_without_admin_hello_refuses() {
    let fx = fixture("admin-nohello").await;
    let fab = fabric(&fx, 1).await;
    // SAFETY: getuid trivially safe.
    let owner = unsafe { libc::getuid() };
    let cfg = admin_host_cfg("nohello", false, owner);
    let host = IpcHost::spawn(cfg.clone(), Arc::new(NoDataPlane)).expect("host");
    host.set_admin_sink(Arc::new(FabricAdminSink::new(fab)));

    let sock = abstract_connect(&cfg.socket_name).expect("connect");
    send_ctl(
        &sock,
        &CtlMsg::AdminReq {
            verb: "job-list".into(),
            arg: String::new(),
        },
        None,
    )
    .expect("send");
    let (reply, _) = recv_ctl(&sock).expect("recv");
    assert!(
        matches!(reply, CtlMsg::Refuse { .. }),
        "AdminReq before AdminHello must refuse, got {reply:?}"
    );
    host.shutdown();
}

// ---------------------------------------------------------------------------
// POSIX ACL posture (fstests generic/099 + generic/319 repro-port, VL10
// release gate): SqueezeFS does not IMPLEMENT POSIX ACL semantics (no
// mode↔ACL_USER_OBJ/mask sync, no default-ACL inheritance, no
// enforcement beyond mode bits) — so STORING the ACL xattrs was a lie
// the kernel and every tool believed (`ls` showed '+', modes diverged
// from the effective ACL, generic/099's Permission-denied legs executed
// freely). The honest posture: `system.posix_acl_*` refuses ENOTSUP —
// tools and fstests then classify the filesystem as no-ACL (setfacl
// fails loud, `_require_acls` notruns) instead of trusting fabricated
// semantics.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn posix_acl_xattrs_refuse_enotsup() {
    let fx = fixture("acl-posture").await;

    for name in ["system.posix_acl_access", "system.posix_acl_default"] {
        let e = fx
            .fs
            .setxattr(req(), ROOT, OsStr::new(name), b"\x02\x00\x00\x00", 0, 0)
            .await
            .expect_err("ACL setxattr must refuse — ACL semantics are not implemented");
        assert_eq!(
            e,
            libc::EOPNOTSUPP.into(),
            "setxattr({name}) must be ENOTSUP (the no-ACL filesystem class)"
        );

        let e = fx
            .fs
            .getxattr(req(), ROOT, OsStr::new(name), 4096)
            .await
            .expect_err("ACL getxattr must refuse");
        assert_eq!(e, libc::EOPNOTSUPP.into(), "getxattr({name})");

        let e = fx
            .fs
            .removexattr(req(), ROOT, OsStr::new(name))
            .await
            .expect_err("ACL removexattr must refuse");
        assert_eq!(e, libc::EOPNOTSUPP.into(), "removexattr({name})");
    }

    // Non-ACL system.* names are untouched by this posture (trusted.*/
    // security.* etc. keep their existing behavior).
    fx.fs
        .setxattr(req(), ROOT, OsStr::new("user.beside"), b"ok", 0, 0)
        .await
        .expect("plain xattrs unaffected");
}

/// fstests generic/533 repro-port (VL10 release gate): removing an
/// ABSENT xattr must fail ENODATA ("No such attribute" — Linux ENOATTR),
/// never ENOENT ("No such file or directory" — that names the FILE,
/// which exists). The backend's absent-xattr arm surfaced generic
/// NotFound, which the errno map rendered ENOENT.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removexattr_of_absent_attr_is_enodata() {
    let fx = fixture("xattr-enodata").await;
    let e = fx
        .fs
        .removexattr(req(), ROOT, OsStr::new("user.never_existed"))
        .await
        .expect_err("absent xattr removal must fail");
    assert_eq!(
        e,
        libc::ENODATA.into(),
        "absent xattr => ENODATA/ENOATTR, never ENOENT"
    );

    // A genuinely absent INODE still fails (the kernel resolves paths
    // before this handler ever runs, so the exact errno for a raced-away
    // nodeid is not load-bearing — failing loud is).
    fx.fs
        .removexattr(req(), 999_999_999, OsStr::new("user.x"))
        .await
        .expect_err("absent inode must fail");
}
