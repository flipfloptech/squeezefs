//! CLI help hygiene tripwire (user directive 2026-07-21): `--help` output
//! is END-USER documentation and must not display internal engineering
//! references — design-doc anchors (`§5.7`, `design-volume-lifecycle`,
//! `docs/design-read-path.md`), key-decision numbers (`KD-11`), PR-ladder
//! names (`PR VL3`, `VL5b`), or program codenames (`D0-guarded`, `the W2
//! fold`, `R5 joint memory budget`, `the L4 PR ladder`, `SMO compactor`,
//! `FIND-*`, `G-VL-*`, `P2-9`, on-disk incompat-bit spellings). That
//! knowledge stays in the tree as `//` comments at the clap definition
//! sites — it just must not be ON DISPLAY in `--help`.
//!
//! Deliberately ALLOWED (operator-facing product vocabulary, not internal
//! anchors): the defrag fragmentation axes D1–D4 (each is a stats-inode
//! gauge `frag_d1_*`..`frag_d4_*` and the defrag help defines them
//! inline) and the fsck check-class labels C1–C7 (findings in the fsck
//! report carry `class: "C7"` — help must be able to name what the report
//! shows). The `D0` single-writer-guard decision id is NOT in that set:
//! the guard is described in behavioral terms instead.
//!
//! The walk is self-maintaining: it parses the `Commands:` section of
//! every `--help` page and recurses, so a newly added subcommand is
//! covered automatically. A node-count floor guards the parser itself
//! against silently walking nothing.

use regex::Regex;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

/// Run `squeezefs <path...> --help` and return combined stdout+stderr.
fn help_for(path: &[String]) -> String {
    let out = Command::new(bin())
        .args(path)
        .arg("--help")
        .output()
        .unwrap_or_else(|e| panic!("spawn `squeezefs {} --help`: {e}", path.join(" ")));
    assert!(
        out.status.success(),
        "`squeezefs {} --help` must exit 0, got {:?}\nstderr: {}",
        path.join(" "),
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Parse the subcommand names out of a clap help page's `Commands:`
/// section. Subcommand rows are indented exactly two spaces; wrapped
/// description continuations are indented deeper and skipped. The
/// auto-generated `help` subcommand is skipped (its page is the parent's).
fn subcommands_of(help: &str) -> Vec<String> {
    let mut subs = Vec::new();
    let mut in_commands = false;
    for line in help.lines() {
        if line.trim_end() == "Commands:" {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        if line.trim().is_empty() || !line.starts_with(' ') {
            in_commands = false;
            continue;
        }
        if let Some(rest) = line.strip_prefix("  ") {
            if !rest.starts_with(' ') {
                if let Some(name) = rest.split_whitespace().next() {
                    if name != "help" {
                        subs.push(name.to_string());
                    }
                }
            }
        }
    }
    subs
}

/// The forbidden internal-reference patterns. Each match is a leak.
fn forbidden_patterns() -> Vec<(Regex, &'static str)> {
    [
        // Design-doc section anchors ("§5.7", "§6.2 grammar", ...).
        (r"§", "design-doc section sign `§`"),
        // Key-decision numbers (KD-3, KD-11/KD-12, KD-17, ...).
        (r"KD-\d", "key-decision anchor `KD-N`"),
        // Design-doc slugs (design-volume-lifecycle, design-read-path, ...).
        (r"design-[a-z]+(-[a-z]+)+", "design-doc slug `design-...`"),
        // Repo doc paths (docs/design-preload-interception.md §5.6.2, ...).
        (r"docs/", "repo doc path `docs/`"),
        // PR-ladder names (PR VL3, PR VL5b, PR VL6a/VL6b, ...).
        (r"PR VL\d", "PR-ladder name `PR VLn`"),
        // Volume-lifecycle program codenames (VL3, VL4 mover, pre-VL5a, ...).
        (r"\bVL\d", "program codename `VLn`"),
        // Finding ids (FIND-M11-A, FIND-L1-A, ...).
        (r"FIND-", "finding id `FIND-*`"),
        // Gate ids (G-VL-*, ...).
        (r"G-VL", "gate id `G-VL*`"),
        // The single-writer-guard decision id (D0-guarded, D0 claims, the
        // guarded D0 open). D1–D4 are the ALLOWED defrag axis gauges, so
        // the pattern is exactly D0.
        (r"\bD0\b", "writer-guard decision id `D0`"),
        // Random-small-write program codename (the W2 fold).
        (r"\bW2\b", "program codename `W2`"),
        // Read-path program codename (R5 joint memory budget).
        (r"\bR5\b", "program codename `R5`"),
        // Interception program codename (the L4 PR ladder, L4 LD_PRELOAD).
        (r"\bL4\b", "program codename `L4`"),
        // KV-internal jargon (the SMO compactor).
        (r"\bSMO\b", "internal jargon `SMO`"),
        // Random-small-write program item ids (RW1..RW6).
        (r"\bRW\d", "program codename `RWn`"),
        // Priority-item anchors (P2-9, P1-8, ...).
        (r"\bP\d-\d", "priority-item anchor `Pn-m`"),
        // On-disk incompat-bit spellings (KV_VOLUME_LIFECYCLE,
        // KV_GUEST_SLOTS) — keep the consequence ("older binaries refuse
        // the set"), drop the spelling (error messages keep it).
        (r"KV_[A-Z]", "incompat-bit spelling `KV_*`"),
    ]
    .into_iter()
    .map(|(pat, label)| (Regex::new(pat).expect("valid pattern"), label))
    .collect()
}

/// Recursively walk the full CLI tree, collecting every forbidden-pattern
/// hit with its command path and offending help line.
fn walk(
    path: &mut Vec<String>,
    patterns: &[(Regex, &'static str)],
    violations: &mut Vec<String>,
    visited: &mut usize,
) {
    *visited += 1;
    let help = help_for(path);
    for line in help.lines() {
        for (re, label) in patterns {
            if re.is_match(line) {
                violations.push(format!(
                    "`squeezefs {} --help` leaks {label}:\n    {}",
                    path.join(" "),
                    line.trim()
                ));
            }
        }
    }
    for sub in subcommands_of(&help) {
        path.push(sub);
        walk(path, patterns, violations, visited);
        path.pop();
    }
}

/// The tripwire: no `--help` page anywhere in the CLI tree may display an
/// internal design anchor, key-decision number, PR-ladder name, or
/// program codename.
#[test]
fn test_help_tree_contains_no_internal_design_anchors() {
    let patterns = forbidden_patterns();
    let mut violations = Vec::new();
    let mut visited = 0usize;
    walk(&mut Vec::new(), &patterns, &mut violations, &mut visited);
    // Parser self-check: the CLI tree has ~85 nodes today; walking far
    // fewer means the `Commands:` parser rotted, not that the CLI shrank.
    assert!(
        visited >= 60,
        "help-tree walk visited only {visited} nodes — the subcommand \
         parser is broken (expected the full CLI tree)"
    );
    assert!(
        violations.is_empty(),
        "internal engineering references on display in --help ({} leaks):\n\n{}",
        violations.len(),
        violations.join("\n")
    );
}

/// The walker must actually see the known top-level verbs — guards the
/// `Commands:` parser against format drift that would silently skip
/// subtrees (and keeps the tripwire honest for new verbs: they appear in
/// the root list and are walked automatically).
#[test]
fn test_help_walker_sees_top_level_verbs() {
    let help = help_for(&[]);
    let subs = subcommands_of(&help);
    for verb in [
        "format", "mount", "umount", "status", "clients", "volume", "job", "fsck", "scrub",
        "defrag", "claim", "bench", "clone", "tune", "config", "df", "storage", "nvmeof",
    ] {
        assert!(
            subs.iter().any(|s| s == verb),
            "root help must list `{verb}` (walker saw: {subs:?})"
        );
    }
}
