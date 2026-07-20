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
//! the CLI.
//!
//! PR VL3 update: the `/dev/shm/squeezefs_runtime_config.json` mechanism
//! was DELETED — the kept `enable`/`disable` health overrides now ride
//! the admin lane on live mounts and the guarded DURABLE record path
//! offline (`-g sqmeta://…`); the fail-stop EIO semantics are unchanged
//! and pinned below. `config list` (which printed the ephemeral file)
//! is now itself a loud refusal naming its successors.

use std::process::Command;

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
    // VL3: `disk-cache list` printed the always-empty ephemeral table off
    // the deleted /dev/shm runtime config — refusal now too.
    assert_refusal(&["config", "disk-cache", "list"], "config get-cache-paths");
}

#[test]
fn config_set_quota_stub_refuses() {
    // set_config_quota is a silent no-op today — a fake-success verb.
    // Quotas are format-time until the lifecycle capacity machinery
    // lands; the refusal names both.
    assert_refusal(&["config", "set", "capacity", "100G"], "squeezefs format");
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

/// Format a tiny file-backed set (1 meta, 2 data) for the offline
/// durable-override tests; returns (tempdir, sqmeta URI).
fn format_tiny_set() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let meta = dir.path().join("meta1");
    let oss1 = dir.path().join("oss1");
    let oss2 = dir.path().join("oss2");
    for (p, len) in [
        (&meta, 256u64 << 20),
        (&oss1, 256 << 20),
        (&oss2, 256 << 20),
    ] {
        std::fs::File::create(p).unwrap().set_len(len).unwrap();
    }
    let uri = format!("sqmeta://{}", meta.display());
    let out = run(&[
        "format",
        &uri,
        &format!("sqdata://{},{}", oss1.display(), oss2.display()),
        "--force",
    ]);
    assert!(
        out.status.success(),
        "format failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (dir, uri)
}

#[test]
fn enable_disable_health_overrides_still_work() {
    // The kept fail-stop overrides, re-homed (VL3): `-g sqmeta://…`
    // flips DURABLE record state through the guarded offline path; the
    // EIO fail-stop semantics text is unchanged.
    let (_dir, uri) = format_tiny_set();

    let out = run(&["config", "-g", &uri, "data-volume", "disable", "oss2"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "disable is the kept fail-stop health override: {combined}"
    );
    assert!(
        combined.contains("EIO") && combined.contains("not an evacuation"),
        "the fail-stop EIO semantics stay documented in the verb output: {combined}"
    );

    // The state is DURABLE: the list probe shows it.
    let out = run(&["config", "-g", &uri, "data-volume", "list"]);
    assert!(out.status.success());
    let rows: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("data-volume list prints JSON");
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "oss2")
        .expect("oss2 row");
    assert_eq!(row["state"], "disabled", "durable state: {rows}");

    let out = run(&["config", "-g", &uri, "data-volume", "enable", "oss2"]);
    assert!(out.status.success());

    // Unknown ids refuse (no ephemeral file to accumulate poison entries).
    let out = run(&["config", "-g", &uri, "data-volume", "disable", "vX"]);
    assert!(
        !out.status.success(),
        "unknown volume ids must refuse on the durable path"
    );

    // Metadata-volume overrides are runtime-only (live admin lane) until
    // VL5a's durable records — the offline target refuses LOUD, naming
    // the successor machinery, instead of faking durability.
    let out = run(&[
        "config",
        "-g",
        &uri,
        "metadata-volume",
        "disable",
        "meta_volume_0",
    ]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "offline metadata-volume disable must refuse (runtime-only): {combined}"
    );
    assert!(
        combined.contains("live mount") && combined.contains("VL5a"),
        "the refusal must name the live-mount lane and the VL5a successor: {combined}"
    );
}

#[test]
fn config_list_removed_and_phantom_seed_gone() {
    // VL3: `config list` printed the ephemeral /dev/shm runtime config —
    // the file mechanism is deleted, the verb refuses naming successors,
    // and the phantom seeds are structurally gone.
    let probe = std::path::Path::new("/dev/shm/squeezefs_runtime_config.json");
    let existed_before = probe.exists(); // stale from an old binary — not ours to delete
    let out = run(&["config", "list"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "`config list` must refuse (the runtime-config file is deleted): {combined}"
    );
    assert!(
        combined.contains("removed") && combined.contains("volume list"),
        "the refusal must name the successor `squeezefs volume list`: {combined}"
    );
    assert!(
        !combined.contains("squeezefs_pjdfs_meta"),
        "the phantom /dev/shm/squeezefs_pjdfs_meta path must be gone"
    );
    // The deleted mechanism's file must not be recreated by the binary.
    if !existed_before {
        assert!(
            !probe.exists(),
            "no verb may recreate the deleted /dev/shm runtime-config file"
        );
    }
}
