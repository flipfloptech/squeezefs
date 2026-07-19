//! Build-time git identity for the shim — the KD-7 skew-gate key. The
//! daemon and the shim built from the same tree embed the same
//! `<full-hash>[-dirty]` identity (`src/version.rs` form); the session
//! HELLO refuses on inequality. Mirrors the root `build.rs` mechanism
//! (packager env fallbacks, `unknown` for no-git tarball builds) —
//! kept minimal: the shim needs only the commit + dirty bit.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=SQUEEZEFS_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=SQUEEZEFS_BUILD_DIRTY");

    let commit = std::env::var("SQUEEZEFS_BUILD_COMMIT")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| git(&["rev-parse", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = std::env::var("SQUEEZEFS_BUILD_DIRTY")
        .ok()
        .map(|v| v == "1")
        .or_else(|| git(&["status", "--porcelain", "--untracked-files=no"]).map(|s| !s.is_empty()))
        .unwrap_or(false);

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

    let suffix = if dirty { "-dirty" } else { "" };
    println!("cargo:rustc-env=SQUEEZEFS_IL_BUILD_COMMIT={commit}{suffix}");
}
