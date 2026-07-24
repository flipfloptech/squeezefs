//! Versioning contract (user policy, 2026-07-24, superseding 2026-07-18):
//! the version line carries BOTH identities — the **release-train version**
//! (`CARGO_PKG_VERSION`, bumped as a release act; currently the 1.1 train)
//! leads, and the **git commit the build was produced from** survives
//! verbatim (short + full hash, `-dirty`, exact `stable-*`/`lts-*` tag).
//! Git commits remain the fine-grained identity; tags remain the release
//! names. Policy + operator surface: docs/operations.md §Versioning &
//! releases.
//!
//! Pinned here:
//! * `squeezefs --version` / `-V` print one grep-friendly, train-first line:
//!   - untagged: `squeezefs <train> (<short12>[-dirty] / <full40>[-dirty]) built <utc-rfc3339>`
//!   - tagged:   `squeezefs <train> (<short12>[-dirty] / <full40>[-dirty], tag <tag>) built <utc-rfc3339>`
//! * The train version is `CARGO_PKG_VERSION` and is no longer the retired
//!   `0.1.0` cargo placeholder.
//! * A binary built from this repo embeds a REAL hash (never the `unknown`
//!   tarball fallback) that matches `git rev-parse HEAD` at build time.
//! * The dirty and tag branches are pinned at the formatting-function level
//!   with injected values (the git capture itself is build-time `build.rs`
//!   work — tests never create tags or dirty state).
//! * The daemon stats JSON exports `build_commit` (full, `-dirty` suffixed
//!   when applicable) and `build_tag` (always present, empty string when
//!   untagged — the always-export stats convention): the fleet mixed-version
//!   detector.
//!
//! CLI tests drive the real binary (`CARGO_BIN_EXE_squeezefs`), the house
//! convention for CLI contracts.

use regex::Regex;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

/// The one-line version-output shape, both branches: the release-train
/// version leads, the commit pair (short / full, `-dirty` on both when
/// applicable) rides in the parens, the exact release tag (when present)
/// trails inside the parens as `, tag <tag>`.
fn version_line_regex() -> Regex {
    Regex::new(
        r"(?x)^squeezefs\ \d+\.\d+\.\d+\ \(
            [0-9a-f]{12}(?:-dirty)?\ /\ [0-9a-f]{40}(?:-dirty)?
            (?:,\ tag\ \S+)?
          \)\ built\ \d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$",
    )
    .expect("version-line regex compiles")
}

fn run_version(flag: &str) -> String {
    let out = Command::new(bin())
        .arg(flag)
        .output()
        .expect("spawn squeezefs binary");
    assert!(
        out.status.success(),
        "`squeezefs {flag}` must exit 0 (status {:?}, stderr: {})",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("version output is UTF-8")
}

/// `--version` prints exactly one line in the train-first shape, and the
/// embedded commit is real: 12-hex short + 40-hex full + UTC RFC3339
/// build timestamp, never the `unknown` no-git fallback (this repo HAS git).
#[test]
fn version_flag_prints_one_train_first_line() {
    let stdout = run_version("--version");
    let line = stdout.trim_end_matches('\n');
    assert!(
        !line.contains('\n'),
        "--version output must be a single line, got: {stdout:?}"
    );
    assert!(
        version_line_regex().is_match(line),
        "--version line must match the train-first format \
         `squeezefs <train> (<short12>[-dirty] / <full40>[-dirty]\
         [, tag <tag>]) built <ts>`, got: {line:?}"
    );
    assert!(
        !line.contains("unknown"),
        "a binary built from this git repo must embed a real commit, \
         not the tarball `unknown` fallback: {line:?}"
    );
}

/// The line leads with the release-train version — `CARGO_PKG_VERSION`,
/// which now tracks the release train (bumped as a release act) and is no
/// longer the retired `0.1.0` cargo-internal placeholder.
#[test]
fn version_line_leads_with_the_release_train() {
    let train = env!("CARGO_PKG_VERSION");
    assert_ne!(
        train, "0.1.0",
        "the package version must track the release train (currently the \
         1.1 train), not the retired 0.1.0 placeholder"
    );
    assert_eq!(
        train,
        squeezefs::version::RELEASE_TRAIN,
        "RELEASE_TRAIN must be CARGO_PKG_VERSION verbatim"
    );
    let stdout = run_version("--version");
    let prefix = format!("squeezefs {train} (");
    assert!(
        stdout.starts_with(&prefix),
        "--version must lead with the release-train version \
         (expected prefix {prefix:?}), got: {stdout:?}"
    );
}

/// `-V` is the same version surface: byte-identical output to `--version`.
#[test]
fn short_version_flag_matches_long_flag() {
    assert_eq!(
        run_version("-V"),
        run_version("--version"),
        "-V and --version must print the identical version line"
    );
}

/// The full 40-hex hash embedded in the binary is the commit the build was
/// produced from — cross-checked against `git rev-parse HEAD` of this repo.
/// (Skips cleanly when the sources have no usable git, e.g. tarball runs.)
#[test]
fn version_embeds_the_real_build_commit() {
    let repo = env!("CARGO_MANIFEST_DIR");
    let head = match Command::new("git")
        .args(["-C", repo, "rev-parse", "HEAD"])
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => {
            eprintln!("[SKIP] no usable git in {repo}; cannot cross-check HEAD");
            return;
        }
    };
    let stdout = run_version("--version");
    let full = Regex::new(r"[0-9a-f]{40}")
        .expect("40-hex regex compiles")
        .find(&stdout)
        .unwrap_or_else(|| panic!("--version must carry a full 40-hex commit, got: {stdout:?}"))
        .as_str();
    assert_eq!(
        full, head,
        "--version's full hash must be the commit the binary was built from \
         (`git rev-parse HEAD`), got: {stdout:?}"
    );
    let short = &head[..12];
    assert!(
        stdout.contains(short),
        "--version must carry the 12-hex short form {short} of the build \
         commit, got: {stdout:?}"
    );
}

/// The binary's version line IS the library's `version_line()` — one source
/// of truth for clap, logs, and operators (same package, same build-script
/// capture).
#[test]
fn binary_version_line_is_the_library_version_line() {
    assert_eq!(
        run_version("--version"),
        format!("squeezefs {}\n", squeezefs::version::version_line()),
        "clap must be wired to squeezefs::version::version_line() verbatim"
    );
}

// ---------------------------------------------------------------------------
// Formatting-function branches with injected values (the git capture is
// build-time; tests must NOT create tags or dirty the tree).
// ---------------------------------------------------------------------------

const TRAIN: &str = "1.1.0";
const SHORT: &str = "f63455bcb824";
const FULL: &str = "f63455bcb8249b064531d000624c40825a6e763e";
const TS: &str = "2026-07-18T00:00:00Z";

#[test]
fn formatter_untagged_clean_is_train_first_with_the_commit_pair() {
    assert_eq!(
        squeezefs::version::format_version_line(TRAIN, SHORT, FULL, false, "", TS),
        format!("{TRAIN} ({SHORT} / {FULL}) built {TS}"),
        "untagged clean build: `<train> (<short> / <full>) built <ts>`"
    );
}

#[test]
fn formatter_untagged_dirty_suffixes_both_hashes() {
    assert_eq!(
        squeezefs::version::format_version_line(TRAIN, SHORT, FULL, true, "", TS),
        format!("{TRAIN} ({SHORT}-dirty / {FULL}-dirty) built {TS}"),
        "dirty build: -dirty rides both the short and full hash"
    );
}

#[test]
fn formatter_tagged_release_carries_the_tag_with_the_commit() {
    assert_eq!(
        squeezefs::version::format_version_line(TRAIN, SHORT, FULL, false, "stable-2026.07", TS),
        format!("{TRAIN} ({SHORT} / {FULL}, tag stable-2026.07) built {TS}"),
        "tagged release: `<train> (<short> / <full>, tag <tag>) built <ts>` \
         — the tag names the release, the commit identity survives verbatim"
    );
}

#[test]
fn formatter_tagged_dirty_still_carries_dirty_hashes() {
    assert_eq!(
        squeezefs::version::format_version_line(TRAIN, SHORT, FULL, true, "lts-2026.07", TS),
        format!("{TRAIN} ({SHORT}-dirty / {FULL}-dirty, tag lts-2026.07) built {TS}"),
        "a dirty rebuild of a tagged commit must not masquerade as the release"
    );
}

#[test]
fn formatter_output_shapes_match_the_cli_regex() {
    // The formatter and the CLI-level regex must agree on every branch.
    let re = version_line_regex();
    for (dirty, tag) in [
        (false, ""),
        (true, ""),
        (false, "stable-2026.07"),
        (true, "lts-2026.07.1"),
    ] {
        let line = format!(
            "squeezefs {}",
            squeezefs::version::format_version_line(TRAIN, SHORT, FULL, dirty, tag, TS)
        );
        assert!(
            re.is_match(&line),
            "formatter branch (dirty={dirty}, tag={tag:?}) must match the \
             published shape, got: {line:?}"
        );
    }
}

#[test]
fn build_commit_stats_form_suffixes_dirty_on_the_full_hash() {
    assert_eq!(
        squeezefs::version::format_build_commit(FULL, false),
        FULL,
        "clean build: stats build_commit is the bare full hash"
    );
    assert_eq!(
        squeezefs::version::format_build_commit(FULL, true),
        format!("{FULL}-dirty"),
        "dirty build: stats build_commit carries the -dirty suffix"
    );
}

/// The embedded (build-time) identity is internally consistent: the version
/// line leads with the release train and carries exactly the values
/// `build_commit()` / `build_tag()` report.
#[test]
fn embedded_identity_is_internally_consistent() {
    let line = squeezefs::version::version_line();
    let commit = squeezefs::version::build_commit();
    let tag = squeezefs::version::build_tag();
    assert!(
        line.starts_with(&format!("{} (", squeezefs::version::RELEASE_TRAIN)),
        "version_line() must lead with RELEASE_TRAIN (line: {line:?})"
    );
    assert!(
        line.contains(&commit),
        "version_line() must embed build_commit() verbatim \
         (line: {line:?}, commit: {commit:?})"
    );
    if !tag.is_empty() {
        assert!(
            line.contains(&format!(", tag {tag}")),
            "tagged builds carry `, tag <tag>` (line: {line:?}, tag: {tag:?})"
        );
    }
}

// ---------------------------------------------------------------------------
// Stats surface: the fleet mixed-version detector.
// ---------------------------------------------------------------------------

/// The daemon stats JSON must always export `build_commit` (full hash,
/// `-dirty` when applicable) and `build_tag` (empty string when untagged —
/// the always-export stats convention: operators key on field names).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stats_surface_exports_build_commit_and_build_tag() {
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::cache::TieredCache;
    use squeezefs::dlm::DlmClient;
    use squeezefs::fuse_client::SqueezefsFilesystem;
    use squeezefs::nvme_dev::NvmeBlockDev;
    use squeezefs::routing::DataRouter;
    use std::sync::Arc;
    use tempfile::NamedTempFile;

    let dlm = DlmClient::new("local").unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "build_identity_stats_test")
            .await
            .unwrap(),
    );
    let s = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("32MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let json: serde_json::Value =
        serde_json::from_str(&fs.generate_stats_json().await).expect("stats JSON parses");

    let commit = json
        .get("build_commit")
        .and_then(|v| v.as_str())
        .expect("stats must export top-level `build_commit` as a string");
    assert_eq!(
        commit,
        squeezefs::version::build_commit(),
        "stats build_commit must be the embedded build identity"
    );
    assert!(
        Regex::new(r"^[0-9a-f]{40}(-dirty)?$")
            .expect("hash regex compiles")
            .is_match(commit),
        "build_commit from a git-built binary is the full 40-hex hash with \
         an optional -dirty suffix, got: {commit:?}"
    );

    let tag = json
        .get("build_tag")
        .and_then(|v| v.as_str())
        .expect("stats must ALWAYS export `build_tag` (empty when untagged)");
    assert_eq!(
        tag,
        squeezefs::version::build_tag(),
        "stats build_tag must be the embedded release tag (empty when untagged)"
    );
}
