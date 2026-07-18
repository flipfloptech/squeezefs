//! Build identity — git-commit versioning (user policy, 2026-07-18).
//!
//! The version of a SqueezeFS build **is the git commit it was built from**:
//! no semver, no calver. Periodic releases are annotated `stable-YYYY.MM[.N]`
//! / `lts-YYYY.MM` git tags on specific commits — the tag names the release,
//! the commit stays the version. `build.rs` captures the identity at compile
//! time (`SQUEEZEFS_BUILD_{COMMIT,COMMIT_SHORT,DIRTY,TAG,TIMESTAMP}` rustc
//! envs, with packager env fallbacks and `unknown` for no-git tarball
//! builds); this module turns it into the one-line `--version` string, the
//! `.stats` `build_commit` / `build_tag` fields (the fleet mixed-version
//! detector), and nothing else. Operator surface: docs/operations.md
//! §Versioning & releases. Contract: tests/cli_version_tests.rs.

/// Format the one-line, grep-friendly, commit-first version string (without
/// the leading binary name — clap prepends `squeezefs `):
///
/// * untagged: `<short>[-dirty] (<full>[-dirty]) built <built_utc>`
/// * tagged:   `<tag> (<short>[-dirty] / <full>[-dirty]) built <built_utc>`
///
/// `-dirty` rides **both** hash forms — a dirty rebuild of a tagged commit
/// must never masquerade as the release.
pub fn format_version_line(
    _short: &str,
    _full: &str,
    _dirty: bool,
    _tag: &str,
    _built_utc: &str,
) -> String {
    unimplemented!("RED skeleton — feat commit implements the formatter")
}

/// The `.stats` `build_commit` form: the full hash, `-dirty` suffixed when
/// the build tree carried uncommitted changes.
pub fn format_build_commit(_full: &str, _dirty: bool) -> String {
    unimplemented!("RED skeleton — feat commit implements the formatter")
}

/// The embedded version line for THIS build — the single source of truth
/// wired verbatim into clap's `version` (so `--version` / `-V` print it).
pub fn version_line() -> &'static str {
    unimplemented!("RED skeleton — feat commit wires the build-script envs")
}

/// The embedded full build commit (with `-dirty` when applicable) — the
/// `.stats` `build_commit` value.
pub fn build_commit() -> String {
    unimplemented!("RED skeleton — feat commit wires the build-script envs")
}

/// The embedded exact release tag (`stable-*` / `lts-*`), empty when this
/// commit is not a release — the `.stats` `build_tag` value.
pub fn build_tag() -> &'static str {
    unimplemented!("RED skeleton — feat commit wires the build-script envs")
}
