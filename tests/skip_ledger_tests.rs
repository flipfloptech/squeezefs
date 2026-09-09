//! **TEST-2** (`docs/pre-rc-engineering-spec.md` §11): the skip ledger
//! and the require-mount mode, pinned red-first.
//!
//! The bug this suite exists to prevent from returning: sixteen test
//! files each carried a private copy of an environment ladder that
//! `eprintln!`'d and `return`ed, so
//!
//! * a box without `/dev/fuse` / `fuse.enable_uring` / `fusermount3`
//!   reported **all green** with the entire live-mount surface — twelve
//!   files, the product's core mechanism — unexecuted, and
//! * libtest captured the notices, so the run was externally
//!   indistinguishable from one that actually mounted.
//!
//! The contracts pinned here:
//!
//! 1. Every skip emits **one machine-readable ledger line** — on an
//!    uncaptured stderr handle and, when `SQUEEZEFS_TEST_SKIP_LEDGER`
//!    names a file, appended there.
//! 2. `SQUEEZEFS_TEST_REQUIRE_MOUNT=1` turns a mount-class skip into a
//!    **failure** (and still ledgers it first, marked `required`).
//! 3. `SQUEEZEFS_TEST_REQUIRE_ALL=1` does the same for every class.
//! 4. **No test file may re-invent a private skip** — the tree-wide audit
//!    below fails on a bare `[SKIP]`-and-return that does not route
//!    through the testkit. This is what stops a 15th file.
//!
//! The require-mode legs run the *real* gate in a child process (the
//! panic is the observable), because a require-mode env var set in-process
//! would poison every other test in this binary.

use std::path::{Path, PathBuf};
use std::process::Command;

use squeezefs_testkit::{self as testkit, site, SkipClass};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn scratch(tag: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME set");
    let base = PathBuf::from(home)
        .join("tmp")
        .join(format!("sqfs_skipledger_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create scratch dir");
    base
}

// ---------------------------------------------------------------------------
// 1. the ledger
// ---------------------------------------------------------------------------

#[test]
fn declared_skip_appends_one_json_line_to_the_ledger() {
    let base = scratch("emit");
    let ledger = base.join("skips.jsonl");
    // Scoped to this test: `declare` reads the var at call time, and the
    // class used here (OptIn) is never required by the gate scripts.
    std::env::set_var("SQUEEZEFS_TEST_SKIP_LEDGER", &ledger);
    let skipped = testkit::declare(site!(), SkipClass::OptIn, "pinning the ledger shape");
    std::env::remove_var("SQUEEZEFS_TEST_SKIP_LEDGER");
    assert!(!skipped, "declare always reports 'do not run'");

    let body = std::fs::read_to_string(&ledger).expect("ledger file written");
    let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "exactly one record per skip: {body}");
    let rec = lines[0];
    for needle in [
        r#""class":"opt-in""#,
        r#""bin":"skip_ledger_tests""#,
        r#""required":false"#,
        r#""reason":"pinning the ledger shape""#,
    ] {
        assert!(
            rec.contains(needle),
            "ledger record missing {needle}: {rec}"
        );
    }
    assert!(
        rec.contains("declared_skip_appends_one_json_line_to_the_ledger"),
        "the record must name the TEST that skipped, not the testkit: {rec}"
    );
    assert!(
        rec.starts_with('{') && rec.ends_with('}'),
        "one JSON object per line (jq -s / diff-able): {rec}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn ledger_records_escape_embedded_quotes_and_newlines() {
    let base = scratch("escape");
    let ledger = base.join("skips.jsonl");
    std::env::set_var("SQUEEZEFS_TEST_SKIP_LEDGER", &ledger);
    let _ = testkit::declare(site!(), SkipClass::OptIn, "a \"quoted\"\nreason\\here");
    std::env::remove_var("SQUEEZEFS_TEST_SKIP_LEDGER");
    let body = std::fs::read_to_string(&ledger).expect("ledger file written");
    assert_eq!(
        body.lines().filter(|l| !l.trim().is_empty()).count(),
        1,
        "an embedded newline must not split the record: {body}"
    );
    assert!(
        body.contains(r#"a \"quoted\"\nreason\\here"#),
        "quotes/newlines/backslashes must be JSON-escaped: {body}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn ledger_appends_across_calls_rather_than_truncating() {
    let base = scratch("append");
    let ledger = base.join("skips.jsonl");
    std::env::set_var("SQUEEZEFS_TEST_SKIP_LEDGER", &ledger);
    let _ = testkit::declare(site!(), SkipClass::OptIn, "first");
    let _ = testkit::declare(site!(), SkipClass::Hardware, "second");
    std::env::remove_var("SQUEEZEFS_TEST_SKIP_LEDGER");
    let body = std::fs::read_to_string(&ledger).expect("ledger file written");
    assert_eq!(
        body.lines().filter(|l| !l.trim().is_empty()).count(),
        2,
        "parallel test binaries append; a truncating open loses records: {body}"
    );
    assert!(body.contains(r#""class":"hardware""#));
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn require_flags_are_read_per_class_and_all_is_a_superset() {
    assert!(
        !testkit::required(SkipClass::Mount),
        "no require var is set in the default gate run"
    );
    std::env::set_var("SQUEEZEFS_TEST_REQUIRE_MOUNT", "1");
    assert!(testkit::required(SkipClass::Mount));
    assert!(
        !testkit::required(SkipClass::Hardware),
        "REQUIRE_MOUNT must not promote unrelated classes"
    );
    std::env::remove_var("SQUEEZEFS_TEST_REQUIRE_MOUNT");
    std::env::set_var("SQUEEZEFS_TEST_REQUIRE_ALL", "yes");
    for c in [
        SkipClass::Mount,
        SkipClass::Root,
        SkipClass::NonRoot,
        SkipClass::Sudo,
        SkipClass::Hardware,
        SkipClass::Toolchain,
        SkipClass::Capability,
        SkipClass::OptIn,
    ] {
        assert!(testkit::required(c), "REQUIRE_ALL covers {}", c.as_str());
    }
    std::env::remove_var("SQUEEZEFS_TEST_REQUIRE_ALL");
}

// ---------------------------------------------------------------------------
// 2. require-mount turns a skip into a failure (child-process legs)
// ---------------------------------------------------------------------------

/// Run one test of this same binary in a child with the given env.
fn run_self(test: &str, envs: &[(&str, &str)]) -> (bool, String) {
    let mut cmd = Command::new(std::env::current_exe().expect("test binary path"));
    // `--ignored` because the probe below is `#[ignore]`d in the parent run.
    cmd.args([
        test,
        "--exact",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    // The child must not inherit a require mode from the parent's shell.
    cmd.env_remove("SQUEEZEFS_TEST_REQUIRE_MOUNT");
    cmd.env_remove("SQUEEZEFS_TEST_REQUIRE_ALL");
    cmd.env_remove("SQUEEZEFS_TEST_SKIP_LEDGER");
    cmd.env("SQUEEZEFS_SKIP_LEDGER_CHILD", "1");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn child test");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

/// The child-side probe: an unconditional mount-class skip. Ignored in
/// the parent run (it would always "skip"); the two legs below drive it.
#[test]
#[ignore = "driven by the require-mode legs in a child process"]
fn child_probe_mount_class_skip() {
    assert!(
        std::env::var("SQUEEZEFS_SKIP_LEDGER_CHILD").is_ok(),
        "this probe is only meaningful under run_self"
    );
    if !testkit::declare(site!(), SkipClass::Mount, "child probe: no mount here") {
        // The default posture: a skip, and the test passes.
        return;
    }
    unreachable!("declare never reports 'run'");
}

#[test]
fn without_require_mount_a_mount_skip_passes_but_is_ledgered() {
    let (ok, text) = run_self("child_probe_mount_class_skip", &[("RUST_BACKTRACE", "0")]);
    assert!(ok, "default posture: a mount skip is not a failure\n{text}");
    assert!(
        text.contains("##SQUEEZEFS-SKIP##") && text.contains(r#""class":"mount""#),
        "the skip must still be announced on an UNCAPTURED stderr handle\n{text}"
    );
    assert!(
        text.contains(r#""required":false"#),
        "and marked not-required\n{text}"
    );
}

#[test]
fn require_mount_turns_the_mount_skip_into_a_failure() {
    let (ok, text) = run_self(
        "child_probe_mount_class_skip",
        &[
            ("SQUEEZEFS_TEST_REQUIRE_MOUNT", "1"),
            ("RUST_BACKTRACE", "0"),
        ],
    );
    assert!(
        !ok,
        "SQUEEZEFS_TEST_REQUIRE_MOUNT=1 must FAIL the test, not skip it\n{text}"
    );
    assert!(
        text.contains("[REQUIRED-MOUNT]"),
        "the failure must name the class loudly\n{text}"
    );
    assert!(
        text.contains(r#""required":true"#),
        "the ledger record must be emitted BEFORE the panic, marked required\n{text}"
    );
}

#[test]
fn require_all_also_covers_the_mount_class() {
    let (ok, text) = run_self(
        "child_probe_mount_class_skip",
        &[("SQUEEZEFS_TEST_REQUIRE_ALL", "1"), ("RUST_BACKTRACE", "0")],
    );
    assert!(!ok, "REQUIRE_ALL is a superset of REQUIRE_MOUNT\n{text}");
    assert!(text.contains("SQUEEZEFS_TEST_REQUIRE_ALL"), "{text}");
}

// ---------------------------------------------------------------------------
// 3. the real mount gate is honest about this host
// ---------------------------------------------------------------------------

#[test]
fn mount_gate_agrees_with_the_host_it_runs_on() {
    // Not a skip: the gate's verdict must MATCH an independent probe, so
    // a broken ladder (e.g. one that returns true with no /dev/fuse)
    // fails here rather than producing phantom green mount runs.
    let independently_supported = Path::new("/dev/fuse").exists()
        && testkit::fuse_uring_enabled().is_ok()
        && testkit::fusermount3_path().is_some();
    let gate = testkit::mount_supported(site!());
    assert_eq!(
        gate, independently_supported,
        "the shared mount ladder must agree with a direct probe of the host"
    );
}

// ---------------------------------------------------------------------------
// 4. the tree-wide audit — what stops a 15th private skip
// ---------------------------------------------------------------------------

/// Test sources that are allowed to mention `[SKIP]` without routing
/// through the testkit: this file (it asserts on the token) and the
/// testkit itself.
const AUDIT_EXEMPT: &[&str] = &["skip_ledger_tests.rs"];

#[test]
fn no_test_file_carries_a_private_skip_ladder() {
    let tests_dir = repo_root().join("tests");
    let mut offenders: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&tests_dir).expect("read tests/") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if AUDIT_EXEMPT.contains(&name.as_str()) {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read test source");
        for (i, line) in src.lines().enumerate() {
            let t = line.trim();
            if t.starts_with("//") {
                continue;
            }
            // The retired idiom: any print of a SKIP notice.
            if (t.contains("eprintln!") || t.contains("println!"))
                && (t.contains("[SKIP]") || t.contains("SKIP:"))
            {
                offenders.push(format!(
                    "{name}:{} — private skip notice; route it through \
                     squeezefs_testkit::declare/site! so it lands in the ledger \
                     and can be promoted by SQUEEZEFS_TEST_REQUIRE_*",
                    i + 1
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "TEST-2: {} private skip ladder(s) found — every environment skip \
         must go through the ONE shared helper:\n  {}",
        offenders.len(),
        offenders.join("\n  ")
    );
}

#[test]
fn the_mount_gated_suites_all_use_the_shared_gate() {
    // The sixteen live-mount files (the surface a phantom-green run hides).
    // If a file leaves this list, it must be because it no longer needs a
    // mount — not because it grew its own ladder.
    const MOUNT_GATED: &[&str] = &[
        "cache_path_policy_tests.rs",
        "cli_clients_df_tests.rs",
        "commit_wake_loss_tests.rs",
        "dismount_staged_residue_tests.rs",
        "format_guard_tests.rs",
        "fsync_promote_staged_tests.rs",
        "inline_raise_tests.rs",
        "mount_owner_override_tests.rs",
        "multi_queue_tests.rs",
        "phantom_backend0_tests.rs",
        "posix_mount_semantics_tests.rs",
        "statfs_tests.rs",
        "transport_concurrency_tests.rs",
        "transport_geometry_tests.rs",
        "transport_ingress_tests.rs",
        "transport_lease_overlong_tests.rs",
    ];
    let tests_dir = repo_root().join("tests");
    for f in MOUNT_GATED {
        let src = std::fs::read_to_string(tests_dir.join(f)).expect("read mount-gated suite");
        assert!(
            src.contains("squeezefs_testkit"),
            "{f} is mount-gated but does not use the shared testkit gate — \
             SQUEEZEFS_TEST_REQUIRE_MOUNT cannot see it"
        );
        assert!(
            !src.contains("/sys/module/fuse/parameters/enable_uring"),
            "{f} still carries a private copy of the enable_uring probe"
        );
    }
}

#[test]
fn the_capability_gated_suites_all_ride_the_zc_capability_gate() {
    // TEST-2, capability edition: every `tests/*.rs` suite that declares a
    // capability-class skip must be NAMED in the packaged consumer of
    // `SQUEEZEFS_TEST_REQUIRE_CAPABILITY=1` (`tests/run_zc_capability_gate.sh`
    // — the root-run leg). Without this pin a new zc suite self-skips
    // forever on the one box that HAS the sqz kernel: unprivileged `cargo
    // test` can never arm FUSE_URING_ZERO_COPY (the kernel gates it on
    // CAP_SYS_ADMIN), so "all green" would mean the zero-copy surface was
    // never executed anywhere — the phantom-green posture the mount gate
    // killed, replayed on the capability class. Auto-discovered, not a
    // hand-list: a fourth capability suite fails HERE until the gate
    // script runs it.
    let tests_dir = repo_root().join("tests");
    let script = std::fs::read_to_string(tests_dir.join("run_zc_capability_gate.sh"))
        .expect("tests/run_zc_capability_gate.sh exists (the capability-gate leg)");
    assert!(
        script.contains("SQUEEZEFS_TEST_REQUIRE_CAPABILITY=1"),
        "the capability gate must export SQUEEZEFS_TEST_REQUIRE_CAPABILITY=1 — \
         without it every decline is a ledgered skip, not a failure"
    );
    let mut capability_suites: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&tests_dir).expect("read tests/") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("utf8 test stem")
            .to_string();
        if name == "skip_ledger_tests" {
            continue; // this file names the class in prose, not as a gate
        }
        let src = std::fs::read_to_string(&path).expect("read test suite");
        if src.contains("SkipClass::Capability") {
            capability_suites.push(name);
        }
    }
    assert!(
        !capability_suites.is_empty(),
        "no capability-class suites found — if the class was retired, retire \
         this pin and the gate script with it"
    );
    for suite in &capability_suites {
        assert!(
            script.contains(suite.as_str()),
            "{suite} declares SkipClass::Capability but is not named in \
             tests/run_zc_capability_gate.sh — it can never be promoted to \
             run on a capable box (add it to the gate's SUITES list)"
        );
    }
}
