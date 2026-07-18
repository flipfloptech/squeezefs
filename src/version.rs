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

use std::sync::LazyLock;

/// Full build commit hash (40-hex), `unknown` on no-git tarball builds
/// without a packager override.
pub const BUILD_COMMIT: &str = env!("SQUEEZEFS_BUILD_COMMIT");
/// Short (12-hex) form of [`BUILD_COMMIT`].
pub const BUILD_COMMIT_SHORT: &str = env!("SQUEEZEFS_BUILD_COMMIT_SHORT");
/// Exact release tag on the build commit (`stable-*` / `lts-*`), empty when
/// the commit is not a release.
pub const BUILD_TAG: &str = env!("SQUEEZEFS_BUILD_TAG");
/// UTC RFC3339 build timestamp (second precision; honors
/// `SOURCE_DATE_EPOCH`).
pub const BUILD_TIMESTAMP: &str = env!("SQUEEZEFS_BUILD_TIMESTAMP");

/// Whether the build tree carried uncommitted tracked changes.
fn build_dirty() -> bool {
    env!("SQUEEZEFS_BUILD_DIRTY") == "1"
}

/// Format the one-line, grep-friendly, commit-first version string (without
/// the leading binary name — clap prepends `squeezefs `):
///
/// * untagged: `<short>[-dirty] (<full>[-dirty]) built <built_utc>`
/// * tagged:   `<tag> (<short>[-dirty] / <full>[-dirty]) built <built_utc>`
///
/// `-dirty` rides **both** hash forms — a dirty rebuild of a tagged commit
/// must never masquerade as the release.
pub fn format_version_line(
    short: &str,
    full: &str,
    dirty: bool,
    tag: &str,
    built_utc: &str,
) -> String {
    let d = if dirty { "-dirty" } else { "" };
    if tag.is_empty() {
        format!("{short}{d} ({full}{d}) built {built_utc}")
    } else {
        format!("{tag} ({short}{d} / {full}{d}) built {built_utc}")
    }
}

/// The `.stats` `build_commit` form: the full hash, `-dirty` suffixed when
/// the build tree carried uncommitted changes.
pub fn format_build_commit(full: &str, dirty: bool) -> String {
    let d = if dirty { "-dirty" } else { "" };
    format!("{full}{d}")
}

static VERSION_LINE: LazyLock<String> = LazyLock::new(|| {
    format_version_line(
        BUILD_COMMIT_SHORT,
        BUILD_COMMIT,
        build_dirty(),
        BUILD_TAG,
        BUILD_TIMESTAMP,
    )
});

/// The embedded version line for THIS build — the single source of truth
/// wired verbatim into clap's `version` (so `--version` / `-V` print it)
/// and every operator-facing version print.
pub fn version_line() -> &'static str {
    &VERSION_LINE
}

/// The embedded full build commit (with `-dirty` when applicable) — the
/// `.stats` `build_commit` value.
pub fn build_commit() -> String {
    format_build_commit(BUILD_COMMIT, build_dirty())
}

/// The embedded exact release tag (`stable-*` / `lts-*`), empty when this
/// commit is not a release — the `.stats` `build_tag` value.
pub fn build_tag() -> &'static str {
    BUILD_TAG
}
