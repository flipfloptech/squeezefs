//! CLI grammar tests for the top-level `squeezefs nvmeof` verb surface
//! (`docs/design-nvmeof-target-management.md` §6.2/§6.3, landed by
//! PR 2/N2), driven through the real binary (`CARGO_BIN_EXE_squeezefs`),
//! unprivileged. Pinned here:
//!
//! * the old `storage nvmeof …` spellings and the deleted `spdk-*`
//!   lifecycle verbs fail with clap's unknown-verb error (the README
//!   removed-verbs ledger explains them);
//! * `--target-stack` resolution (flag > `SQUEEZEFS_NVMEOF_TARGET_STACK`
//!   env > default `spdk`) — **as of N4 the default stack reaches a real
//!   `SpdkStack`**: the N3 interim milestone refusal is dead, so an
//!   unprivileged SPDK-selected share/restore dies on the ROOT rung
//!   (proving stack construction succeeded), never on a milestone
//!   message; resolution is observed via the `--nsid` per-stack
//!   discriminator (nvmet refuses ≠ 1 at the grammar rung, spdk allows);
//! * the `target …` verb surface exists (N3): install/setup/start/stop/
//!   status demand root; systemd-unit emits unprivileged;
//!   `install --version` must equal the pin;
//! * per-stack flag semantics (§6.2): `--nsid` is SPDK-only (nvmet index
//!   structurally fixed at 1 ⇒ `--nsid` ≠ 1 with nvmet refuses loud);
//!   `--ns-uuid` seeds both stacks and must parse; N4 adds
//!   `--accept-version-drift` to the mutating share verbs (share /
//!   unshare / restore — SPDK rung 4) and `--force` to unshare (the R7
//!   live-consumer override), both refusing loud where meaningless.
//!
//! Every invocation uses a missing backing path and a tempdir state dir,
//! so even a root run mutates nothing.

use std::process::{Command, Output};

fn squeezefs_bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

fn run(args: &[&str], envs: &[(&str, &str)]) -> Output {
    let state_dir = tempfile::tempdir().expect("state dir");
    let mut cmd = Command::new(squeezefs_bin());
    cmd.args(args)
        .env("SQUEEZEFS_NVMEOF_STATE_DIR", state_dir.path())
        .env_remove("SQUEEZEFS_NVMEOF_TARGET_STACK");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn squeezefs")
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

// ---------------------------------------------------------------------------
// removed verbs (README ledger entries explain these)
// ---------------------------------------------------------------------------

/// The `storage nvmeof …` surface is gone: stale scripts fail with clap's
/// unknown-verb error, comprehensibly.
#[test]
fn test_old_storage_nvmeof_spellings_removed() {
    for args in [
        vec!["storage", "nvmeof", "list"],
        vec![
            "storage",
            "nvmeof",
            "share",
            "/dev/null",
            "--ip",
            "127.0.0.1",
        ],
        vec!["storage", "nvmeof", "unshare", "nqn.x"],
        vec!["storage", "nvmeof", "restore-shares"],
        vec!["storage", "nvmeof", "spdk-install"],
    ] {
        let out = run(&args, &[]);
        assert!(!out.status.success(), "removed spelling {args:?} must fail");
        let err = stderr_of(&out);
        assert!(
            err.contains("unrecognized subcommand") || err.contains("unexpected argument"),
            "removed spelling {args:?} must die on clap's unknown-verb error, got: {err}"
        );
    }
}

/// The deleted `spdk-*` verbs (and `restore-shares`) do not exist under
/// the new top-level verb either.
#[test]
fn test_deleted_verbs_absent_from_new_grammar() {
    for verb in [
        "spdk-install",
        "spdk-setup",
        "spdk-bind",
        "spdk-unbind",
        "spdk-start",
        "restore-shares",
    ] {
        let out = run(&["nvmeof", verb], &[]);
        assert!(!out.status.success(), "{verb} must not exist");
        let err = stderr_of(&out);
        assert!(
            err.contains("unrecognized subcommand") || err.contains("error"),
            "{verb}: {err}"
        );
    }
}

/// The new verb surface parses: `nvmeof --help` lists the §6.2 verbs
/// (incl. `adopt`, PR 4b).
#[test]
fn test_new_grammar_help_lists_verbs() {
    let out = run(&["nvmeof", "--help"], &[]);
    assert!(out.status.success(), "nvmeof --help must succeed");
    let help = combined(&out);
    for verb in [
        "share",
        "unshare",
        "list",
        "restore",
        "adopt",
        "connect",
        "disconnect",
        "target",
    ] {
        assert!(help.contains(verb), "help must list '{verb}': {help}");
    }
}

/// The `adopt` verb grammar (§6.10, PR 4b): root demanded before any
/// probing (an unprivileged run dies on the root rung — adopt writes
/// the ledger and probes both stacks); `--target-stack` is the
/// both-stacks-live disambiguator and parses the stack values; a bogus
/// value dies on clap's invalid-value error before anything runs.
#[test]
fn test_adopt_grammar_root_rung_and_stack_disambiguator() {
    let out = run(&["nvmeof", "adopt", "nqn.2026-06.io.foreign:x"], &[]);
    assert!(!out.status.success(), "unprivileged adopt must fail");
    let text = combined(&out);
    assert!(
        text.contains("root"),
        "unprivileged adopt dies on the root rung: {text}"
    );

    for stack in ["spdk", "nvmet"] {
        let out = run(
            &[
                "nvmeof",
                "adopt",
                "nqn.2026-06.io.foreign:x",
                "--target-stack",
                stack,
            ],
            &[],
        );
        assert!(!out.status.success());
        let text = combined(&out);
        assert!(
            text.contains("root"),
            "--target-stack {stack} parses and adopt still dies on the root rung: {text}"
        );
    }

    let out = run(
        &[
            "nvmeof",
            "adopt",
            "nqn.2026-06.io.foreign:x",
            "--target-stack",
            "banana",
        ],
        &[],
    );
    assert!(!out.status.success());
    let err = stderr_of(&out);
    assert!(
        err.contains("invalid value") || err.contains("possible values"),
        "a bogus stack dies on clap: {err}"
    );

    // adopt takes exactly one positional (the subnqn).
    let out = run(&["nvmeof", "adopt"], &[]);
    assert!(!out.status.success());
    let err = stderr_of(&out);
    assert!(
        err.contains("required") || err.contains("SUBNQN") || err.contains("subnqn"),
        "missing subnqn dies on clap: {err}"
    );
}

/// The N3 `target …` verb surface exists (§6.2).
#[test]
fn test_target_grammar_help_lists_lifecycle_verbs() {
    let out = run(&["nvmeof", "target", "--help"], &[]);
    assert!(out.status.success(), "nvmeof target --help must succeed");
    let help = combined(&out);
    for verb in [
        "install",
        "setup",
        "start",
        "stop",
        "status",
        "systemd-unit",
    ] {
        assert!(
            help.contains(verb),
            "target help must list '{verb}': {help}"
        );
    }
}

// ---------------------------------------------------------------------------
// the SPDK default reaches a REAL SpdkStack from N4 on (the interim
// milestone refusal is dead — unprivileged runs die on the root rung)
// ---------------------------------------------------------------------------

fn assert_reaches_real_spdk_path(context: &str, text: &str) {
    assert!(
        !text.contains("lands with milestone N4") && !text.contains("SPDK share management lands"),
        "{context}: the N3 interim milestone refusal must be DEAD at N4: {text}"
    );
    assert!(
        text.contains("root"),
        "{context}: unprivileged SPDK-selected verbs die on the root rung (stack construction \
         succeeded): {text}"
    );
}

/// `share` with the default stack (spdk) reaches the real SPDK path:
/// unprivileged it dies demanding root — never on a milestone message.
#[test]
fn test_share_default_stack_reaches_real_spdk_path() {
    let out = run(
        &[
            "nvmeof",
            "share",
            "/nonexistent/backing.img",
            "--ip",
            "127.0.0.1",
        ],
        &[],
    );
    assert!(!out.status.success(), "unprivileged share must fail");
    assert_reaches_real_spdk_path("default stack", &combined(&out));
}

/// Explicit `--target-stack spdk` reaches the same real path.
#[test]
fn test_share_explicit_spdk_reaches_real_path() {
    let out = run(
        &[
            "nvmeof",
            "share",
            "/nonexistent/backing.img",
            "--ip",
            "127.0.0.1",
            "--target-stack",
            "spdk",
        ],
        &[],
    );
    assert!(!out.status.success());
    assert_reaches_real_spdk_path("explicit spdk", &combined(&out));
}

/// Stack resolution order: the flag wins over the env; the env wins over
/// the default — observed via the `--nsid` per-stack discriminator
/// (nvmet refuses `--nsid` ≠ 1 at the grammar rung, spdk allows it).
#[test]
fn test_stack_resolution_flag_over_env_over_default() {
    // Default (spdk): --nsid 2 is legal — the run proceeds to the root
    // rung, never the structural-index refusal.
    let out = run(
        &[
            "nvmeof",
            "share",
            "/nonexistent/backing.img",
            "--ip",
            "127.0.0.1",
            "--nsid",
            "2",
        ],
        &[],
    );
    assert!(!out.status.success());
    let text = combined(&out);
    assert!(
        !text.contains("structurally fixed"),
        "default spdk allows --nsid 2: {text}"
    );
    assert_reaches_real_spdk_path("default stack nsid", &text);

    // env=nvmet: the env is honored — --nsid 2 dies on the structural
    // refusal BEFORE root.
    let out = run(
        &[
            "nvmeof",
            "share",
            "/nonexistent/backing.img",
            "--ip",
            "127.0.0.1",
            "--nsid",
            "2",
        ],
        &[("SQUEEZEFS_NVMEOF_TARGET_STACK", "nvmet")],
    );
    assert!(!out.status.success());
    let text = combined(&out);
    assert!(
        text.contains("structurally fixed at 1"),
        "env nvmet must select the kernel stack: {text}"
    );

    // flag=spdk beats env=nvmet: --nsid 2 is legal again.
    let out = run(
        &[
            "nvmeof",
            "share",
            "/nonexistent/backing.img",
            "--ip",
            "127.0.0.1",
            "--nsid",
            "2",
            "--target-stack",
            "spdk",
        ],
        &[("SQUEEZEFS_NVMEOF_TARGET_STACK", "nvmet")],
    );
    assert!(!out.status.success());
    let text = combined(&out);
    assert!(
        !text.contains("structurally fixed"),
        "the flag must beat the env: {text}"
    );
    assert_reaches_real_spdk_path("flag over env", &text);
}

/// An unparseable env value refuses loud (never a silent default).
#[test]
fn test_invalid_env_stack_value_refuses_loud() {
    let out = run(
        &[
            "nvmeof",
            "share",
            "/nonexistent/backing.img",
            "--ip",
            "127.0.0.1",
        ],
        &[("SQUEEZEFS_NVMEOF_TARGET_STACK", "banana")],
    );
    assert!(!out.status.success());
    let text = combined(&out);
    assert!(
        text.contains("SQUEEZEFS_NVMEOF_TARGET_STACK") && text.contains("banana"),
        "invalid env value must be named loudly: {text}"
    );
}

/// `restore --target-stack spdk` reaches the real SPDK restore path at
/// N4: unprivileged it dies on the root rung, never on a milestone
/// message.
#[test]
fn test_restore_spdk_filter_reaches_real_path() {
    let out = run(&["nvmeof", "restore", "--target-stack", "spdk"], &[]);
    assert!(!out.status.success());
    assert_reaches_real_spdk_path("restore spdk", &combined(&out));
}

// ---------------------------------------------------------------------------
// N4 mutating-verb flags (§6.2 flag placement + R7)
// ---------------------------------------------------------------------------

/// `--accept-version-drift` exists on the mutating share verbs (share /
/// unshare / restore — it gates SPDK preflight rung 4 wherever that rung
/// runs); unshare additionally carries `--force` (the live-consumer
/// override). Unprivileged runs die on the root rung, never on clap.
#[test]
fn test_mutating_share_verbs_carry_drift_and_force_flags() {
    for args in [
        vec![
            "nvmeof",
            "share",
            "/nonexistent/backing.img",
            "--ip",
            "127.0.0.1",
            "--accept-version-drift",
        ],
        vec![
            "nvmeof",
            "unshare",
            "nqn.2026-07.io.squeezefs:share-x",
            "--accept-version-drift",
        ],
        vec![
            "nvmeof",
            "unshare",
            "nqn.2026-07.io.squeezefs:share-x",
            "--force",
        ],
        vec!["nvmeof", "restore", "--accept-version-drift"],
        vec![
            "nvmeof",
            "restore",
            "--target-stack",
            "spdk",
            "--accept-version-drift",
        ],
    ] {
        let out = run(&args, &[]);
        assert!(!out.status.success(), "{args:?} unprivileged must fail");
        let text = combined(&out);
        assert!(
            !text.contains("unexpected argument") && !text.contains("unrecognized subcommand"),
            "{args:?} must exist in the N4 grammar: {text}"
        );
        assert!(text.contains("root"), "{args:?} demands root: {text}");
    }
}

/// `--accept-version-drift` with an explicit nvmet selection refuses
/// loud at the grammar rung (SPDK-only — the kernel target has no
/// version to drift; the `--disk-cache-paths` never-silently-ignore
/// precedent), before the root check.
#[test]
fn test_share_accept_version_drift_with_nvmet_refuses_loud() {
    let out = run(
        &[
            "nvmeof",
            "share",
            "/nonexistent/backing.img",
            "--ip",
            "127.0.0.1",
            "--target-stack",
            "nvmet",
            "--accept-version-drift",
        ],
        &[],
    );
    assert!(!out.status.success());
    let text = combined(&out);
    assert!(
        text.contains("--accept-version-drift") && text.contains("SPDK-only"),
        "must name the flag as SPDK-only: {text}"
    );
    assert!(
        !text.contains("root privileges"),
        "the flag refusal fires before the root check: {text}"
    );

    // Same law on the restore filter.
    let out = run(
        &[
            "nvmeof",
            "restore",
            "--target-stack",
            "nvmet",
            "--accept-version-drift",
        ],
        &[],
    );
    assert!(!out.status.success());
    let text = combined(&out);
    assert!(
        text.contains("--accept-version-drift") && text.contains("SPDK-only"),
        "restore --target-stack nvmet --accept-version-drift refuses: {text}"
    );
}

// ---------------------------------------------------------------------------
// per-stack flag semantics (§6.2, rev-3 issue 23)
// ---------------------------------------------------------------------------

/// `--nsid` ≠ 1 with `--target-stack nvmet` refuses loud: the nvmet
/// namespace index is structurally fixed at 1 (the `--disk-cache-paths`
/// precedent — never a silent flag-ignore).
#[test]
fn test_nsid_not_one_with_nvmet_refuses_loud() {
    let out = run(
        &[
            "nvmeof",
            "share",
            "/nonexistent/backing.img",
            "--ip",
            "127.0.0.1",
            "--target-stack",
            "nvmet",
            "--nsid",
            "2",
        ],
        &[],
    );
    assert!(!out.status.success(), "--nsid 2 with nvmet must refuse");
    let text = combined(&out);
    assert!(text.contains("--nsid"), "must name the flag: {text}");
    assert!(
        text.contains("structurally fixed at 1"),
        "must state the structural convention: {text}"
    );
    assert!(
        !text.contains("root privileges"),
        "the flag refusal must fire before the root check: {text}"
    );
}

/// `--nsid 1` matches the structural index and passes flag validation
/// (the run then fails at the root/backing rungs, proving validation was
/// the gate that passed).
#[test]
fn test_nsid_one_with_nvmet_passes_flag_validation() {
    let out = run(
        &[
            "nvmeof",
            "share",
            "/nonexistent/backing.img",
            "--ip",
            "127.0.0.1",
            "--target-stack",
            "nvmet",
            "--nsid",
            "1",
        ],
        &[],
    );
    assert!(!out.status.success());
    let text = combined(&out);
    assert!(
        !text.contains("structurally fixed"),
        "--nsid 1 must not trip the per-stack refusal: {text}"
    );
    assert!(
        text.contains("root") || text.contains("does not exist"),
        "must fail at the root/backing rung instead: {text}"
    );
}

/// A malformed `--ns-uuid` refuses loud on both stacks (it seeds the
/// recorded namespace identity — garbage must never reach the ledger).
#[test]
fn test_malformed_ns_uuid_refuses_loud() {
    let out = run(
        &[
            "nvmeof",
            "share",
            "/nonexistent/backing.img",
            "--ip",
            "127.0.0.1",
            "--target-stack",
            "nvmet",
            "--ns-uuid",
            "not-a-uuid",
        ],
        &[],
    );
    assert!(!out.status.success());
    let text = combined(&out);
    assert!(
        text.contains("--ns-uuid") && text.contains("not-a-uuid"),
        "must name the flag and the bad value: {text}"
    );
}

/// An unparseable listener IP refuses loud at the grammar rung.
#[test]
fn test_malformed_ip_refuses_loud() {
    let out = run(
        &[
            "nvmeof",
            "share",
            "/nonexistent/backing.img",
            "--ip",
            "not.an.ip.addr",
            "--target-stack",
            "nvmet",
        ],
        &[],
    );
    assert!(!out.status.success());
    let text = combined(&out);
    assert!(
        text.contains("not.an.ip.addr"),
        "must name the bad address: {text}"
    );
}

/// `unshare` of an NQN absent from the ledger refuses loud with `list`
/// guidance and the manual-remediation pointer (we never tear down
/// objects we did not record) — pinned unprivileged via the root-check
/// ordering: ledger dispatch happens only under root, so here we assert
/// the verb exists and demands root, not clap failure.
#[test]
fn test_unshare_verb_exists_and_demands_root() {
    let out = run(
        &["nvmeof", "unshare", "nqn.2026-07.io.squeezefs:share-x"],
        &[],
    );
    assert!(!out.status.success());
    let text = combined(&out);
    assert!(
        !text.contains("unrecognized subcommand"),
        "unshare must exist in the new grammar: {text}"
    );
    assert!(
        text.contains("root"),
        "unprivileged unshare demands root: {text}"
    );
}

// ---------------------------------------------------------------------------
// the N3 target lifecycle verb surface (§6.2/§6.5)
// ---------------------------------------------------------------------------

/// The mutating target verbs exist and demand root (unprivileged runs
/// die on the root rung, never on clap).
#[test]
fn test_target_mutating_verbs_exist_and_demand_root() {
    for args in [
        vec!["nvmeof", "target", "install"],
        vec!["nvmeof", "target", "setup"],
        vec!["nvmeof", "target", "start"],
        vec!["nvmeof", "target", "stop"],
        vec!["nvmeof", "target", "status"],
    ] {
        let out = run(&args, &[]);
        assert!(!out.status.success(), "{args:?} unprivileged must fail");
        let text = combined(&out);
        assert!(
            !text.contains("unrecognized subcommand"),
            "{args:?} must exist in the grammar: {text}"
        );
        assert!(text.contains("root"), "{args:?} demands root: {text}");
    }
}

/// `target install --version` must equal the pin — pin bumps are
/// deliberate PRs, never a CLI flag (§6.5/R1). Grammar-rung refusal:
/// fires before the root check.
#[test]
fn test_target_install_version_pin_refusal() {
    let out = run(&["nvmeof", "target", "install", "--version", "v27.01"], &[]);
    assert!(!out.status.success(), "--version v27.01 must refuse");
    let text = combined(&out);
    assert!(
        text.contains("v26.05") && text.contains("v27.01"),
        "names the pin and the request: {text}"
    );
    assert!(
        !text.contains("root privileges"),
        "the pin refusal fires before the root check: {text}"
    );
}

/// `target setup --hugemem-mb … --restore-prior` is a contradiction —
/// clap refuses the combination.
#[test]
fn test_target_setup_restore_prior_conflicts_with_hugemem() {
    let out = run(
        &[
            "nvmeof",
            "target",
            "setup",
            "--hugemem-mb",
            "512",
            "--restore-prior",
        ],
        &[],
    );
    assert!(!out.status.success());
    let text = combined(&out);
    assert!(
        text.contains("cannot be used with"),
        "clap conflict expected: {text}"
    );
}

/// `target start --core-mask … --cores …` is a contradiction.
#[test]
fn test_target_start_core_mask_conflicts_with_cores() {
    let out = run(
        &[
            "nvmeof",
            "target",
            "start",
            "--core-mask",
            "0x1",
            "--cores",
            "2",
        ],
        &[],
    );
    assert!(!out.status.success());
    assert!(
        combined(&out).contains("cannot be used with"),
        "clap conflict expected: {}",
        combined(&out)
    );
}

/// `target stop --target-stack nvmet` refuses loud: the kernel target is
/// not a process (there is nothing to stop) — never a silent no-op.
#[test]
fn test_target_stop_nvmet_refuses_loud() {
    let out = run(
        &["nvmeof", "target", "stop", "--target-stack", "nvmet"],
        &[],
    );
    assert!(!out.status.success());
    let text = combined(&out);
    assert!(
        text.contains("not a process"),
        "must explain the nvmet stop refusal: {text}"
    );
}

/// `target install` with the nvmet stack selected via env still installs
/// SPDK — the verb is SPDK-only by definition (§6.2 table); no silent
/// reinterpretation, the help says so. Here: the env must NOT change the
/// version-pin refusal path (proving install ignores stack selection).
#[test]
fn test_target_install_is_spdk_only_regardless_of_env() {
    let out = run(
        &["nvmeof", "target", "install", "--version", "v27.01"],
        &[("SQUEEZEFS_NVMEOF_TARGET_STACK", "nvmet")],
    );
    assert!(!out.status.success());
    assert!(
        combined(&out).contains("v26.05"),
        "install stays the SPDK pin path under env=nvmet: {}",
        combined(&out)
    );
}
