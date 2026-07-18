/*
 * JuiceFS, Copyright 2026 Juicedata, Inc.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Mock-mode tests of the transitional (pre-rebuild) NVMe-oF verb paths,
//! adjusted by PR 1 (N1) to the share-ledger wiring
//! (`docs/design-nvmeof-target-management.md` §6.4 + the PR 1
//! transitional contract): share/unshare ride the write-ahead intent
//! API, unshare dispatch and the loop-device association come from the
//! ledger, and `restore-shares` replays/reconciles ledger records for
//! nvmet. The registry-persistence test died with the self-truncating
//! registry it covered. The `SQUEEZEFS_MOCK_NVMEOF*` env forks these
//! tests ride are themselves transitional — the zero-mock fidelity tier
//! replaces them as the stacks are rebuilt (N2+).

use once_cell::sync::Lazy;
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use tempfile::tempdir;

use squeezefs::nvmeof::ledger::Ledger;
use squeezefs::nvmeof::stack::{Listener, ShareRecord, ShareState};
use squeezefs::nvmeof::StackKind;

static TEST_MUTEX: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

fn backing_path_to_str(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// Point the ledger at a per-test state dir (the §6.8-sanctioned
/// relocation seam) — never the production /var/lib path.
fn set_state_dir(dir: &Path) {
    std::env::set_var("SQUEEZEFS_NVMEOF_STATE_DIR", dir);
}

#[tokio::test]
async fn test_nvmeof_target_and_initiator_mock_lifecycle() {
    let _guard = TEST_MUTEX.lock().unwrap();
    // 1. Set environment to mock mode
    std::env::set_var("SQUEEZEFS_MOCK_NVMEOF", "1");
    std::env::set_var("SQUEEZEFS_MOCK_NVMEOF_DIR", "/tmp/squeezefs_nvmet");
    std::env::set_var(
        "SQUEEZEFS_MOCK_NVMEOF_FABRICS_DIR",
        "/tmp/squeezefs_nvme_fabrics",
    );
    std::env::set_var("SQUEEZEFS_MOCK_NVMEOF_NVME_DIR", "/tmp/squeezefs_nvme");
    let state_dir = tempdir().unwrap();
    set_state_dir(state_dir.path());

    // Clean up mock target directories from any previous runs
    let mock_configfs = Path::new("/tmp/squeezefs_nvmet");
    let mock_fabrics = Path::new("/tmp/squeezefs_nvme_fabrics");
    let mock_nvme = Path::new("/tmp/squeezefs_nvme");

    let _ = fs::remove_dir_all(mock_configfs);
    let _ = fs::remove_dir_all(mock_fabrics);
    let _ = fs::remove_dir_all(mock_nvme);

    let temp_dir = tempdir().unwrap();
    let backing_file = temp_dir.path().join("backing_disk.img");

    // 2. Share backing file as an NVMe-oF target
    let subnqn = squeezefs::nvmeof::share_target(
        backing_path_to_str(&backing_file),
        None,
        4420,
        &["127.0.0.1".to_string()],
    )
    .expect("Share target should succeed");

    assert!(subnqn.starts_with("nqn.2026-06.io.squeezefs:subsystem-"));

    // The share rode the write-ahead intent API: the finalized record is
    // active, on the nvmet stack, with the loop association recorded in
    // the ledger (law 5 — never configfs) and the N1-era optional fields
    // null (the transitional field-presence contract).
    let ledger = Ledger::open_default();
    let record = ledger
        .find(&subnqn)
        .expect("ledger load")
        .expect("share must be ledgered");
    assert_eq!(record.state, ShareState::Active);
    assert_eq!(record.stack, StackKind::Nvmet);
    assert_eq!(record.loop_device.as_deref(), Some("/dev/loop99"));
    assert_eq!(record.listeners.len(), 1);
    assert_eq!(record.listeners[0].ip, "127.0.0.1");
    assert_eq!(record.listeners[0].port, 4420);
    assert_eq!(record.listeners[0].nvmet_port_id, None);
    assert_eq!(record.nsid, None);
    assert_eq!(record.ns_uuid, None);
    assert_eq!(record.bdev_name, None);
    assert_eq!(record.ptpl_file, None);

    // Sharing the same backing again must refuse loud via the ledger
    // duplicate guard (the old registry guard never fired as root — its
    // own loads truncated it first).
    let err = squeezefs::nvmeof::share_target(
        backing_path_to_str(&backing_file),
        None,
        4420,
        &["127.0.0.1".to_string()],
    )
    .expect_err("duplicate backing must refuse");
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert!(
        err.to_string().contains(&subnqn),
        "refusal must name the holder: {err}"
    );

    // Share a second target on the same port/IP to verify configfs port reuse
    let backing_file2 = temp_dir.path().join("backing_disk2.img");
    let subnqn2 = squeezefs::nvmeof::share_target(
        backing_path_to_str(&backing_file2),
        None,
        4420,
        &["127.0.0.1".to_string()],
    )
    .expect("Sharing second target on same port/IP should succeed");

    assert!(subnqn2.starts_with("nqn.2026-06.io.squeezefs:subsystem-"));

    // Verify configfs mock structures
    let ns1_dir = mock_configfs
        .join("subsystems")
        .join(&subnqn)
        .join("namespaces")
        .join("1");
    assert!(ns1_dir.exists());

    let device_path = fs::read_to_string(ns1_dir.join("device_path")).unwrap();
    assert!(device_path.contains("loop"));

    let enable = fs::read_to_string(ns1_dir.join("enable")).unwrap();
    assert_eq!(enable.trim(), "1");

    let port_addr =
        fs::read_to_string(mock_configfs.join("ports").join("1").join("addr_traddr")).unwrap();
    assert_eq!(port_addr.trim(), "127.0.0.1");

    // Link check
    let link_path = mock_configfs
        .join("ports")
        .join("1")
        .join("subsystems")
        .join(&subnqn);
    assert!(link_path.exists());

    let link_path2 = mock_configfs
        .join("ports")
        .join("1")
        .join("subsystems")
        .join(&subnqn2);
    assert!(link_path2.exists());

    // 3. Connect to the target
    let dev = squeezefs::nvmeof::connect_target("127.0.0.1", 4420, &subnqn)
        .expect("Connect target should succeed");

    assert_eq!(dev, "/dev/nvme0n1");

    // Verify fabrics ctl write
    let ctl_write = fs::read_to_string(mock_fabrics.join("ctl")).unwrap();
    assert!(ctl_write.contains(&subnqn));
    assert!(ctl_write.contains("127.0.0.1"));

    // 4. List targets/initiators
    let list_res = squeezefs::nvmeof::list_nvmeof();
    assert!(list_res.is_ok());

    // 5. Disconnect target
    squeezefs::nvmeof::disconnect_target(&subnqn).expect("Disconnect target should succeed");

    // Verify controller deletion (the nvme0 dir should no longer exist if mocked)
    // Wait, disconnect_target writes "1" to delete_controller, it does not delete the nvme0 dir.
    let del_ctrl = fs::read_to_string(mock_nvme.join("nvme0").join("delete_controller")).unwrap();
    assert_eq!(del_ctrl.trim(), "1");

    // 6. Unshare targets (ledger-dispatched — no --spdk hint needed)
    squeezefs::nvmeof::unshare_target(&subnqn, false).expect("Unshare target should succeed");
    squeezefs::nvmeof::unshare_target(&subnqn2, false).expect("Unshare target 2 should succeed");

    // Verify subsystems/ports cleanup
    assert!(!mock_configfs.join("subsystems").join(&subnqn).exists());
    assert!(!mock_configfs.join("subsystems").join(&subnqn2).exists());
    assert!(!link_path.exists());
    assert!(!link_path2.exists());

    // Law 6 completed: teardown deleted the records.
    assert!(ledger.find(&subnqn).expect("ledger load").is_none());
    assert!(ledger.find(&subnqn2).expect("ledger load").is_none());

    // An unledgered NQN refuses NotFound (nothing left to guess from).
    let err = squeezefs::nvmeof::unshare_target(&subnqn, false)
        .expect_err("unshare of a gone share must fail");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);

    // Clean up paths
    let _ = fs::remove_dir_all(mock_configfs);
    let _ = fs::remove_dir_all(mock_fabrics);
    let _ = fs::remove_dir_all(mock_nvme);
}

/// `restore-shares` replays the ledger for nvmet through the pre-rebuild
/// configfs path and reconciles the §6.4 law-6 intent states: active +
/// gone ⇒ re-shared (loop bookkeeping refreshed), pending + live ⇒
/// finalized, pending + no live objects ⇒ garbage-collected, removing ⇒
/// teardown resumed; SPDK records are kept but not replayed at N1.
#[test]
fn test_nvmeof_ledger_restore_reconciles_intents() {
    let _guard = TEST_MUTEX.lock().unwrap();
    std::env::set_var("SQUEEZEFS_MOCK_NVMEOF", "1");
    std::env::set_var("SQUEEZEFS_MOCK_NVMEOF_DIR", "/tmp/squeezefs_nvmet_restore");
    std::env::set_var(
        "SQUEEZEFS_MOCK_NVMEOF_FABRICS_DIR",
        "/tmp/squeezefs_nvme_fabrics_restore",
    );
    std::env::set_var(
        "SQUEEZEFS_MOCK_NVMEOF_NVME_DIR",
        "/tmp/squeezefs_nvme_restore",
    );
    let state_dir = tempdir().unwrap();
    set_state_dir(state_dir.path());

    let mock_configfs = Path::new("/tmp/squeezefs_nvmet_restore");
    let _ = fs::remove_dir_all(mock_configfs);

    let temp_dir = tempdir().unwrap();
    let ledger = Ledger::open_default();

    // (1) A normally-shared target…
    let backing_a = temp_dir.path().join("restore_a.img");
    let nqn_a = squeezefs::nvmeof::share_target(
        backing_path_to_str(&backing_a),
        Some("nqn.test:restore-a"),
        4420,
        &["127.0.0.1".to_string()],
    )
    .expect("share a");
    assert!(mock_configfs.join("subsystems").join(&nqn_a).exists());

    // (4-prep) …and a second one whose unshare will be interrupted.
    let backing_d = temp_dir.path().join("restore_d.img");
    let nqn_d = squeezefs::nvmeof::share_target(
        backing_path_to_str(&backing_d),
        Some("nqn.test:restore-d"),
        4420,
        &["127.0.0.1".to_string()],
    )
    .expect("share d");

    // Simulate the reboot/target restart: configfs state vanishes, the
    // ledger (persistent state dir) survives.
    let _ = fs::remove_dir_all(mock_configfs);

    // (2) A crash-window share that never completed: pending intent, no
    // live objects.
    let pending_gc = ShareRecord {
        subnqn: "nqn.test:restore-pending-gc".to_string(),
        stack: StackKind::Nvmet,
        state: ShareState::Pending,
        backing_path: "/dev/mock-pending-gc".to_string(),
        backing_canonical: "/dev/mock-pending-gc".to_string(),
        nsid: None,
        ns_uuid: None,
        listeners: vec![Listener {
            ip: "127.0.0.1".to_string(),
            port: 4420,
            nvmet_port_id: None,
        }],
        bdev_name: None,
        ptpl_file: None,
        loop_device: None,
        created_utc: "2026-07-17T00:00:00Z".to_string(),
        allow_hosts: Vec::new(),
        adopted_from: None,
    };
    ledger.begin_share(&pending_gc).expect("begin pending-gc");

    // (3) A crash-window share whose mutations all completed (live
    // objects exist) but the finalize flip never ran.
    let mut pending_live = pending_gc.clone();
    pending_live.subnqn = "nqn.test:restore-pending-live".to_string();
    pending_live.backing_path = "/dev/mock-pending-live".to_string();
    pending_live.backing_canonical = "/dev/mock-pending-live".to_string();
    ledger
        .begin_share(&pending_live)
        .expect("begin pending-live");
    fs::create_dir_all(
        mock_configfs
            .join("subsystems")
            .join("nqn.test:restore-pending-live"),
    )
    .expect("hand-build live objects for the pending-live case");

    // (4) The interrupted unshare: removing intent.
    ledger.mark_removing(&nqn_d).expect("mark d removing");

    // (5) An SPDK-stack record: kept, never replayed at N1.
    let nqn_e = squeezefs::nvmeof::share_target_spdk(
        backing_path_to_str(&temp_dir.path().join("restore_e.img")),
        Some("nqn.test:restore-e-spdk"),
        4420,
        &["127.0.0.1".to_string()],
    )
    .expect("share e via spdk (mock rpc)");

    // Replay + reconcile.
    squeezefs::nvmeof::restore_shares().expect("restore-shares");

    // (1) active + gone ⇒ re-shared through the pre-rebuild configfs
    // path, still active, loop bookkeeping refreshed in the ledger.
    assert!(
        mock_configfs.join("subsystems").join(&nqn_a).exists(),
        "active record must be re-shared after the wipe"
    );
    let rec_a = ledger.find(&nqn_a).expect("load").expect("a ledgered");
    assert_eq!(rec_a.state, ShareState::Active);
    assert_eq!(rec_a.loop_device.as_deref(), Some("/dev/loop99"));

    // (2) pending + no live objects ⇒ garbage-collected.
    assert!(
        ledger
            .find("nqn.test:restore-pending-gc")
            .expect("load")
            .is_none(),
        "pending intent with no live objects must be GC'd by restore"
    );

    // (3) pending + live objects ⇒ finalized active.
    assert_eq!(
        ledger
            .find("nqn.test:restore-pending-live")
            .expect("load")
            .expect("pending-live record kept")
            .state,
        ShareState::Active,
        "pending intent whose live objects exist must be finalized"
    );

    // (4) removing ⇒ teardown resumed, record deleted, objects gone.
    assert!(ledger.find(&nqn_d).expect("load").is_none());
    assert!(!mock_configfs.join("subsystems").join(&nqn_d).exists());

    // (5) SPDK record kept for ownership/dispatch, untouched by replay.
    assert_eq!(
        ledger
            .find(&nqn_e)
            .expect("load")
            .expect("spdk record kept")
            .state,
        ShareState::Active
    );

    // Idempotency (law 4): a second restore over already-live shares is
    // a verified no-op and still succeeds.
    squeezefs::nvmeof::restore_shares().expect("restore-shares is idempotent");

    let _ = fs::remove_dir_all(mock_configfs);
    let _ = fs::remove_dir_all("/tmp/squeezefs_nvme_fabrics_restore");
    let _ = fs::remove_dir_all("/tmp/squeezefs_nvme_restore");
}

#[tokio::test]
async fn test_nvmeof_multirail_mock_connect() {
    let _guard = TEST_MUTEX.lock().unwrap();
    // 1. Set environment to mock mode
    std::env::set_var("SQUEEZEFS_MOCK_NVMEOF", "1");
    std::env::set_var("SQUEEZEFS_MOCK_NVMEOF_DIR", "/tmp/squeezefs_nvmet_mr");
    std::env::set_var(
        "SQUEEZEFS_MOCK_NVMEOF_FABRICS_DIR",
        "/tmp/squeezefs_nvme_fabrics_mr",
    );
    std::env::set_var("SQUEEZEFS_MOCK_NVMEOF_NVME_DIR", "/tmp/squeezefs_nvme_mr");
    let state_dir = tempdir().unwrap();
    set_state_dir(state_dir.path());

    let mock_configfs = Path::new("/tmp/squeezefs_nvmet_mr");
    let mock_fabrics = Path::new("/tmp/squeezefs_nvme_fabrics_mr");
    let mock_nvme = Path::new("/tmp/squeezefs_nvme_mr");

    let _ = fs::remove_dir_all(mock_configfs);
    let _ = fs::remove_dir_all(mock_fabrics);
    let _ = fs::remove_dir_all(mock_nvme);

    let temp_dir = tempdir().unwrap();
    let backing_file = temp_dir.path().join("backing_disk_mr.img");

    // 2. Share backing file as target
    let subnqn = squeezefs::nvmeof::share_target(
        backing_path_to_str(&backing_file),
        None,
        4420,
        &["127.0.0.1".to_string()],
    )
    .expect("Share target should succeed");

    // 3. Connect target
    let dev = squeezefs::nvmeof::connect_target("127.0.0.1", 4420, &subnqn)
        .expect("Connect target should succeed");

    assert_eq!(dev, "/dev/nvme0n1");

    // Verify fabrics ctl write has been performed
    let ctl_write = fs::read_to_string(mock_fabrics.join("ctl")).unwrap();
    assert!(ctl_write.contains(&subnqn));
    assert!(ctl_write.contains("127.0.0.1"));

    // Verify that mock controller nvme0 is created
    assert!(mock_nvme.join("nvme0").exists());

    // 4. Disconnect target
    squeezefs::nvmeof::disconnect_target(&subnqn).expect("Disconnect target should succeed");

    // 5. Unshare target
    squeezefs::nvmeof::unshare_target(&subnqn, false).expect("Unshare target should succeed");

    // Clean up paths
    let _ = fs::remove_dir_all(mock_configfs);
    let _ = fs::remove_dir_all(mock_fabrics);
    let _ = fs::remove_dir_all(mock_nvme);
}

/// The SPDK share records through the intent API and — the bug the
/// registry truncation made permanent — `unshare` WITHOUT the `--spdk`
/// flag dispatches to the SPDK stack because the ledger says so.
#[test]
fn test_nvmeof_spdk_target_mock_lifecycle() {
    let _guard = TEST_MUTEX.lock().unwrap();
    std::env::set_var("SQUEEZEFS_MOCK_NVMEOF", "1");
    let state_dir = tempdir().unwrap();
    set_state_dir(state_dir.path());
    let temp_dir = tempdir().unwrap();
    let backing = temp_dir.path().join("spdk_backing.img");

    // 1. Share via SPDK
    let subnqn = squeezefs::nvmeof::share_target_spdk(
        backing_path_to_str(&backing),
        None,
        4420,
        &["127.0.0.1".to_string()],
    )
    .expect("Share SPDK target should succeed in mock mode");

    assert!(subnqn.starts_with("nqn.2026-06.io.squeezefs:spdk-subsystem-"));

    // The record is active on the spdk stack; the N1-era SPDK-only
    // fields stay null — the pre-rebuild path stamps/pins nothing
    // (nsid/UUID/ptpl_file pinning lands at N4).
    let ledger = Ledger::open_default();
    let record = ledger
        .find(&subnqn)
        .expect("ledger load")
        .expect("spdk share must be ledgered");
    assert_eq!(record.stack, StackKind::Spdk);
    assert_eq!(record.state, ShareState::Active);
    assert_eq!(record.nsid, None);
    assert_eq!(record.ns_uuid, None);
    assert_eq!(record.bdev_name, None);
    assert_eq!(record.ptpl_file, None);
    assert_eq!(record.loop_device, None);

    // 2. Duplicate backing via the OTHER stack refuses (cross-stack
    // duplicate-backing guard is a ledger job, §6.4).
    let err = squeezefs::nvmeof::share_target(
        backing_path_to_str(&backing),
        None,
        4420,
        &["127.0.0.1".to_string()],
    )
    .expect_err("cross-stack duplicate backing must refuse");
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);

    // 3. Unshare via the plain dispatcher (no --spdk flag): the ledger
    // resolves the stack. (Pre-N1 this fell into the configfs branch and
    // failed NotFound, because the registry truncated itself.)
    squeezefs::nvmeof::unshare_target(&subnqn, false)
        .expect("Unsharing SPDK target via ledger dispatch should succeed");
    assert!(ledger.find(&subnqn).expect("ledger load").is_none());
}

#[test]
fn test_nvmeof_spdk_helpers_mock() {
    let _guard = TEST_MUTEX.lock().unwrap();
    std::env::set_var("SQUEEZEFS_MOCK_NVMEOF", "1");

    assert!(squeezefs::nvmeof::spdk_install().is_ok());
    assert!(squeezefs::nvmeof::spdk_setup(1024).is_ok());
    assert!(squeezefs::nvmeof::spdk_bind("0000:01:00.0").is_ok());
    assert!(squeezefs::nvmeof::spdk_unbind("0000:01:00.0").is_ok());
    assert!(squeezefs::nvmeof::spdk_start().is_ok());
}
