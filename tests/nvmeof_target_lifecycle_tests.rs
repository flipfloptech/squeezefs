//! SPDK target-lifecycle contract tests
//! (`docs/design-nvmeof-target-management.md` §6.5/§6.3/§6.9, PR 3/N3):
//! pinned-version constants, hugepage reservation math + recorded-prior
//! semantics, the pidfile state machine, reactor core-mask math, the G3
//! loud-fail preflight messages (no binary / no hugepages / dead RPC /
//! version drift — the drift matrix lives in `nvmeof_rpc_tests`),
//! systemd-unit golden emission, and the G3 module-graph rule
//! (`src/nvmeof/spdk/` ↮ `nvmet` — zero cross-stack fallback paths by
//! construction).
//!
//! Everything runs unprivileged over injected roots (§6.8 relocation
//! seams: the production code pointed at tempdirs). The real
//! install→setup→start→status→stop cycle is the root tier
//! (`.agents/spdk-scoping/n3-spdk-gate.sh` until PR 5's harness).

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use squeezefs::nvmeof::spdk::hugepages::{
    pages_for_mb, preflight_free_for_dpdk, read_snapshot, recorded_prior,
    reservation_exceeds_warn_fraction, restore_prior, setup, HugepageSnapshot, DEFAULT_HUGEMEM_MB,
    HUGEPAGE_2M_MB, PRIOR_RECORD_FILE,
};
use squeezefs::nvmeof::spdk::lifecycle::{
    default_core_mask, mask_for_cores, parse_cpu_list, preflight_binary, preflight_rpc,
    read_pidfile_state, render_nvmet_unit, render_spdk_unit, toolchain_refusal_message,
    top_cores_mask, validate_install_version, PidfileState, ToolDep, SPDK_PINNED_COMMIT,
    SPDK_PINNED_TAG,
};
use squeezefs::nvmeof::spdk::SpdkPaths;

fn squeezefs_bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

fn run(args: &[&str], envs: &[(&str, &str)]) -> Output {
    let state_dir = tempfile::tempdir().expect("state dir");
    let run_dir = tempfile::tempdir().expect("run dir");
    let mut cmd = Command::new(squeezefs_bin());
    cmd.args(args)
        .env("SQUEEZEFS_NVMEOF_STATE_DIR", state_dir.path())
        .env("SQUEEZEFS_NVMEOF_RUN_DIR", run_dir.path())
        .env_remove("SQUEEZEFS_NVMEOF_TARGET_STACK")
        .env_remove("SQUEEZEFS_SPDK_TGT_BIN");
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

// ---------------------------------------------------------------------------
// pinned constants (§6.5)
// ---------------------------------------------------------------------------

#[test]
fn test_pinned_version_constants_shape() {
    assert_eq!(SPDK_PINNED_TAG, "v26.05");
    assert_eq!(
        SPDK_PINNED_COMMIT, "d519b163cbc0e2f28c35d9bc86d610da368b032c",
        "the pin is the FULL sha from the scoping provenance — never truncated"
    );
    assert_eq!(SPDK_PINNED_COMMIT.len(), 40);
    assert!(SPDK_PINNED_COMMIT.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn test_install_version_flag_must_equal_the_pin() {
    validate_install_version(None).expect("default = the pin");
    validate_install_version(Some("v26.05")).expect("explicit pin ok");
    let err = validate_install_version(Some("v27.01")).expect_err("pin bumps are deliberate PRs");
    let text = err.to_string();
    assert!(
        text.contains("v26.05") && text.contains("v27.01"),
        "must name both the pin and the request: {text}"
    );
    assert!(
        text.contains(SPDK_PINNED_COMMIT),
        "must carry the pinned sha (what install verifies): {text}"
    );
}

// ---------------------------------------------------------------------------
// hugepage math + recorded-prior semantics (§6.5)
// ---------------------------------------------------------------------------

fn write_hp(dir: &Path, total: u64, free: u64) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("nr_hugepages"), format!("{total}\n")).unwrap();
    fs::write(dir.join("free_hugepages"), format!("{free}\n")).unwrap();
}

#[test]
fn test_pages_for_mb_rounds_up() {
    assert_eq!(HUGEPAGE_2M_MB, 2);
    assert_eq!(pages_for_mb(2048), 1024, "the 2 GiB default = 1024 pages");
    assert_eq!(pages_for_mb(DEFAULT_HUGEMEM_MB), 1024);
    assert_eq!(pages_for_mb(1024), 512);
    assert_eq!(pages_for_mb(3), 2, "odd MiB rounds up");
    assert_eq!(pages_for_mb(0), 0);
}

#[test]
fn test_setup_records_prior_once_and_reserves() {
    let tmp = tempfile::tempdir().unwrap();
    let sysfs = tmp.path().join("hugepages-2048kB");
    let state = tmp.path().join("state-spdk");
    write_hp(&sysfs, 0, 0);

    let out = setup(&sysfs, &state, 2048, None).expect("first setup");
    assert_eq!(out.prior_recorded, Some(0), "prior recorded by this run");
    assert_eq!(out.requested_pages, 1024);
    assert_eq!(out.target_pages, 1024);
    assert_eq!(out.achieved_pages, 1024);
    assert!(!out.clamped_to_in_use);
    assert_eq!(
        fs::read_to_string(sysfs.join("nr_hugepages"))
            .unwrap()
            .trim(),
        "1024"
    );
    assert_eq!(
        recorded_prior(&state).unwrap(),
        Some(0),
        "the record file carries the pre-mutation value"
    );

    // Simulate the kernel: all reserved pages are free.
    write_hp(&sysfs, 1024, 1024);
    // Second setup with a smaller reservation: lowers (nothing in use),
    // but the prior record is WRITE-ONCE — it still says 0.
    let out = setup(&sysfs, &state, 1024, None).expect("second setup");
    assert_eq!(
        out.prior_recorded, None,
        "an existing record is never overwritten (write-once until restored)"
    );
    assert_eq!(out.achieved_pages, 512);
    assert_eq!(recorded_prior(&state).unwrap(), Some(0));
    assert_eq!(
        fs::read_to_string(sysfs.join("nr_hugepages"))
            .unwrap()
            .trim(),
        "512"
    );
}

#[test]
fn test_setup_is_idempotent_at_the_target() {
    let tmp = tempfile::tempdir().unwrap();
    let sysfs = tmp.path().join("hugepages-2048kB");
    let state = tmp.path().join("state-spdk");
    write_hp(&sysfs, 1024, 1024);
    let out = setup(&sysfs, &state, 2048, None).expect("setup at target");
    assert!(out.verified_noop, "already at target = verified no-op");
    assert_eq!(out.achieved_pages, 1024);
}

#[test]
fn test_setup_never_lowers_below_in_use() {
    let tmp = tempfile::tempdir().unwrap();
    let sysfs = tmp.path().join("hugepages-2048kB");
    let state = tmp.path().join("state-spdk");
    // 1024 pages, 24 free → 1000 in use by a running target.
    write_hp(&sysfs, 1024, 24);
    let out = setup(&sysfs, &state, 1024, None).expect("clamped setup");
    assert_eq!(out.requested_pages, 512);
    assert_eq!(out.target_pages, 1000, "clamped to in-use");
    assert!(out.clamped_to_in_use);
    assert!(
        out.warnings.iter().any(|w| w.contains("in use")),
        "the clamp is loud: {:?}",
        out.warnings
    );
    assert_eq!(
        fs::read_to_string(sysfs.join("nr_hugepages"))
            .unwrap()
            .trim(),
        "1000"
    );
}

#[test]
fn test_setup_warns_past_the_mem_available_fraction() {
    let tmp = tempfile::tempdir().unwrap();
    let sysfs = tmp.path().join("hugepages-2048kB");
    let state = tmp.path().join("state-spdk");
    write_hp(&sysfs, 0, 0);
    // 2 GiB reservation on a 4 GiB-available box: > 25 % (R2).
    let out = setup(&sysfs, &state, 2048, Some(4 * 1024 * 1024)).expect("setup");
    assert!(
        out.warnings.iter().any(|w| w.contains("MemAvailable")),
        "R2 warning expected: {:?}",
        out.warnings
    );
    assert!(reservation_exceeds_warn_fraction(2048, 4 * 1024 * 1024));
    assert!(!reservation_exceeds_warn_fraction(1024, 100 * 1024 * 1024));
}

#[test]
fn test_restore_prior_restores_and_clears_the_record() {
    let tmp = tempfile::tempdir().unwrap();
    let sysfs = tmp.path().join("hugepages-2048kB");
    let state = tmp.path().join("state-spdk");
    write_hp(&sysfs, 0, 0);
    setup(&sysfs, &state, 2048, None).expect("setup");
    write_hp(&sysfs, 1024, 1024); // kernel view after reservation

    let out = restore_prior(&sysfs, &state).expect("restore");
    assert_eq!(out.prior, 0);
    assert_eq!(
        fs::read_to_string(sysfs.join("nr_hugepages"))
            .unwrap()
            .trim(),
        "0"
    );
    assert!(
        !state.join(PRIOR_RECORD_FILE).exists(),
        "record cleared after restore"
    );
    let err = restore_prior(&sysfs, &state).expect_err("no record = loud refusal");
    assert!(
        err.to_string().contains("no recorded prior"),
        "must say why: {err}"
    );
}

#[test]
fn test_restore_prior_refuses_below_in_use() {
    let tmp = tempfile::tempdir().unwrap();
    let sysfs = tmp.path().join("hugepages-2048kB");
    let state = tmp.path().join("state-spdk");
    write_hp(&sysfs, 0, 0);
    setup(&sysfs, &state, 2048, None).expect("setup");
    // Target running: 1000 of 1024 pages in use.
    write_hp(&sysfs, 1024, 24);
    let err = restore_prior(&sysfs, &state).expect_err("would strand in-use pages");
    let text = err.to_string();
    assert!(
        text.contains("in use") && text.contains("target stop"),
        "refusal names the in-use pages and the stop-first remediation: {text}"
    );
    assert!(
        state.join(PRIOR_RECORD_FILE).exists(),
        "a refused restore keeps the record"
    );
}

#[test]
fn test_snapshot_reader_and_in_use_math() {
    let tmp = tempfile::tempdir().unwrap();
    let sysfs = tmp.path().join("hugepages-2048kB");
    write_hp(&sysfs, 1024, 900);
    let snap = read_snapshot(&sysfs).expect("read");
    assert_eq!(
        snap,
        HugepageSnapshot {
            total: 1024,
            free: 900
        }
    );
    assert_eq!(snap.in_use(), 124);
}

#[test]
fn test_preflight_free_pages_math_and_message() {
    let tmp = tempfile::tempdir().unwrap();
    let sysfs = tmp.path().join("hugepages-2048kB");
    write_hp(&sysfs, 1024, 100);
    // -s 1024 needs 512 free 2M pages; only 100 free.
    let err = preflight_free_for_dpdk(&sysfs, 1024).expect_err("G3 no-hugepages");
    let text = err.to_string();
    assert!(
        text.contains("512") && text.contains("100"),
        "the message carries the arithmetic (needed vs free): {text}"
    );
    assert!(
        text.contains("target setup") && text.contains("--hugemem-mb"),
        "remediation names the setup verb: {text}"
    );
    write_hp(&sysfs, 1024, 600);
    preflight_free_for_dpdk(&sysfs, 1024).expect("600 free ≥ 512 needed");
}

// ---------------------------------------------------------------------------
// pidfile state machine (§6.5 pidfile direct mode)
// ---------------------------------------------------------------------------

#[test]
fn test_pidfile_state_machine() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile = tmp.path().join("spdk_tgt.pid");

    assert_eq!(
        read_pidfile_state(&pidfile).expect("absent is a state"),
        PidfileState::NotRunning
    );

    // Our own pid is alive.
    fs::write(&pidfile, format!("{}\n", std::process::id())).unwrap();
    assert_eq!(
        read_pidfile_state(&pidfile).expect("live pid"),
        PidfileState::Running(std::process::id() as i32)
    );

    // A reaped child pid is stale.
    let child = Command::new("true")
        .spawn()
        .expect("spawn")
        .wait_with_output();
    let dead_pid = {
        let mut c = Command::new("true").spawn().expect("spawn");
        let pid = c.id() as i32;
        c.wait().expect("reap");
        pid
    };
    drop(child);
    fs::write(&pidfile, format!("{dead_pid}\n")).unwrap();
    assert_eq!(
        read_pidfile_state(&pidfile).expect("dead pid"),
        PidfileState::Stale(dead_pid)
    );

    // Corrupt content is an error, never guessed around.
    fs::write(&pidfile, "not-a-pid\n").unwrap();
    read_pidfile_state(&pidfile).expect_err("corrupt pidfile refuses");
}

// ---------------------------------------------------------------------------
// reactor core-mask math (§6.5)
// ---------------------------------------------------------------------------

#[test]
fn test_core_mask_math() {
    assert_eq!(
        parse_cpu_list("0-31").unwrap(),
        (0..=31).collect::<Vec<_>>()
    );
    assert_eq!(
        parse_cpu_list("0-3,8,10-11").unwrap(),
        vec![0, 1, 2, 3, 8, 10, 11]
    );
    assert_eq!(parse_cpu_list("24").unwrap(), vec![24]);
    parse_cpu_list("").expect_err("empty refuses");
    parse_cpu_list("3-1").expect_err("inverted range refuses");

    assert_eq!(mask_for_cores(&[31]).unwrap(), "0x80000000");
    assert_eq!(
        mask_for_cores(&[24]).unwrap(),
        "0x1000000",
        "the scoping mask"
    );
    assert_eq!(mask_for_cores(&[30, 31]).unwrap(), "0xc0000000");
    assert_eq!(mask_for_cores(&[0]).unwrap(), "0x1");
    // Arbitrary width: cpu 127 renders, never saturates at u64.
    let wide = mask_for_cores(&[127]).unwrap();
    assert_eq!(wide, format!("0x8{}", "0".repeat(31)));
    mask_for_cores(&[]).expect_err("no cores refuses");

    let online: Vec<u32> = (0..=31).collect();
    assert_eq!(
        default_core_mask(&online).unwrap(),
        "0x80000000",
        "§6.5 default: the highest online CPU"
    );
    assert_eq!(top_cores_mask(&online, 2).unwrap(), "0xc0000000");
    top_cores_mask(&online, 0).expect_err("zero cores refuses");
    top_cores_mask(&[0, 1], 4).expect_err("more cores than online refuses");
}

// ---------------------------------------------------------------------------
// G3 loud-fail matrix: binary / RPC rungs (drift + hugepages live above /
// in nvmeof_rpc_tests)
// ---------------------------------------------------------------------------

fn tmp_paths(tmp: &Path) -> SpdkPaths {
    SpdkPaths::with_roots(tmp.join("opt-prefix"), tmp.join("run"), tmp.join("state"))
}

#[test]
fn test_preflight_binary_missing_names_target_install() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = tmp_paths(tmp.path());
    let err = preflight_binary(&paths).expect_err("no pinned binary");
    let text = err.to_string();
    assert!(
        text.contains("target install"),
        "remediation names the install verb: {text}"
    );
    assert!(
        text.contains(SPDK_PINNED_TAG),
        "names the pinned release: {text}"
    );
    assert!(
        text.contains(&paths.pinned_bin().display().to_string()),
        "names the expected path: {text}"
    );
}

#[test]
fn test_preflight_binary_override_semantics() {
    let tmp = tempfile::tempdir().unwrap();
    let mut paths = tmp_paths(tmp.path());

    // Set-but-missing override: loud, never a silent fallthrough.
    paths.bin_override = Some(tmp.path().join("missing-spdk-tgt"));
    let err = preflight_binary(&paths).expect_err("missing override refuses");
    assert!(
        err.to_string().contains("SQUEEZEFS_SPDK_TGT_BIN"),
        "names the env override: {err}"
    );

    // Present override: proceeds WITH the loud unpinned warning.
    let fake = tmp.path().join("fake-spdk-tgt");
    fs::write(&fake, "#!/bin/sh\n").unwrap();
    paths.bin_override = Some(fake.clone());
    let (bin, warning) = preflight_binary(&paths).expect("override present");
    assert_eq!(bin, fake);
    let warning = warning.expect("the override is never silent");
    assert!(
        warning.contains("unpinned") && warning.contains("SQUEEZEFS_SPDK_TGT_BIN"),
        "the §6.2 loud unpinned warning: {warning}"
    );

    // Pinned binary present, no override: silent pass.
    paths.bin_override = None;
    let pinned = paths.pinned_bin();
    fs::create_dir_all(pinned.parent().unwrap()).unwrap();
    fs::write(&pinned, "#!/bin/sh\n").unwrap();
    let (bin, warning) = preflight_binary(&paths).expect("pinned present");
    assert_eq!(bin, pinned);
    assert!(warning.is_none(), "the pin carries no warning");
}

#[test]
fn test_preflight_rpc_dead_message_is_the_designed_runbook() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = tmp_paths(tmp.path());
    let err = preflight_rpc(&paths).expect_err("no socket = dead RPC");
    let text = err.to_string();
    // The §6.2 designed message, rung 3.
    assert!(
        text.contains("SPDK target stack unavailable"),
        "names the unavailable stack: {text}"
    );
    assert!(
        text.contains("RPC socket") && text.contains("not answering"),
        "names the condition: {text}"
    );
    assert!(
        text.contains(&paths.rpc_sock().display().to_string()),
        "names the socket path: {text}"
    );
    assert!(
        text.contains("nvmeof target status") && text.contains("nvmeof target start"),
        "check/start remediation verbs: {text}"
    );
    assert!(
        text.contains("--target-stack nvmet"),
        "the explicit kernel-stack choice appears as operator guidance: {text}"
    );
    assert!(
        text.contains("never falls back between target stacks"),
        "the no-silent-fallback law: {text}"
    );
}

// ---------------------------------------------------------------------------
// toolchain consent refusal (§6.5 — no system mutation without consent)
// ---------------------------------------------------------------------------

#[test]
fn test_toolchain_refusal_message_lists_deps_and_names_the_consent_flag() {
    let missing = [
        ToolDep {
            probe: "gcc",
            package_hint: "gcc (build-essential / gcc)",
        },
        ToolDep {
            probe: "/usr/include/libaio.h",
            package_hint: "libaio-dev / libaio-devel",
        },
    ];
    let text = toolchain_refusal_message(&missing);
    assert!(
        text.contains("gcc") && text.contains("libaio"),
        "every missing dep is listed: {text}"
    );
    assert!(
        text.contains("build-essential") && text.contains("libaio-dev"),
        "package hints included: {text}"
    );
    assert!(
        text.contains("--with-pkgdep"),
        "names the explicit consent flag: {text}"
    );
    assert!(
        text.contains("never mutates system packages without"),
        "states the no-mutation default: {text}"
    );
}

// ---------------------------------------------------------------------------
// systemd-unit emission (§6.5 golden text — values baked, never installed)
// ---------------------------------------------------------------------------

#[test]
fn test_spdk_systemd_unit_golden() {
    let unit = render_spdk_unit(
        Path::new("/opt/squeezefs/spdk/v26.05/build/bin/spdk_tgt"),
        Path::new("/run/squeezefs/nvmeof/spdk.sock"),
        "0x80000000",
        1024,
        Path::new("/usr/local/bin/squeezefs"),
    );
    let expected_body = "\
[Unit]
Description=SqueezeFS-managed SPDK NVMe-oF target (pinned v26.05)
Wants=network-online.target
After=network-online.target
StartLimitIntervalSec=60
StartLimitBurst=5
[Service]
Type=simple
ExecStart=/opt/squeezefs/spdk/v26.05/build/bin/spdk_tgt -r /run/squeezefs/nvmeof/spdk.sock -m 0x80000000 -s 1024
ExecStartPost=/usr/local/bin/squeezefs nvmeof restore --target-stack spdk
Restart=always
RestartSec=2
LimitMEMLOCK=infinity
RuntimeDirectory=squeezefs/nvmeof
RuntimeDirectoryMode=0700
[Install]
WantedBy=multi-user.target
";
    assert!(
        unit.ends_with(expected_body),
        "the §6.5 unit body must render verbatim after the comment header.\n--- got ---\n{unit}\n--- want tail ---\n{expected_body}"
    );
    assert!(
        unit.starts_with('#'),
        "a comment header explains baked values + operator installs: {unit}"
    );
    assert!(
        unit.contains("BAKED"),
        "header states the baked-values law: {unit}"
    );
    assert!(!unit.contains("${"), "no ${{VAR}} indirection ever: {unit}");
}

#[test]
fn test_nvmet_systemd_unit_variant() {
    let unit = render_nvmet_unit(Path::new("/usr/local/bin/squeezefs"));
    for needle in [
        "[Unit]",
        "Wants=network-online.target",
        "After=network-online.target",
        "[Service]",
        "Type=oneshot",
        "RemainAfterExit=yes",
        "ExecStart=/usr/local/bin/squeezefs nvmeof restore --target-stack nvmet",
        "[Install]",
        "WantedBy=multi-user.target",
    ] {
        assert!(
            unit.contains(needle),
            "nvmet unit must carry '{needle}': {unit}"
        );
    }
    assert!(!unit.contains("${"), "no ${{VAR}} indirection ever: {unit}");
}

// ---------------------------------------------------------------------------
// systemd-unit through the real binary (baked /proc/self/exe, stdout only)
// ---------------------------------------------------------------------------

#[test]
fn test_systemd_unit_verb_bakes_values_and_warns_on_override() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = tmp.path().join("fake-spdk-tgt");
    fs::write(&fake, "#!/bin/sh\n").unwrap();
    let out = run(
        &[
            "nvmeof",
            "target",
            "systemd-unit",
            "--core-mask",
            "0x1000000",
            "--dpdk-mem-mb",
            "512",
        ],
        &[("SQUEEZEFS_SPDK_TGT_BIN", fake.to_str().unwrap())],
    );
    assert!(
        out.status.success(),
        "unit emission mutates nothing and needs no root: {}",
        combined(&out)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains(&format!("ExecStart={} -r ", fake.display())),
        "the override binary is baked into ExecStart: {stdout}"
    );
    assert!(
        stdout.contains("-m 0x1000000 -s 512"),
        "flags are baked verbatim: {stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "ExecStartPost={} nvmeof restore --target-stack spdk",
            squeezefs_bin()
        )),
        "the squeezefs path is baked via /proc/self/exe: {stdout}"
    );
    assert!(
        stderr.contains("unpinned"),
        "the unpinned-override warning goes to stderr (stdout stays a valid unit): {stderr}"
    );
    assert!(
        !stdout.contains("unpinned"),
        "stdout carries ONLY the unit text: {stdout}"
    );
}

#[test]
fn test_systemd_unit_verb_refuses_without_a_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("missing-spdk-tgt");
    let out = run(
        &["nvmeof", "target", "systemd-unit"],
        &[("SQUEEZEFS_SPDK_TGT_BIN", missing.to_str().unwrap())],
    );
    assert!(
        !out.status.success(),
        "a unit whose ExecStart cannot exist is refused, not emitted"
    );
    assert!(
        combined(&out).contains("SQUEEZEFS_SPDK_TGT_BIN"),
        "names the missing override: {}",
        combined(&out)
    );
}

#[test]
fn test_systemd_unit_nvmet_variant_through_the_binary() {
    let out = run(
        &[
            "nvmeof",
            "target",
            "systemd-unit",
            "--target-stack",
            "nvmet",
        ],
        &[],
    );
    assert!(out.status.success(), "{}", combined(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Type=oneshot") && stdout.contains("restore --target-stack nvmet"),
        "nvmet unit emitted: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// G3 module-graph rule: zero cross-stack references (spdk/ ↮ nvmet)
// ---------------------------------------------------------------------------

/// Strip `//` line comments so doc references don't false-positive.
fn code_of(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn test_module_graph_rule_no_cross_stack_references() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/nvmeof");
    for entry in fs::read_dir(root.join("spdk")).expect("spdk module dir") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let code = code_of(&path);
        for needle in ["nvmet::", "use super::nvmet", "crate::nvmeof::nvmet"] {
            assert!(
                !code.contains(needle),
                "{} references nvmet items ('{needle}') — the G3 module-graph rule forbids \
                 cross-stack paths; dispatch lives in src/nvmeof/mod.rs only",
                path.display()
            );
        }
    }
    let nvmet = code_of(&root.join("nvmet.rs"));
    for needle in ["spdk::", "use super::spdk", "crate::nvmeof::spdk"] {
        assert!(
            !nvmet.contains(needle),
            "nvmet.rs references spdk items ('{needle}') — the G3 module-graph rule forbids \
             cross-stack paths"
        );
    }
}
