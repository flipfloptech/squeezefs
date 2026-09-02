//! Build identity — release-train + git-commit versioning (user policy,
//! 2026-07-24, superseding the commit-only 2026-07-18 policy).
//!
//! A SqueezeFS build carries **two identities, both surfaced**: the
//! **release-train version** ([`RELEASE_TRAIN`] = `CARGO_PKG_VERSION`,
//! bumped in `Cargo.toml` as a release act — currently the 1.1 train; the
//! first-party `fuse3` fork and the preload/ipc crates track the same
//! train) and the **git commit the build was produced from** (the
//! fine-grained identity). Periodic releases remain annotated
//! `stable-YYYY.MM[.N]` / `lts-YYYY.MM` git tags on specific commits — the
//! tag names the release, the commit pins the exact build. `build.rs`
//! captures the commit identity at compile time
//! (`SQUEEZEFS_BUILD_{COMMIT,COMMIT_SHORT,DIRTY,TAG,TIMESTAMP}` rustc envs,
//! with packager env fallbacks and `unknown` for no-git tarball builds);
//! this module turns both into the one-line `--version` string, the
//! `.stats` `build_commit` / `build_tag` fields (the fleet mixed-version
//! detector), and nothing else. Operator surface: docs/operations.md
//! §Versioning & releases. Contract: tests/cli_version_tests.rs.

use std::sync::LazyLock;

/// The release-train version — `CARGO_PKG_VERSION`, bumped as a release
/// act (docs/operations.md §Versioning & releases).
pub const RELEASE_TRAIN: &str = env!("CARGO_PKG_VERSION");

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
/// The cargo profile NAME this binary was built under (`release` = thin
/// LTO, the dev/field/gate profile; `dist` = fat LTO + one codegen unit,
/// tagged releases only — the two-profile LTO law, 2026-09-02). Surfaced
/// so a measurement row can never silently mix the two.
pub const BUILD_PROFILE: &str = env!("SQUEEZEFS_BUILD_PROFILE");

/// Whether the build tree carried uncommitted tracked changes.
fn build_dirty() -> bool {
    env!("SQUEEZEFS_BUILD_DIRTY") == "1"
}

/// Format the one-line, grep-friendly, train-first version string (without
/// the leading binary name — clap prepends `squeezefs `):
///
/// * untagged: `<train> (<short>[-dirty] / <full>[-dirty]) built <built_utc> profile <profile>`
/// * tagged:   `<train> (<short>[-dirty] / <full>[-dirty], tag <tag>) built <built_utc> profile <profile>`
///
/// The release-train version leads; the commit identity survives verbatim.
/// `-dirty` rides **both** hash forms — a dirty rebuild of a tagged commit
/// must never masquerade as the release. The profile trails (the
/// two-profile LTO law): a `release` binary and a `dist` binary are
/// different measurement subjects.
pub fn format_version_line(
    train: &str,
    short: &str,
    full: &str,
    dirty: bool,
    tag: &str,
    built_utc: &str,
    profile: &str,
) -> String {
    let d = if dirty { "-dirty" } else { "" };
    if tag.is_empty() {
        format!("{train} ({short}{d} / {full}{d}) built {built_utc} profile {profile}")
    } else {
        format!("{train} ({short}{d} / {full}{d}, tag {tag}) built {built_utc} profile {profile}")
    }
}

/// The `.stats` `build_commit` form: the full hash, `-dirty` suffixed when
/// the build tree carried uncommitted changes.
pub fn format_build_commit(full: &str, dirty: bool) -> String {
    let d = if dirty { "-dirty" } else { "" };
    format!("{full}{d}")
}

/// ENG-8 (pre-RC spec §10): the build features that make THIS binary unfit
/// for measurement, in the order they are reported.
///
/// * `dhat-on` replaces the global allocator with `dhat::Alloc`, which
///   removes jemalloc **and** the `dirty_decay_ms:1000` tuning `main.rs`
///   documents as load-bearing for R1b liveness. Every throughput, IOPS,
///   latency and RSS figure from such a build describes a different
///   allocator than the shipped one.
/// * `coz-on` arms the `coz_progress!` instrumentation points.
///
/// Neither is compiled by `cargo build --release` (the shipped and
/// rig-built configuration); both are compiled by `--all-features`, which
/// is why the gate now lints the default configuration separately and why
/// this binary says so about itself.
pub fn measurement_disqualifiers() -> &'static [&'static str] {
    &[
        #[cfg(feature = "dhat-on")]
        "dhat-on",
        #[cfg(feature = "coz-on")]
        "coz-on",
    ]
}

/// Whether this build may carry a performance number at all (ENG-8).
pub fn measurement_valid() -> bool {
    measurement_disqualifiers().is_empty()
}

/// The profiling notice appended to the version line, `None` for a
/// measurement-valid build (so the shipped `--version` shape is unchanged,
/// byte for byte — `tests/cli_version_tests.rs`).
pub fn format_profiling_notice(disqualifiers: &[&str]) -> Option<String> {
    if disqualifiers.is_empty() {
        return None;
    }
    Some(format!(
        " [PROFILING BUILD: {} — NOT measurement-valid]",
        disqualifiers.join("+")
    ))
}

/// The one-line loud warning the daemon and `squeezefs bench` emit when a
/// profiling build is used for anything that produces a number (ENG-8).
pub fn profiling_build_warning(disqualifiers: &[&str]) -> Option<String> {
    if disqualifiers.is_empty() {
        return None;
    }
    Some(format!(
        "PROFILING BUILD ({}): this binary is built with a profiling feature — \
         dhat-on replaces jemalloc (and its load-bearing dirty_decay_ms:1000 \
         tuning), coz-on arms instrumentation points. Numbers from this build \
         are NOT measurement-valid; rebuild with `cargo build --release` \
         (default features) before recording any performance evidence.",
        disqualifiers.join("+")
    ))
}

static VERSION_LINE: LazyLock<String> = LazyLock::new(|| {
    let base = format_version_line(
        RELEASE_TRAIN,
        BUILD_COMMIT_SHORT,
        BUILD_COMMIT,
        build_dirty(),
        BUILD_TAG,
        BUILD_TIMESTAMP,
        BUILD_PROFILE,
    );
    match format_profiling_notice(measurement_disqualifiers()) {
        Some(notice) => format!("{base}{notice}"),
        None => base,
    }
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

/// The embedded cargo profile name — the `.stats` `build_profile` value.
pub fn build_profile() -> &'static str {
    BUILD_PROFILE
}
