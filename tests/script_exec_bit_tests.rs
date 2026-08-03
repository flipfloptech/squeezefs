//! Every tracked shell script under `tests/` and `docker/` must carry the
//! executable bit.
//!
//! Found 2026-08-02 by the ENG-process wave: `tests/run_require_mount_gate.sh`
//! was recorded `100644`, so `task check:require-mount` failed "Permission
//! denied" on any fresh checkout — and **23 more** carried the same defect,
//! including `run_fstests.sh`, `run_pjdfstests.sh` and `run_ltp_syscalls.sh`,
//! the three suites AGENTS.md documents as `sudo tests/run_*.sh` and names as
//! the mandatory release gate. A release gate that cannot start is worse than
//! one that fails: it fails at the moment you most need it, on a clean clone,
//! for a reason that looks like a permissions problem rather than a repo bug.
//!
//! The mode lives in the git index, not the worktree, so this reads
//! `git ls-files -s` rather than `std::fs` metadata — a checkout with
//! `core.fileMode=false` (this repo's dev box) shows 755 on disk regardless.

use std::process::Command;

#[test]
fn every_tracked_shell_script_is_executable() {
    let out = Command::new("git")
        .args(["ls-files", "-s", "--", "*.sh"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("git ls-files");
    assert!(out.status.success(), "git ls-files failed");

    let listing = String::from_utf8_lossy(&out.stdout);
    let offenders: Vec<&str> = listing
        .lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let mode = cols.next()?;
            // `<mode> <sha> <stage>\t<path>`
            let path = line.split('\t').nth(1)?;
            (mode == "100644").then_some(path)
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "{} tracked shell script(s) are missing the executable bit — a \
         documented `sudo tests/<script>.sh` invocation fails \"Permission \
         denied\" on a fresh checkout. Fix with `git update-index --chmod=+x \
         <path>` (the mode lives in the index; chmod alone does not record it \
         when core.fileMode=false):\n  {}",
        offenders.len(),
        offenders.join("\n  ")
    );
}
