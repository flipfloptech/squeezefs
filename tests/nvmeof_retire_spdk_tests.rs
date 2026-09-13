//! SPDK retired as an NVMe-oF target — the kernel `nvmet` is THE target
//! (`docs/design-symmetric-metadata.md` §5.8.1, KD-SYM-23, owner ruling
//! R-SYM-8 of 2026-09-12; PR 16 of the symmetric-metadata program).
//!
//! Forward-only: nothing SPDK-shaped falls back or silently defaults.
//! Pinned here:
//!
//! * `--target-stack spdk` (flag), `SQUEEZEFS_NVMEOF_TARGET_STACK=spdk`
//!   (env) and the `spdk` spelling itself refuse LOUD naming nvmet and
//!   the operator's re-share sequence; the default stack is nvmet;
//! * the SPDK-only verb (`target install`) is a retired verb under the
//!   `removed_verb()` convention — refuses naming its successor before
//!   the root rung; the SPDK-only flags are gone from the grammar;
//! * an SPDK share still in the ledger is LISTED (classification names
//!   the retirement + the re-share sequence) and NEVER re-presented:
//!   `restore` skips it loud, `unshare` removes only the ledger record,
//!   `adopt` can only ever mint an nvmet candidate, and the ledger's
//!   duplicate-backing guard holds the backing until the record is gone;
//! * the retired knobs (`SQUEEZEFS_SPDK_TGT_BIN`,
//!   `SQUEEZEFS_NVMEOF_RUN_DIR`) refuse under the ENG-10 convention, and
//!   the retired VALUE `SQUEEZEFS_NVMEOF_TARGET_STACK=spdk` refuses at the
//!   same startup gate with the retirement + the re-share sequence (not the
//!   generic enum message);
//! * `restore` exits nonzero while a retired SPDK record remains (the
//!   nvmet oneshot unit fails loud until step 1 of the sequence has run);
//! * `src/nvmeof/spdk/` is gone and nothing under `src/nvmeof/` reaches
//!   for it (the no-dead-code law, structurally).
//!
//! Every binary invocation uses a missing backing path and a tempdir
//! state dir, so even a root run mutates nothing.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use squeezefs::env_knobs::{self, Kind};
use squeezefs::nvmeof::ledger::Ledger;
use squeezefs::nvmeof::stack::{Listener, LiveShare, RestoreOutcome, ShareRecord, ShareState};
use squeezefs::nvmeof::{
    adopt_candidate, classification_of, partition_restorable, resolve_stack, restore_outcome,
    retire_spdk_share, StackKind,
};

const UUID_A: &str = "e2b1c9a4-52d1-4a08-9f31-7c2b8d1e0aa1";
const NQN_SPDK: &str = "nqn.2026-07.io.squeezefs:share-spdk-legacy";
const NQN_NVMET: &str = "nqn.2026-07.io.squeezefs:share-nvmet-live";

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

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn record(subnqn: &str, stack: StackKind, backing: &str) -> ShareRecord {
    ShareRecord {
        subnqn: subnqn.to_string(),
        stack,
        state: ShareState::Pending,
        backing_path: backing.to_string(),
        backing_canonical: backing.to_string(),
        nsid: (stack == StackKind::Spdk).then_some(1),
        ns_uuid: Some(UUID_A.to_string()),
        listeners: vec![Listener {
            ip: "127.0.0.1".to_string(),
            port: 4420,
            nvmet_port_id: (stack == StackKind::Nvmet).then_some(53000),
        }],
        bdev_name: (stack == StackKind::Spdk).then(|| "sqz_aio_legacy".to_string()),
        ptpl_file: (stack == StackKind::Spdk).then(|| format!("spdk/ptpl/{UUID_A}.json")),
        loop_device: None,
        created_utc: "2026-07-18T00:00:00Z".to_string(),
        allow_hosts: Vec::new(),
        adopted_from: None,
    }
}

/// The refusal text must carry all three: the retirement, the successor
/// stack, and the operator's re-share sequence (unshare → share on nvmet).
fn assert_names_retirement_and_reshare(context: &str, text: &str) {
    assert!(
        text.to_ascii_lowercase().contains("retired"),
        "{context}: must say SPDK was retired: {text}"
    );
    assert!(text.contains("nvmet"), "{context}: must name nvmet: {text}");
    assert!(
        text.contains("nvmeof unshare") && text.contains("nvmeof share"),
        "{context}: must carry the re-share sequence (unshare → share): {text}"
    );
}

// ---------------------------------------------------------------------------
// the spelling, the flag, the env, the default
// ---------------------------------------------------------------------------

#[test]
fn the_spdk_spelling_parses_to_a_refusal_naming_nvmet() {
    let err = "spdk"
        .parse::<StackKind>()
        .expect_err("'spdk' must not resolve to a target stack");
    assert!(err.to_ascii_lowercase().contains("retired"), "{err}");
    assert!(err.contains("nvmet"), "{err}");
    assert_eq!("nvmet".parse::<StackKind>(), Ok(StackKind::Nvmet));
}

#[test]
fn resolve_stack_refuses_the_spdk_flag_with_the_reshare_sequence() {
    let err = resolve_stack(Some(StackKind::Spdk))
        .expect_err("--target-stack spdk must refuse")
        .to_string();
    assert_names_retirement_and_reshare("resolve_stack(spdk)", &err);
    assert!(
        err.contains("--target-stack nvmet"),
        "the refusal names the successor spelling: {err}"
    );
}

#[test]
fn the_default_stack_is_nvmet() {
    assert_eq!(
        resolve_stack(None).expect("default resolves"),
        StackKind::Nvmet,
        "with no flag and no env the ONE target is nvmet"
    );
    assert_eq!(
        resolve_stack(Some(StackKind::Nvmet)).expect("explicit nvmet"),
        StackKind::Nvmet
    );
}

// ---------------------------------------------------------------------------
// ledger: listed with the re-share sequence, never re-presented
// ---------------------------------------------------------------------------

#[test]
fn an_spdk_ledger_record_is_listed_as_retired_never_as_managed_or_restorable() {
    let mut rec = record(NQN_SPDK, StackKind::Spdk, "/dev/zram9");
    rec.state = ShareState::Active;
    for live in [true, false] {
        let class = classification_of(&rec, live);
        assert_names_retirement_and_reshare(&format!("spdk record live={live}"), class);
        assert!(
            !class.starts_with("managed") && !class.contains("restore candidate"),
            "an SPDK record is never managed or a restore candidate: {class}"
        );
    }
    // The nvmet classifications are untouched.
    let mut nv = record(NQN_NVMET, StackKind::Nvmet, "/dev/zram8");
    nv.state = ShareState::Active;
    assert_eq!(classification_of(&nv, true), "managed");
    assert!(classification_of(&nv, false).contains("restore candidate"));
}

#[test]
fn restore_skips_spdk_records_loud_and_replays_only_nvmet() {
    let mut spdk = record(NQN_SPDK, StackKind::Spdk, "/dev/zram9");
    spdk.state = ShareState::Active;
    let mut nvmet = record(NQN_NVMET, StackKind::Nvmet, "/dev/zram8");
    nvmet.state = ShareState::Active;

    let (replay, skipped) = partition_restorable(vec![spdk.clone(), nvmet.clone()]);
    assert_eq!(replay, vec![nvmet], "only nvmet records are replayed");
    assert_eq!(
        skipped.len(),
        1,
        "every SPDK record is reported, never dropped"
    );
    assert_eq!(skipped[0].subnqn, NQN_SPDK);
    match &skipped[0].outcome {
        RestoreOutcome::Skipped(why) => {
            assert_names_retirement_and_reshare("restore skip", why);
        }
        other => panic!("an SPDK record must be Skipped, got {other:?}"),
    }
}

/// A share the ledger records and this binary cannot serve is a FAILED
/// restore for that share: `restore` (and so `target start` and the nvmet
/// oneshot unit) exits nonzero while any retired SPDK record remains, naming
/// the sequence — never a green unit over an unserved share.
#[test]
fn restore_exits_nonzero_while_a_retired_spdk_record_remains() {
    restore_outcome(2, 0, 0).expect("all replayed, nothing retired: success");
    restore_outcome(0, 0, 0).expect("an empty ledger is a success");
    let err = restore_outcome(2, 0, 1)
        .expect_err("a retired record left in the ledger fails the restore")
        .to_string();
    assert_names_retirement_and_reshare("restore verdict", &err);
    assert!(err.contains('1'), "counts the retired records: {err}");
    let err = restore_outcome(3, 2, 0)
        .expect_err("nvmet failures still fail")
        .to_string();
    assert!(err.contains("2 of 3"), "{err}");
}

#[test]
fn unshare_of_an_spdk_record_removes_only_the_ledger_entry_and_names_the_manual_teardown() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = Ledger::new(dir.path());
    let rec = record(NQN_SPDK, StackKind::Spdk, "/dev/zram9");
    ledger
        .begin_share(&rec)
        .expect("seed the legacy SPDK record");
    ledger.finalize_share(NQN_SPDK).expect("finalize");
    let active = ledger.find(NQN_SPDK).expect("load").expect("present");
    assert_eq!(active.state, ShareState::Active);

    let note = retire_spdk_share(&ledger, &active).expect("ledger-only removal succeeds");
    assert!(
        ledger.find(NQN_SPDK).expect("load").is_none(),
        "the SPDK record is gone from the ledger"
    );
    assert_names_retirement_and_reshare("unshare note", &note);
    assert!(
        note.contains("rpc.py"),
        "the note names the manual spdk_tgt teardown the product no longer drives: {note}"
    );
    let residue = dir.path().join("spdk");
    assert!(
        note.contains(&residue.display().to_string())
            && note.contains(&format!("spdk/ptpl/{UUID_A}.json")),
        "the note names the state-dir residue the retired stack left (the SPDK state dir + the \
         recorded ptpl file) for the operator to remove once every SPDK record is gone: {note}"
    );

    // Retiring a record that is not SPDK is a programming error, refused.
    let nv = record(NQN_NVMET, StackKind::Nvmet, "/dev/zram8");
    ledger.begin_share(&nv).expect("seed nvmet");
    retire_spdk_share(&ledger, &nv).expect_err("an nvmet record rides the nvmet stack's unshare");
    assert!(ledger.find(NQN_NVMET).expect("load").is_some());
}

#[test]
fn an_spdk_record_holds_its_backing_against_an_nvmet_reshare_until_unshared() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = Ledger::new(dir.path());
    let legacy = record(NQN_SPDK, StackKind::Spdk, "/dev/zram9");
    ledger.begin_share(&legacy).expect("seed");
    ledger.finalize_share(NQN_SPDK).expect("finalize");

    let reshare = record(NQN_NVMET, StackKind::Nvmet, "/dev/zram9");
    let err = ledger
        .begin_share(&reshare)
        .expect_err("the same backing must never be double-served, across stacks included");
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    let msg = err.to_string();
    assert!(
        msg.contains("nvmeof unshare") && msg.contains(NQN_SPDK),
        "the refusal names the unshare of the SPDK holder: {msg}"
    );

    let active = ledger.find(NQN_SPDK).expect("load").expect("present");
    retire_spdk_share(&ledger, &active).expect("step 1 of the re-share sequence");
    ledger
        .begin_share(&reshare)
        .expect("step 2: the backing is free for the nvmet share");
}

#[test]
fn adopt_never_mints_an_spdk_candidate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = Ledger::new(dir.path());
    let live = LiveShare {
        subnqn: "nqn.2026-06.io.foreign:handbuilt-1".to_string(),
        device_path: "/dev/zram5".to_string(),
        backing_canonical: "/dev/zram5".to_string(),
        ns_uuid: Some(UUID_A.to_string()),
        listeners: vec![Listener {
            ip: "10.0.0.7".to_string(),
            port: 4420,
            nvmet_port_id: Some(53011),
        }],
        enabled: true,
        nsids: vec![1],
        allow_hosts: Vec::new(),
    };
    let (candidate, _notes) =
        adopt_candidate(&live.subnqn, std::slice::from_ref(&live), &ledger).expect("candidate");
    assert_eq!(candidate.stack, StackKind::Nvmet);
    assert_eq!(candidate.state, ShareState::Pending);
    assert!(candidate.bdev_name.is_none() && candidate.ptpl_file.is_none());

    // An NQN the ledger still holds as an SPDK share is never adopted
    // onto anything — it is `unshare` territory (the re-share sequence).
    let legacy = record(&live.subnqn, StackKind::Spdk, "/dev/zram9");
    ledger.begin_share(&legacy).expect("seed");
    let err = adopt_candidate(&live.subnqn, std::slice::from_ref(&live), &ledger)
        .expect_err("already ledgered")
        .to_string();
    assert!(err.contains("adopt_already_ledgered"), "{err}");
    assert_names_retirement_and_reshare("adopt of an SPDK-ledgered NQN", &err);

    // The same law for the BACKING arm: a live nvmet object on a backing an
    // SPDK record still holds is `unshare` territory — `restore` never
    // reconciles that record, so the remediation must not say it does.
    let other = LiveShare {
        subnqn: "nqn.2026-06.io.foreign:handbuilt-2".to_string(),
        backing_canonical: "/dev/zram9".to_string(),
        device_path: "/dev/zram9".to_string(),
        ..live.clone()
    };
    let err = adopt_candidate(&other.subnqn, std::slice::from_ref(&other), &ledger)
        .expect_err("backing held by the SPDK record")
        .to_string();
    assert!(err.contains("adopt_already_ledgered"), "{err}");
    assert_names_retirement_and_reshare("adopt of an SPDK-held backing", &err);
    assert!(
        !err.contains("restore` reconciles"),
        "restore never reconciles a retired record: {err}"
    );
}

// ---------------------------------------------------------------------------
// knobs (ENG-10)
// ---------------------------------------------------------------------------

/// The knob keeps `nvmet` as its ONE admissible value and carries `spdk` as
/// a RETIRED value: the ENG-10 startup gate refuses it with the retirement
/// and the re-share sequence — the same text every other SPDK-shaped
/// surface carries — not the generic "expected one of" enum message.
#[test]
fn the_target_stack_knob_defaults_to_nvmet_and_refuses_spdk_as_a_retired_value() {
    let knob = env_knobs::lookup("SQUEEZEFS_NVMEOF_TARGET_STACK").expect("registered");
    assert_eq!(knob.default, "nvmet");
    match knob.kind {
        Kind::Enum { allowed, retired } => {
            assert_eq!(allowed, &["nvmet"]);
            assert_eq!(
                retired.len(),
                1,
                "exactly the one retired spelling: {retired:?}"
            );
            assert_eq!(retired[0].0, "spdk");
        }
        other => panic!("the target-stack knob stays an enum, got {other:?}"),
    }

    for spelling in ["spdk", "SPDK"] {
        let v = env_knobs::validate_vars([("SQUEEZEFS_NVMEOF_TARGET_STACK", spelling)]);
        assert_eq!(v.errors.len(), 1, "{v:?}");
        let msg = &v.errors[0];
        assert!(
            msg.contains("SQUEEZEFS_NVMEOF_TARGET_STACK") && msg.contains(spelling),
            "{msg}"
        );
        assert_names_retirement_and_reshare("startup gate", msg);
        assert!(
            !msg.contains("expected one of"),
            "the retired value gets the retirement text, not the generic enum refusal: {msg}"
        );
    }
    // A genuinely unknown word keeps the generic enum refusal.
    let v = env_knobs::validate_vars([("SQUEEZEFS_NVMEOF_TARGET_STACK", "banana")]);
    assert_eq!(v.errors.len(), 1, "{v:?}");
    assert!(
        v.errors[0].contains("expected one of nvmet"),
        "{}",
        v.errors[0]
    );
    assert!(env_knobs::validate_vars([("SQUEEZEFS_NVMEOF_TARGET_STACK", "nvmet")]).is_clean());
}

#[test]
fn the_spdk_only_knobs_are_retired_and_refuse() {
    for key in ["SQUEEZEFS_SPDK_TGT_BIN", "SQUEEZEFS_NVMEOF_RUN_DIR"] {
        let knob = env_knobs::lookup(key).unwrap_or_else(|| panic!("{key} stays registered"));
        assert!(
            matches!(knob.kind, Kind::Retired { .. }),
            "{key} must be Kind::Retired, got {:?}",
            knob.kind
        );
        let v = env_knobs::validate_vars([(key, "/var/tmp/spdk-scoping/build/bin/spdk_tgt")]);
        assert_eq!(v.errors.len(), 1, "{key} must refuse: {v:?}");
        let msg = &v.errors[0];
        assert!(msg.contains(key), "{msg}");
        assert!(
            msg.contains("SPDK") && msg.contains("nvmet"),
            "the refusal names the retirement and the successor target: {msg}"
        );
        assert!(v.unknown.is_empty(), "a retired name is known, not unknown");
        assert!(env_knobs::validate_vars([(key, "")]).errors.is_empty());
    }
}

// ---------------------------------------------------------------------------
// the binary
// ---------------------------------------------------------------------------

/// `--target-stack spdk` refuses on every verb that carries the flag —
/// at the grammar rung, before root, naming nvmet + the re-share sequence.
#[test]
fn the_binary_refuses_target_stack_spdk_on_every_verb_before_root() {
    for args in [
        vec![
            "nvmeof",
            "share",
            "/nonexistent/backing.img",
            "--ip",
            "127.0.0.1",
            "--target-stack",
            "spdk",
        ],
        vec!["nvmeof", "restore", "--target-stack", "spdk"],
        vec![
            "nvmeof",
            "adopt",
            "nqn.2026-06.io.foreign:x",
            "--target-stack",
            "spdk",
        ],
        vec!["nvmeof", "target", "setup", "--target-stack", "spdk"],
        vec!["nvmeof", "target", "start", "--target-stack", "spdk"],
        vec!["nvmeof", "target", "stop", "--target-stack", "spdk"],
        vec!["nvmeof", "target", "status", "--target-stack", "spdk"],
        vec!["nvmeof", "target", "systemd-unit", "--target-stack", "spdk"],
    ] {
        let out = run(&args, &[]);
        assert!(!out.status.success(), "{args:?} must refuse");
        let text = combined(&out);
        assert_names_retirement_and_reshare(&format!("{args:?}"), &text);
        assert!(
            !text.contains("root privileges"),
            "{args:?}: the retirement refusal fires before the root rung: {text}"
        );
        assert!(
            !text.contains("unexpected argument") && !text.contains("invalid value"),
            "{args:?}: the refusal is OURS, not clap's: {text}"
        );
    }
}

#[test]
fn the_help_advertises_only_nvmet() {
    let out = run(&["nvmeof", "share", "--help"], &[]);
    assert!(out.status.success());
    let help = combined(&out);
    assert!(help.contains("nvmet"), "{help}");
    assert!(
        !help.to_ascii_lowercase().contains("possible values: spdk")
            && !help.contains("default spdk")
            && !help.contains("SPDK only"),
        "spdk is not an advertised choice anymore: {help}"
    );
}

#[test]
fn the_binary_refuses_the_spdk_env_value_naming_nvmet() {
    let out = run(
        &[
            "nvmeof",
            "share",
            "/nonexistent/backing.img",
            "--ip",
            "127.0.0.1",
        ],
        &[("SQUEEZEFS_NVMEOF_TARGET_STACK", "spdk")],
    );
    assert!(!out.status.success());
    let text = combined(&out);
    assert!(
        text.contains("SQUEEZEFS_NVMEOF_TARGET_STACK") && text.contains("spdk"),
        "names the knob and the value: {text}"
    );
    assert_names_retirement_and_reshare("binary startup gate", &text);
}

/// `target install` existed only to build SPDK: a retired verb under the
/// `removed_verb()` convention — refuses loud naming its successor,
/// before the root rung, with or without its old flags.
#[test]
fn target_install_is_a_retired_verb_naming_its_successor() {
    for args in [
        vec!["nvmeof", "target", "install"],
        vec![
            "nvmeof",
            "target",
            "install",
            "--version",
            "v26.05",
            "--with-pkgdep",
        ],
    ] {
        let out = run(&args, &[]);
        assert!(!out.status.success(), "{args:?} must refuse");
        let text = combined(&out);
        assert!(
            text.contains("removed") || text.to_ascii_lowercase().contains("retired"),
            "{args:?}: says the verb is gone: {text}"
        );
        assert!(
            text.contains("target setup"),
            "{args:?}: names the successor verb: {text}"
        );
        assert!(
            !text.contains("root privileges") && !text.contains("unexpected argument"),
            "{args:?}: ours, before root: {text}"
        );
    }
    // Retired verbs stay out of the advertised grammar.
    let help = combined(&run(&["nvmeof", "target", "--help"], &[]));
    assert!(
        !help.contains("install"),
        "the retired verb is hidden from help: {help}"
    );
}

/// The SPDK-only flags are deleted, not silently accepted.
#[test]
fn the_spdk_only_flags_are_gone_from_the_grammar() {
    for args in [
        vec![
            "nvmeof",
            "share",
            "/nonexistent/backing.img",
            "--ip",
            "127.0.0.1",
            "--accept-version-drift",
        ],
        vec!["nvmeof", "unshare", "nqn.x", "--force"],
        vec!["nvmeof", "unshare", "nqn.x", "--accept-version-drift"],
        vec!["nvmeof", "restore", "--accept-version-drift"],
        vec!["nvmeof", "target", "setup", "--hugemem-mb", "512"],
        vec!["nvmeof", "target", "setup", "--restore-prior"],
        vec!["nvmeof", "target", "start", "--core-mask", "0x1"],
        vec!["nvmeof", "target", "start", "--cores", "2"],
        vec!["nvmeof", "target", "start", "--dpdk-mem-mb", "512"],
        vec!["nvmeof", "target", "stop", "--force"],
        vec!["nvmeof", "target", "systemd-unit", "--dpdk-mem-mb", "512"],
    ] {
        let out = run(&args, &[]);
        assert!(!out.status.success(), "{args:?} must fail");
        let text = combined(&out);
        assert!(
            text.contains("unexpected argument"),
            "{args:?}: a deleted flag dies on clap, never a silent accept: {text}"
        );
    }
}

/// With no flag the systemd unit is the nvmet oneshot (the default stack
/// is nvmet); the unit emission still mutates nothing and needs no root.
#[test]
fn systemd_unit_defaults_to_the_nvmet_oneshot() {
    let out = run(&["nvmeof", "target", "systemd-unit"], &[]);
    assert!(out.status.success(), "{}", combined(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Type=oneshot")
            && stdout.contains(&format!(
                "ExecStart={} nvmeof restore --target-stack nvmet",
                squeezefs_bin()
            )),
        "{stdout}"
    );
    assert!(
        !stdout.contains("spdk_tgt") && !stdout.contains("--target-stack spdk"),
        "{stdout}"
    );
}

/// `target stop` refuses on the ONE target (the kernel target is not a
/// process) — never a silent no-op — and names the teardown verb.
#[test]
fn target_stop_refuses_naming_unshare() {
    for args in [
        vec!["nvmeof", "target", "stop"],
        vec!["nvmeof", "target", "stop", "--target-stack", "nvmet"],
    ] {
        let out = run(&args, &[]);
        assert!(!out.status.success(), "{args:?}");
        let text = combined(&out);
        assert!(
            text.contains("not a process") && text.contains("nvmeof unshare"),
            "{args:?}: {text}"
        );
    }
}

// ---------------------------------------------------------------------------
// the module is gone (no-dead-code law, structurally)
// ---------------------------------------------------------------------------

#[test]
fn the_spdk_module_is_deleted_and_nothing_reaches_for_it() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(
        !root.join("src/nvmeof/spdk").exists(),
        "src/nvmeof/spdk/ must be deleted, not parked"
    );
    for entry in fs::read_dir(root.join("src/nvmeof")).expect("src/nvmeof") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let code: String = fs::read_to_string(&path)
            .expect("read")
            .lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        for needle in [
            "spdk::",
            "mod spdk",
            "SpdkStack",
            "SpdkPaths",
            "SpdkRpcClient",
        ] {
            assert!(
                !code.contains(needle),
                "{} still references SPDK machinery ('{needle}')",
                path.display()
            );
        }
    }
}
