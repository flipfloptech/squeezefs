//! VAL-7c: the ADMIN lane must enforce the SAME screening ladder the data
//! plane does (pre-RC spec §3 item VAL-7c).
//!
//! Before this: `AdminHello` carried only `(pid, uid)`. The lane checked
//! `SO_PEERCRED` identity correctly but skipped the KD-7 ABI/build-commit
//! equality gate and the anti-replay nonce the data plane enforces — so a
//! mismatched-build `squeezefs` CLI (or a replayed frame) reached the
//! mutating admin verbs (`job-cancel`, `volume-add-data`, health
//! overrides) on a live mount. `owner_uid` additionally derived from
//! `SUDO_UID`, which is caller-controlled environment, with no way for the
//! operator to state the administering identity explicitly.
//!
//! The ladder is now version → nonce → peercred, mirroring
//! `handle_hello_msg`, and `mount -o admin_uid=N` states the identity
//! explicitly (VAL-4/VAL-5 built the data-lane half of this; this is the
//! same ladder applied to the control lane).

use squeezefs::ipc_host::{
    abstract_connect, recv_ctl, send_ctl, AdminSink, IpcHost, IpcHostConfig,
};
use squeezefs_ipc::layout::Geometry;
use squeezefs_ipc::wire::{CtlMsg, RefuseClass, NONCE_LEN};
use std::sync::Arc;

struct EchoAdmin;
impl AdminSink for EchoAdmin {
    fn handle(&self, verb: &str, _arg: &str) -> (bool, String) {
        (true, format!("served {verb}"))
    }
}

fn cfg(name: &str, owner_uid: u32) -> IpcHostConfig {
    IpcHostConfig {
        socket_name: format!("sqz-il0-val7c-{}-{}", std::process::id(), name),
        socket_dir: None,
        build_commit: "c".repeat(40),
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
        data_plane: false,
        owner_uid,
    }
}

fn me() -> (u32, u32) {
    // SAFETY: getpid/getuid are trivially safe.
    unsafe { (libc::getpid() as u32, libc::getuid()) }
}

/// The full ladder, positive leg: a matched-build, fresh-nonce, correct-
/// peercred AdminHello is admitted and its verbs round-trip.
#[test]
fn admin_hello_with_the_full_ladder_is_admitted() {
    let c = cfg("ok", me().1);
    let host = IpcHost::spawn(c.clone(), Arc::new(squeezefs::ipc_host::EchoSessionSink))
        .expect("spawn host");
    host.set_admin_sink(Arc::new(EchoAdmin));

    let sock = abstract_connect(&c.socket_name).expect("connect");
    let (pid, uid) = me();
    send_ctl(
        &sock,
        &CtlMsg::AdminHello {
            abi: squeezefs_ipc::layout::IPC_ABI,
            pid,
            uid,
            build_commit: c.build_commit.clone(),
            nonce: host.current_nonce(),
        },
        None,
    )
    .expect("send AdminHello");
    let (reply, _) = recv_ctl(&sock).expect("recv");
    assert!(
        matches!(reply, CtlMsg::AdminOk),
        "a fully-screened AdminHello must be admitted, got {reply:?}"
    );
    host.shutdown();
}

/// KD-7 skew gate on the ADMIN lane: a build-commit mismatch refuses
/// `Version` — the admin verbs mutate durable job/volume state through
/// structures that are version-locked to the build.
#[test]
fn admin_hello_refuses_build_commit_skew() {
    let c = cfg("skew", me().1);
    let host = IpcHost::spawn(c.clone(), Arc::new(squeezefs::ipc_host::EchoSessionSink))
        .expect("spawn host");
    host.set_admin_sink(Arc::new(EchoAdmin));

    let sock = abstract_connect(&c.socket_name).expect("connect");
    let (pid, uid) = me();
    send_ctl(
        &sock,
        &CtlMsg::AdminHello {
            abi: squeezefs_ipc::layout::IPC_ABI,
            pid,
            uid,
            build_commit: "d".repeat(40),
            nonce: host.current_nonce(),
        },
        None,
    )
    .expect("send AdminHello");
    let (reply, _) = recv_ctl(&sock).expect("recv");
    assert!(
        matches!(
            reply,
            CtlMsg::Refuse {
                class: RefuseClass::Version
            }
        ),
        "mismatched build-commit must refuse Version, got {reply:?}"
    );

    // Same for a bad ABI.
    let sock = abstract_connect(&c.socket_name).expect("connect");
    send_ctl(
        &sock,
        &CtlMsg::AdminHello {
            abi: squeezefs_ipc::layout::IPC_ABI ^ 0xffff,
            pid,
            uid,
            build_commit: c.build_commit.clone(),
            nonce: host.current_nonce(),
        },
        None,
    )
    .expect("send AdminHello");
    let (reply, _) = recv_ctl(&sock).expect("recv");
    assert!(
        matches!(
            reply,
            CtlMsg::Refuse {
                class: RefuseClass::Version
            }
        ),
        "mismatched ABI must refuse Version, got {reply:?}"
    );
    host.shutdown();
}

/// Anti-replay: a stale/unknown nonce refuses `Nonce`. The bootstrap
/// xattr is the fresh-nonce source the CLI already reads, so an honest
/// admin client always has one.
#[test]
fn admin_hello_refuses_a_stale_nonce() {
    let c = cfg("nonce", me().1);
    let host = IpcHost::spawn(c.clone(), Arc::new(squeezefs::ipc_host::EchoSessionSink))
        .expect("spawn host");
    host.set_admin_sink(Arc::new(EchoAdmin));

    let sock = abstract_connect(&c.socket_name).expect("connect");
    let (pid, uid) = me();
    send_ctl(
        &sock,
        &CtlMsg::AdminHello {
            abi: squeezefs_ipc::layout::IPC_ABI,
            pid,
            uid,
            build_commit: c.build_commit.clone(),
            nonce: [0x5a; NONCE_LEN],
        },
        None,
    )
    .expect("send AdminHello");
    let (reply, _) = recv_ctl(&sock).expect("recv");
    assert!(
        matches!(
            reply,
            CtlMsg::Refuse {
                class: RefuseClass::Nonce
            }
        ),
        "an unknown nonce must refuse Nonce, got {reply:?}"
    );
    host.shutdown();
}

/// The peercred check is unchanged (it was already correct): a claimed
/// identity contradicting the kernel's refuses even with a perfect
/// version + nonce.
#[test]
fn admin_hello_still_refuses_a_lying_peercred_claim() {
    let c = cfg("peercred", me().1);
    let host = IpcHost::spawn(c.clone(), Arc::new(squeezefs::ipc_host::EchoSessionSink))
        .expect("spawn host");
    host.set_admin_sink(Arc::new(EchoAdmin));

    let sock = abstract_connect(&c.socket_name).expect("connect");
    let (pid, uid) = me();
    send_ctl(
        &sock,
        &CtlMsg::AdminHello {
            abi: squeezefs_ipc::layout::IPC_ABI,
            pid: pid.wrapping_add(1),
            uid,
            build_commit: c.build_commit.clone(),
            nonce: host.current_nonce(),
        },
        None,
    )
    .expect("send AdminHello");
    let (reply, _) = recv_ctl(&sock).expect("recv");
    assert!(
        matches!(
            reply,
            CtlMsg::Refuse {
                class: RefuseClass::Peercred
            }
        ),
        "a lying pid claim must refuse Peercred, got {reply:?}"
    );
    host.shutdown();
}

/// The explicit admin-uid surface: `-o admin_uid=N` states the
/// administering identity instead of inheriting it from `SUDO_UID`
/// (caller-controlled environment). Absent the option the resolution
/// falls back to the invoking owner — the shipped behavior, unchanged.
#[test]
fn admin_uid_mount_option_wins_over_the_environment() {
    use squeezefs::fuse_client::admin_uid_from_options;
    assert_eq!(
        admin_uid_from_options(Some("rw,admin_uid=4242,noatime"), 1000),
        4242,
        "an explicit -o admin_uid must win verbatim"
    );
    assert_eq!(
        admin_uid_from_options(Some("rw,noatime"), 1000),
        1000,
        "without the option the invoking owner is the fallback"
    );
    assert_eq!(
        admin_uid_from_options(None, 7),
        7,
        "no option string at all keeps the fallback"
    );
    assert_eq!(
        admin_uid_from_options(Some("admin_uid=notanumber"), 1000),
        1000,
        "an unparseable value must not silently become uid 0"
    );
    assert_eq!(
        admin_uid_from_options(Some("admin_uid=0"), 1000),
        0,
        "root may be named explicitly"
    );
}
