//! Build-time git identity capture — git-commit versioning (user policy,
//! 2026-07-18): the version of a build IS the commit it was built from.
//!
//! Emits the `SQUEEZEFS_BUILD_*` rustc envs consumed by `src/version.rs`
//! (the `--version` line, the `.stats` `build_commit`/`build_tag` fields).
//! Never fails the build over versioning: without usable git (tarball
//! builds) it honors the packager envs `SQUEEZEFS_BUILD_COMMIT` /
//! `SQUEEZEFS_BUILD_TAG`, else embeds `unknown`.
//!
//! Rerun policy (the standard pattern): watch `.git/HEAD` + `.git/refs`
//! (+ `packed-refs`) so new commits/tags/checkouts re-capture, while plain
//! source edits do NOT rerun this script — no rebuild thrash. Consequence:
//! the dirty flag is captured when this script runs, not per source edit
//! (the no-thrash trade every git-versioned build makes).

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// (full, short12, dirty, exact_tag) from git, or None when git is
/// unavailable / not a repo / no commits yet.
fn git_identity() -> Option<(String, String, bool, String)> {
    let full = git(&["rev-parse", "HEAD"])?;
    let short = git(&["rev-parse", "--short=12", "HEAD"])?;
    // Tracked modifications only (`git describe --dirty` semantics):
    // untracked scratch files must not stamp a release binary -dirty.
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    // Exact release tag on HEAD (`stable-*` / `lts-*` release acts);
    // empty when this commit is not a release.
    let tag = git(&["describe", "--tags", "--exact-match", "HEAD"]).unwrap_or_default();

    // Rerun hints: HEAD lives in the (worktree) git dir; refs and
    // packed-refs live in the common dir. Emit only paths that exist —
    // a missing rerun-if-changed path would rerun the script every build.
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        let common = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .unwrap_or_else(|| git_dir.clone());
        for path in [
            format!("{git_dir}/HEAD"),
            format!("{common}/refs"),
            format!("{common}/packed-refs"),
        ] {
            if std::path::Path::new(&path).exists() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
    Some((full, short, dirty, tag))
}

/// UTC RFC3339 (second precision) without pulling a date dependency into
/// the build graph. Honors `SOURCE_DATE_EPOCH` (reproducible-builds
/// convention) when set and parseable.
fn utc_rfc3339_now() -> String {
    let secs = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });
    // Howard Hinnant's civil_from_days (days -> y/m/d, proleptic Gregorian).
    let z = (secs / 86_400) as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe as i64 + era * 400 + i64::from(month <= 2);
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

fn main() {
    // Packager fallback envs participate in the fingerprint so overridden
    // tarball builds re-stamp when the override changes.
    println!("cargo:rerun-if-env-changed=SQUEEZEFS_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=SQUEEZEFS_BUILD_TAG");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let (full, short, dirty, tag) = git_identity().unwrap_or_else(|| {
        // No-git fallback (tarball builds): packager envs, else `unknown`.
        // Never fail the build over versioning.
        let full = std::env::var("SQUEEZEFS_BUILD_COMMIT")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string());
        let short = if full == "unknown" {
            full.clone()
        } else {
            full.chars().take(12).collect()
        };
        let tag = std::env::var("SQUEEZEFS_BUILD_TAG")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        (full, short, false, tag)
    });

    println!("cargo:rustc-env=SQUEEZEFS_BUILD_COMMIT={full}");
    println!("cargo:rustc-env=SQUEEZEFS_BUILD_COMMIT_SHORT={short}");
    println!(
        "cargo:rustc-env=SQUEEZEFS_BUILD_DIRTY={}",
        if dirty { "1" } else { "0" }
    );
    println!("cargo:rustc-env=SQUEEZEFS_BUILD_TAG={tag}");
    println!(
        "cargo:rustc-env=SQUEEZEFS_BUILD_TIMESTAMP={}",
        utc_rfc3339_now()
    );
    println!(
        "cargo:rustc-env=SQUEEZEFS_BUILD_PROFILE={}",
        build_profile_name()
    );
}

/// The derivation is shared with the crate (`src/build_profile_core.rs`) so
/// `tests/cli_version_tests.rs` pins the exact function that stamps the
/// binary — including the containerized `/build/target/…` layout that made
/// the 1.2 `dist` artifacts report `profile release`.
#[path = "src/build_profile_core.rs"]
mod build_profile_core;

/// The cargo PROFILE NAME this binary is built under (`release`, `dist`,
/// `dev`, `preload-release`, …), read from `OUT_DIR`'s layout because
/// cargo's `PROFILE` env collapses every custom profile to `debug` /
/// `release` — the two-profile LTO law needs the binary to say WHICH
/// release-class profile it is, since only `dist` carries fat LTO. Falls
/// back to the collapsed `PROFILE` when `OUT_DIR` has no recognizable
/// shape, never guesses.
fn build_profile_name() -> String {
    let out_dir = std::env::var("OUT_DIR").unwrap_or_default();
    build_profile_core::profile_name_from_out_dir(&out_dir)
        .unwrap_or_else(|| std::env::var("PROFILE").unwrap_or_else(|_| "unknown".into()))
}
