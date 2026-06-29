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

use once_cell::sync::Lazy;
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use tempfile::tempdir;

static TEST_MUTEX: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

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
        "127.0.0.1",
    )
    .expect("Share target should succeed");

    assert!(subnqn.starts_with("nqn.2026-06.io.squeezefs:subsystem-"));

    // Share a second target on the same port/IP to verify configfs port reuse
    let backing_file2 = temp_dir.path().join("backing_disk2.img");
    let subnqn2 = squeezefs::nvmeof::share_target(
        backing_path_to_str(&backing_file2),
        None,
        4420,
        "127.0.0.1",
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

    // 6. Unshare target
    squeezefs::nvmeof::unshare_target(&subnqn).expect("Unshare target should succeed");
    squeezefs::nvmeof::unshare_target(&subnqn2).expect("Unshare target 2 should succeed");

    // Verify subsystems/ports cleanup
    assert!(!mock_configfs.join("subsystems").join(&subnqn).exists());
    assert!(!mock_configfs.join("subsystems").join(&subnqn2).exists());
    assert!(!link_path.exists());
    assert!(!link_path2.exists());

    // Clean up paths
    let _ = fs::remove_dir_all(mock_configfs);
    let _ = fs::remove_dir_all(mock_fabrics);
    let _ = fs::remove_dir_all(mock_nvme);
}

fn backing_path_to_str(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn test_nvmeof_target_share_persistence() {
    let _guard = TEST_MUTEX.lock().unwrap();
    let mock_configfs = Path::new("/tmp/squeezefs_nvmet_persist");
    let mock_fabrics = Path::new("/tmp/squeezefs_nvme_fabrics_persist");
    let mock_nvme = Path::new("/tmp/squeezefs_nvme_persist");

    // Clean up any stale paths
    let _ = fs::remove_dir_all(mock_configfs);
    let _ = fs::remove_dir_all(mock_fabrics);
    let _ = fs::remove_dir_all(mock_nvme);

    fs::create_dir_all(mock_configfs).unwrap();
    fs::create_dir_all(mock_fabrics).unwrap();
    fs::create_dir_all(mock_nvme).unwrap();

    fs::write(mock_fabrics.join("ctl"), "").unwrap();

    std::env::set_var("SQUEEZEFS_MOCK_NVMEOF", "1");
    std::env::set_var("SQUEEZEFS_TEST_ENV", "1");
    std::env::set_var("SQUEEZEFS_MOCK_NVMEOF_DIR", "/tmp/squeezefs_nvmet_persist");
    std::env::set_var(
        "SQUEEZEFS_MOCK_NVMEOF_FABRICS_DIR",
        "/tmp/squeezefs_nvme_fabrics_persist",
    );
    std::env::set_var(
        "SQUEEZEFS_MOCK_NVMEOF_NVME_DIR",
        "/tmp/squeezefs_nvme_persist",
    );
    let temp_config_file = "/tmp/squeezefs_nvmeof_shares_test.json";
    let _ = fs::remove_file(temp_config_file);

    // Register a mock share
    squeezefs::nvmeof::register_share(
        "/tmp/test_persist_backing.img",
        "nqn.test-subsystem-1",
        4420,
        "127.0.0.1",
    )
    .unwrap();

    // Check restore
    let res = squeezefs::nvmeof::restore_shares();
    assert!(res.is_ok());

    // Clean up
    let _ = fs::remove_dir_all(mock_configfs);
    let _ = fs::remove_dir_all(mock_fabrics);
    let _ = fs::remove_dir_all(mock_nvme);
    let _ = fs::remove_file(temp_config_file);
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
        "127.0.0.1",
    )
    .expect("Share target should succeed");

    // 3. Connect target using multiple local IPs
    use std::net::IpAddr;
    let local_ips = vec![
        "127.0.0.1".parse::<IpAddr>().unwrap(),
        "127.0.0.2".parse::<IpAddr>().unwrap(),
    ];
    let dev =
        squeezefs::nvmeof::connect_target_with_local_ips("127.0.0.1", 4420, &subnqn, &local_ips)
            .expect("Multi-rail connect target should succeed");

    assert_eq!(dev, "/dev/nvme0n1");

    // Verify fabrics ctl write has been performed for both
    let ctl_write = fs::read_to_string(mock_fabrics.join("ctl")).unwrap();
    assert!(ctl_write.contains(&subnqn));
    assert!(ctl_write.contains("127.0.0.1"));
    assert!(ctl_write.contains("host_traddr=127.0.0.1"));
    assert!(ctl_write.contains("host_traddr=127.0.0.2"));

    // Verify that both mock controllers are created (nvme0 and nvme1)
    assert!(mock_nvme.join("nvme0").exists());
    assert!(mock_nvme.join("nvme1").exists());

    // 4. Disconnect target
    squeezefs::nvmeof::disconnect_target(&subnqn).expect("Disconnect target should succeed");

    // 5. Unshare target
    squeezefs::nvmeof::unshare_target(&subnqn).expect("Unshare target should succeed");

    // Clean up paths
    let _ = fs::remove_dir_all(mock_configfs);
    let _ = fs::remove_dir_all(mock_fabrics);
    let _ = fs::remove_dir_all(mock_nvme);
}
