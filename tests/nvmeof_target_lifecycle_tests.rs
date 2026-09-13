//! Kernel-nvmet target-lifecycle contract tests
//! (`docs/design-nvmeof-target-management.md` §6.5/§6.6): the
//! `target systemd-unit` golden emission (values baked, never
//! installed, no root) — the one lifecycle artifact the kernel target
//! has, since configfs is the "running target" and nothing is a
//! process. The SPDK lifecycle this suite used to pin (pinned build,
//! hugepages, pidfile, RPC preflights, the spdk unit) was RETIRED with
//! SPDK as a target (R-SYM-8, `docs/design-symmetric-metadata.md`
//! §5.8.1); its refusals live in `tests/nvmeof_retire_spdk_tests.rs`.
//!
//! The real setup→start→status cycle is the root tier
//! (`tests/run_nvmeof_fidelity.sh`).

use std::path::Path;
use std::process::{Command, Output};

use squeezefs::nvmeof::nvmet::render_nvmet_unit;

fn squeezefs_bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

fn run(args: &[&str]) -> Output {
    let state_dir = tempfile::tempdir().expect("state dir");
    Command::new(squeezefs_bin())
        .args(args)
        .env("SQUEEZEFS_NVMEOF_STATE_DIR", state_dir.path())
        .env_remove("SQUEEZEFS_NVMEOF_TARGET_STACK")
        .output()
        .expect("spawn squeezefs")
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

// ---------------------------------------------------------------------------
// systemd-unit golden emission (§6.5 — values BAKED, emitted never installed)
// ---------------------------------------------------------------------------

#[test]
fn test_nvmet_systemd_unit_golden() {
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

/// The unit bakes the squeezefs path via `/proc/self/exe`, goes to stdout
/// only, mutates nothing and needs no root — with the explicit flag and
/// with the default (nvmet) alike.
#[test]
fn test_systemd_unit_verb_bakes_the_binary_path_explicit_and_default() {
    for args in [
        vec![
            "nvmeof",
            "target",
            "systemd-unit",
            "--target-stack",
            "nvmet",
        ],
        vec!["nvmeof", "target", "systemd-unit"],
    ] {
        let out = run(&args);
        assert!(
            out.status.success(),
            "{args:?}: unit emission mutates nothing and needs no root: {}",
            combined(&out)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("Type=oneshot")
                && stdout.contains(&format!(
                    "ExecStart={} nvmeof restore --target-stack nvmet",
                    squeezefs_bin()
                )),
            "{args:?}: the squeezefs path is baked via /proc/self/exe: {stdout}"
        );
        assert!(
            stdout.starts_with("# squeezefs nvmeof target systemd-unit")
                && !stdout.to_ascii_lowercase().contains("warn"),
            "{args:?}: stdout carries ONLY the unit text (diagnostics go to stderr): {stdout}"
        );
    }
}
