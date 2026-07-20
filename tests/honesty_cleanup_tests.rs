//! PR VL1 — honesty cleanup contracts (docs/design-volume-lifecycle.md
//! §5.0, gate G-VL-1).
//!
//! The fake admin surfaces must REFUSE loudly (exit nonzero, stderr
//! naming the successor verb and the design doc) — never print stub
//! success. The real surfaces that remain (`enable`/`disable` health
//! overrides, `list`, `set-cache-paths`) keep working, and the phantom
//! `meta_volume_0 → /dev/shm/squeezefs_pjdfs_meta` seed is gone from
//! the default runtime config.
//!
//! These tests spawn the built binary (`CARGO_BIN_EXE_squeezefs`) —
//! the refusal contract is CLI-visible behavior, so it is pinned at
//! the CLI. The suite runs under the gate's `--test-threads=1`; tests
//! that touch the process-global `/dev/shm/squeezefs_runtime_config.json`
//! save and restore it.

use std::process::Command;

const RUNTIME_CONFIG: &str = "/dev/shm/squeezefs_runtime_config.json";

/// Save/restore guard for the global runtime-config file.
struct RestoreConfig(Option<Vec<u8>>);
impl RestoreConfig {
    fn capture() -> Self {
        RestoreConfig(std::fs::read(RUNTIME_CONFIG).ok())
    }
}
impl Drop for RestoreConfig {
    fn drop(&mut self) {
        match &self.0 {
            Some(bytes) => {
                let _ = std::fs::write(RUNTIME_CONFIG, bytes);
            }
            None => {
                let _ = std::fs::remove_file(RUNTIME_CONFIG);
            }
        }
    }
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_squeezefs"))
        .args(args)
        .output()
        .expect("spawn squeezefs binary")
}

/// One removed verb: must exit nonzero and the error text must carry
/// the refusal marker, the successor verb, and the design-doc pointer.
fn assert_refusal(args: &[&str], successor: &str) {
    let out = run(args);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        !out.status.success(),
        "`squeezefs {}` must refuse (exit nonzero), got success with output: {combined}",
        args.join(" ")
    );
    assert!(
        combined.contains("removed"),
        "`squeezefs {}` refusal must say the verb was removed, got: {combined}",
        args.join(" ")
    );
    assert!(
        combined.contains(successor),
        "`squeezefs {}` refusal must name the successor `{successor}`, got: {combined}",
        args.join(" ")
    );
    assert!(
        combined.contains("design-volume-lifecycle"),
        "`squeezefs {}` refusal must cite the design doc, got: {combined}",
        args.join(" ")
    );
    // A refusal is not a stub success: none of the old success strings.
    assert!(
        !combined.contains("successfully"),
        "`squeezefs {}` must not print stub success, got: {combined}",
        args.join(" ")
    );
}

// ---------------------------------------------------------------------------
// removed verbs refuse loudly
// ---------------------------------------------------------------------------

#[test]
fn data_volume_add_remove_migrate_refuse() {
    assert_refusal(
        &[
            "config",
            "data-volume",
            "add",
            "v9",
            "--volume",
            "/dev/null",
        ],
        "volume add-data",
    );
    assert_refusal(
        &["config", "data-volume", "remove", "v9"],
        "volume remove-data",
    );
    assert_refusal(
        &["config", "data-volume", "migrate", "a", "b"],
        "volume remove-data",
    );
}

#[test]
fn metadata_volume_add_remove_migrate_refuse() {
    assert_refusal(
        &[
            "config",
            "metadata-volume",
            "add",
            "m9",
            "--volume",
            "/dev/null",
        ],
        "volume add-meta",
    );
    assert_refusal(
        &["config", "metadata-volume", "remove", "m9"],
        "volume remove-meta",
    );
    assert_refusal(
        &["config", "metadata-volume", "migrate", "a", "b"],
        "volume remove-meta",
    );
}

#[test]
fn disk_cache_stub_verbs_refuse() {
    for verb in ["add", "remove", "enable", "disable", "flush"] {
        assert_refusal(
            &["config", "disk-cache", verb, "/tmp/nowhere"],
            "config set-cache-paths",
        );
    }
}

#[test]
fn config_fsck_stub_refuses() {
    let out = run(&["config", "fsck"]);
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "`config fsck` must refuse, got success: {combined}"
    );
    assert!(
        combined.contains("removed") && combined.contains("squeezefs fsck"),
        "`config fsck` refusal must name the successor `squeezefs fsck`: {combined}"
    );
    assert!(
        !combined.contains("No consistency issues"),
        "the lying always-clean fsck output must be gone: {combined}"
    );
}

// ---------------------------------------------------------------------------
// kept surfaces stay real
// ---------------------------------------------------------------------------

#[test]
fn enable_disable_health_overrides_still_work() {
    let _g = RestoreConfig::capture();
    let out = run(&["config", "data-volume", "disable", "vX"]);
    assert!(
        out.status.success(),
        "disable is the kept fail-stop health override: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = run(&["config", "data-volume", "enable", "vX"]);
    assert!(out.status.success());
    let out = run(&["config", "metadata-volume", "disable", "meta_volume_1"]);
    assert!(out.status.success());
    let out = run(&["config", "metadata-volume", "enable", "meta_volume_1"]);
    assert!(out.status.success());
}

#[test]
fn phantom_meta_volume_seed_is_gone() {
    let _g = RestoreConfig::capture();
    std::fs::remove_file(RUNTIME_CONFIG).ok();
    let out = run(&["config", "list"]);
    assert!(out.status.success(), "config list stays");
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("config list prints JSON");
    assert_eq!(
        v["metadata_volumes"]
            .as_object()
            .map(|o| o.len())
            .unwrap_or(usize::MAX),
        0,
        "default config must not seed the phantom meta_volume_0: {v}"
    );
    assert!(
        !out.stdout
            .windows(b"squeezefs_pjdfs_meta".len())
            .any(|w| w == b"squeezefs_pjdfs_meta"),
        "the phantom /dev/shm/squeezefs_pjdfs_meta path must be gone"
    );
    // The removed redirections bookkeeping does not resurface.
    assert!(
        v.get("metadata_volume_redirections").is_none(),
        "redirections field (sole writer was the fake migrate) must be gone: {v}"
    );
}
