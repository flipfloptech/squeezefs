//! CLI grammar tests for the top-level `squeezefs nvmeof` verb surface
//! (`docs/design-nvmeof-target-management.md` §6.2/§6.3, landed by
//! PR 2/N2), driven through the real binary (`CARGO_BIN_EXE_squeezefs`),
//! unprivileged. Pinned here:
//!
//! * the old `storage nvmeof …` spellings and the deleted `spdk-*`
//!   lifecycle verbs fail with clap's unknown-verb error (the
//!   docs/operations.md removed-verbs ledger explains them);
//! * `--target-stack` resolution (flag > `SQUEEZEFS_NVMEOF_TARGET_STACK`
//!   env > default `nvmet` — the ONE target since SPDK was retired,
//!   R-SYM-8; the retirement refusals themselves are pinned in
//!   `tests/nvmeof_retire_spdk_tests.rs`): an unprivileged share dies on
//!   the ROOT rung (proving stack construction succeeded), and an
//!   unparseable env value refuses loud;
//! * the `target …` verb surface exists: setup/start/status demand root;
//!   stop refuses (the kernel target is not a process); systemd-unit
//!   emits unprivileged;
//! * flag semantics (§6.2): the nvmet namespace index is structurally
//!   fixed at 1 (`--nsid` ≠ 1 refuses loud); `--ns-uuid` must parse.
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
// removed verbs (docs/operations.md ledger entries explain these)
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

    let out = run(
        &[
            "nvmeof",
            "adopt",
            "nqn.2026-06.io.foreign:x",
            "--target-stack",
            "nvmet",
        ],
        &[],
    );
    assert!(!out.status.success());
    let text = combined(&out);
    assert!(
        text.contains("root"),
        "--target-stack nvmet parses and adopt still dies on the root rung: {text}"
    );

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

/// The `target …` verb surface exists (§6.2); the retired `install`
/// (SPDK-only by definition) is hidden from help.
#[test]
fn test_target_grammar_help_lists_lifecycle_verbs() {
    let out = run(&["nvmeof", "target", "--help"], &[]);
    assert!(out.status.success(), "nvmeof target --help must succeed");
    let help = combined(&out);
    assert!(
        !help.contains("install"),
        "the retired install verb is hidden: {help}"
    );
    for verb in ["setup", "start", "stop", "status", "systemd-unit"] {
        assert!(
            help.contains(verb),
            "target help must list '{verb}': {help}"
        );
    }
}

// ---------------------------------------------------------------------------
// the default stack is nvmet — the ONE target (R-SYM-8); unprivileged runs
// die on the root rung, proving stack construction succeeded
// ---------------------------------------------------------------------------

fn assert_reaches_real_nvmet_path(context: &str, text: &str) {
    assert!(
        text.contains("root"),
        "{context}: unprivileged nvmet-selected verbs die on the root rung (stack construction \
         succeeded): {text}"
    );
    assert!(
        !text.to_ascii_lowercase().contains("retired"),
        "{context}: the nvmet path never trips the SPDK retirement refusal: {text}"
    );
}

/// `share` with the default stack reaches the real nvmet path:
/// unprivileged it dies demanding root.
#[test]
fn test_share_default_stack_reaches_real_nvmet_path() {
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
    assert_reaches_real_nvmet_path("default stack", &combined(&out));
}

/// Explicit `--target-stack nvmet` and `SQUEEZEFS_NVMEOF_TARGET_STACK=nvmet`
/// reach the same real path — observed via the `--nsid` discriminator
/// (the nvmet index is structurally 1, so `--nsid 2` dies on the
/// structural refusal BEFORE root on every resolution rung).
#[test]
fn test_stack_resolution_flag_env_default_all_select_nvmet() {
    const ENV_NVMET: &[(&str, &str)] = &[("SQUEEZEFS_NVMEOF_TARGET_STACK", "nvmet")];
    const FLAG_NVMET: &[&str] = &["--target-stack", "nvmet"];
    for (extra, envs) in [
        (&[][..], &[][..]),
        (FLAG_NVMET, &[][..]),
        (&[][..], ENV_NVMET),
        (FLAG_NVMET, ENV_NVMET),
    ] {
        let mut args = vec![
            "nvmeof",
            "share",
            "/nonexistent/backing.img",
            "--ip",
            "127.0.0.1",
            "--nsid",
            "2",
        ];
        args.extend(extra.iter());
        let out = run(&args, envs);
        assert!(!out.status.success());
        let text = combined(&out);
        assert!(
            text.contains("structurally fixed at 1"),
            "{args:?} {envs:?}: every rung selects the kernel stack: {text}"
        );
        assert!(
            !text.contains("root privileges"),
            "{args:?}: the structural refusal fires before root: {text}"
        );
    }
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

// ---------------------------------------------------------------------------
// flag semantics (§6.2, rev-3 issue 23)
// ---------------------------------------------------------------------------

/// `--nsid` ≠ 1 refuses loud: the nvmet namespace index is structurally
/// fixed at 1 (the `--disk-cache-paths` precedent — never a silent
/// flag-ignore).
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

/// A malformed `--ns-uuid` refuses loud (it seeds the
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
// connect path/queue flags (2026-07-25 live-cluster findings: same-subnet
// dual-NIC hosts need the source pinned; a target granting fewer I/O
// queues than requested failed the connect with errno -18)
// ---------------------------------------------------------------------------

/// The three connect path/queue flags exist in the grammar, singly and
/// combined — unprivileged runs die on the root rung, never on clap.
#[test]
fn test_connect_path_flags_exist_singly_and_combined() {
    let base = [
        "nvmeof",
        "connect",
        "--ip",
        "127.0.0.1",
        "--subnqn",
        "nqn.2026-07.io.squeezefs:share-x",
    ];
    for extra in [
        vec!["--host-traddr", "10.0.0.2"],
        vec!["--host-iface", "eth1"],
        vec!["--nr-io-queues", "8"],
        vec!["--host-traddr", "10.0.0.2", "--host-iface", "eth1"],
        vec![
            "--host-traddr",
            "10.0.0.2",
            "--host-iface",
            "eth1",
            "--nr-io-queues",
            "4",
        ],
    ] {
        let mut args: Vec<&str> = base.to_vec();
        args.extend(extra.iter());
        let out = run(&args, &[]);
        assert!(!out.status.success(), "{args:?} unprivileged must fail");
        let text = combined(&out);
        assert!(
            !text.contains("unexpected argument") && !text.contains("unrecognized subcommand"),
            "{args:?} must exist in the connect grammar: {text}"
        );
        assert!(text.contains("root"), "{args:?} demands root: {text}");
    }
}

/// `--nr-io-queues 0` refuses at the grammar rung — zero I/O queues is
/// not a connection; the flag exists to BOUND the request, not to zero
/// it — before the root check.
#[test]
fn test_connect_nr_io_queues_zero_refuses_loud() {
    let out = run(
        &[
            "nvmeof",
            "connect",
            "--ip",
            "127.0.0.1",
            "--subnqn",
            "nqn.2026-07.io.squeezefs:share-x",
            "--nr-io-queues",
            "0",
        ],
        &[],
    );
    assert!(!out.status.success(), "--nr-io-queues 0 must refuse");
    let text = combined(&out);
    assert!(
        text.contains("invalid value") && text.contains("--nr-io-queues"),
        "must die on clap's range validation naming the flag: {text}"
    );
    assert!(
        !text.contains("root privileges"),
        "the range refusal fires before the root check: {text}"
    );
}

// ---------------------------------------------------------------------------
// the N3 target lifecycle verb surface (§6.2/§6.5)
// ---------------------------------------------------------------------------

/// The mutating target verbs exist and demand root (unprivileged runs
/// die on the root rung, never on clap). `stop` refuses before root (the
/// kernel target is not a process) and `install` is retired — both are
/// pinned elsewhere in this file / in `nvmeof_retire_spdk_tests`.
#[test]
fn test_target_mutating_verbs_exist_and_demand_root() {
    for args in [
        vec!["nvmeof", "target", "setup"],
        vec!["nvmeof", "target", "start"],
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
