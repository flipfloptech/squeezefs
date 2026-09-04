//! The cargo PROFILE NAME derivation — `#[path]`-shared between `build.rs`
//! (which stamps it into `SQUEEZEFS_BUILD_PROFILE`) and the crate (whose
//! tests pin it), so the two cannot drift.
//!
//! Cargo's `PROFILE` env collapses every custom profile to `debug` /
//! `release`, so the NAME is read from `OUT_DIR`'s layout:
//! `<target-dir>/<profile>/build/<pkg>-<hash>/out`. The two-profile LTO
//! law (2026-09-02) needs the binary to say WHICH release-class profile it
//! is, since only `dist` carries fat LTO.
//!
//! The anchor is the trailing `build/<pkg>-<hash>/out` triple, walked from
//! the END: the 1.2 `dist` artifacts reported `profile release` because the
//! container's target dir is `/build/target/…` and the first `build`
//! segment from the front was that directory, not the profile's.

/// The profile name for an `OUT_DIR`, or `None` when the path does not end
/// in `<profile>/build/<pkg>-<hash>/out` (the caller falls back to cargo's
/// collapsed `PROFILE`).
pub fn profile_name_from_out_dir(out_dir: &str) -> Option<String> {
    let parts: Vec<&str> = out_dir.split('/').filter(|s| !s.is_empty()).collect();
    let n = parts.len();
    if n < 4 || parts[n - 1] != "out" || parts[n - 3] != "build" {
        return None;
    }
    let profile = parts[n - 4];
    (!profile.is_empty()).then(|| profile.to_string())
}
