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
//! Style tripwires (user ruling 2026-07-22): removing the anchors was not
//! enough — the register must be man-page prose (`man mount`), not
//! design-doc prose. Mechanical checks so the register cannot bit-rot:
//!
//! 1. Every entry in every `Commands:` listing is a one-line summary —
//!    the description column is at most 72 characters (essay `about`s
//!    belong in `long_about`, shown on the subcommand's own page).
//! 2. No house editorial voice anywhere in help output: banned phrases
//!    `honestly`, `loudly`, ` loud`, `refuses everything`, `rides `,
//!    `the numbers printed`. State the behavior instead ("fails with an
//!    explanation", "exits nonzero").
//! 3. No prose ALL-CAPS emphasis (METADATA, OFFLINE, WITH, ...). Genuine
//!    acronyms and arg-name references are allowlisted explicitly;
//!    `<PLACEHOLDER>`/`[bracketed]` spans are clap syntax, not prose, and
//!    are stripped before the scan.
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
use std::sync::OnceLock;

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

/// One row of a clap `Commands:` listing: the subcommand name and the
/// rendered description column (everything after the name, trimmed).
struct CommandRow {
    name: String,
    description: String,
}

/// Parse the subcommand rows out of a clap help page's `Commands:`
/// section. Subcommand rows are indented exactly two spaces; wrapped
/// description continuations are indented deeper and skipped. The
/// auto-generated `help` subcommand is skipped (its page is the parent's).
fn command_rows(help: &str) -> Vec<CommandRow> {
    let mut rows = Vec::new();
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
                        let description = rest[name.len()..].trim_start().trim_end().to_string();
                        rows.push(CommandRow {
                            name: name.to_string(),
                            description,
                        });
                    }
                }
            }
        }
    }
    rows
}

/// One walked node of the CLI tree: the command path (root = empty) and
/// its full `--help` page.
struct HelpPage {
    path: String,
    help: String,
}

/// Walk the full CLI tree once and cache every `--help` page — all the
/// tripwires below scan the same collection.
fn help_pages() -> &'static [HelpPage] {
    static PAGES: OnceLock<Vec<HelpPage>> = OnceLock::new();
    PAGES.get_or_init(|| {
        fn walk(path: &mut Vec<String>, pages: &mut Vec<HelpPage>) {
            let help = help_for(path);
            let rows = command_rows(&help);
            pages.push(HelpPage {
                path: path.join(" "),
                help,
            });
            for row in rows {
                path.push(row.name);
                walk(path, pages);
                path.pop();
            }
        }
        let mut pages = Vec::new();
        walk(&mut Vec::new(), &mut pages);
        // Parser self-check: the CLI tree has ~85 nodes today; walking far
        // fewer means the `Commands:` parser rotted, not that the CLI shrank.
        assert!(
            pages.len() >= 60,
            "help-tree walk visited only {} nodes — the subcommand \
             parser is broken (expected the full CLI tree)",
            pages.len()
        );
        pages
    })
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

/// The tripwire: no `--help` page anywhere in the CLI tree may display an
/// internal design anchor, key-decision number, PR-ladder name, or
/// program codename.
#[test]
fn test_help_tree_contains_no_internal_design_anchors() {
    let patterns = forbidden_patterns();
    let mut violations = Vec::new();
    for page in help_pages() {
        for line in page.help.lines() {
            for (re, label) in &patterns {
                if re.is_match(line) {
                    violations.push(format!(
                        "`squeezefs {} --help` leaks {label}:\n    {}",
                        page.path,
                        line.trim()
                    ));
                }
            }
        }
    }
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
    let root = &help_pages()[0];
    assert_eq!(root.path, "", "the first walked page is the root");
    let subs: Vec<String> = command_rows(&root.help)
        .into_iter()
        .map(|r| r.name)
        .collect();
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

// ===========================================================================
// Man-page register (user ruling 2026-07-22)
// ===========================================================================

/// Style tripwire 1: every `Commands:` entry is a one-line summary.
///
/// The build does not enable clap's `wrap_help`, so each row renders as
/// one physical line whatever its length — "fits on one rendered line"
/// is therefore enforced as the equivalent contract: the description
/// column (the subcommand's short `about`) is at most 72 characters.
/// Essays belong in `long_about`, shown on the subcommand's own page.
#[test]
fn test_commands_listings_are_one_line_summaries() {
    const MAX_ABOUT_CHARS: usize = 72;
    let mut violations = Vec::new();
    for page in help_pages() {
        for row in command_rows(&page.help) {
            let len = row.description.chars().count();
            if len > MAX_ABOUT_CHARS {
                violations.push(format!(
                    "`squeezefs {} --help` lists `{}` with a {len}-char description \
                     (max {MAX_ABOUT_CHARS}):\n    {}",
                    page.path, row.name, row.description
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "Commands-list entries must be one-line summaries ({} essays):\n\n{}",
        violations.len(),
        violations.join("\n")
    );
}

/// Style tripwire 2: no house editorial voice in any help output.
/// Case-insensitive; `loud`/`rides` are word-bounded so "refuse loud"
/// and "rides `squeezefs df`" are caught while ordinary words such as
/// "cloud" or "overrides" are not.
#[test]
fn test_help_tree_contains_no_editorial_voice() {
    let banned: Vec<(Regex, &'static str)> = [
        ("honestly", "honestly"),
        ("loudly", "loudly"),
        (r"\bloud\b", "loud"),
        ("refuses everything", "refuses everything"),
        (r"\brides\b", "rides"),
        ("the numbers printed", "the numbers printed"),
    ]
    .into_iter()
    .map(|(pat, label)| {
        (
            Regex::new(&format!("(?i){pat}")).expect("valid pattern"),
            label,
        )
    })
    .collect();
    let mut violations = Vec::new();
    for page in help_pages() {
        for line in page.help.lines() {
            for (re, label) in &banned {
                if re.is_match(line) {
                    violations.push(format!(
                        "`squeezefs {} --help` uses banned phrase `{label}`:\n    {}",
                        page.path,
                        line.trim()
                    ));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "editorial voice on display in --help ({} hits):\n\n{}",
        violations.len(),
        violations.join("\n")
    );
}

/// Acronyms and arg-name references that may legitimately render as
/// all-caps words in help prose. Everything else that is an all-caps
/// word of 3+ letters is prose emphasis and forbidden. Env-var names
/// (`SQUEEZEFS_*`, `SUDO_*`) are allowed by prefix below.
const CAPS_ALLOWLIST: &[&str] = &[
    "ACL",
    "AEAD",
    "CLI",
    "CPU",
    "DPDK",
    "FUSE",
    "GID",
    "JSON",
    "LD_PRELOAD",
    "LTP",
    "LVM",
    "NQN",
    "NVME",
    "O_DIRECT",
    "O_SYNC",
    "PEM",
    "PID",
    "POSIX",
    "RAM",
    "RPC",
    "RSA",
    "SIGKILL",
    "SIGTERM",
    "SPDK",
    "SQPOLL",
    "TARGET",
    "TCP",
    "TLS",
    "TTL",
    "UID",
    "URI",
    "UUID",
];

/// Strip clap syntax spans that are not prose: `<VALUE_NAME>` placeholders
/// and `[bracketed]` annotations (`[OPTIONS]`, `[default: ...]`,
/// `[env: ...]`, `[possible values: ...]`).
fn strip_non_prose(line: &str) -> String {
    static ANGLE: OnceLock<Regex> = OnceLock::new();
    static SQUARE: OnceLock<Regex> = OnceLock::new();
    let angle = ANGLE.get_or_init(|| Regex::new(r"<[^<>]*>").expect("valid pattern"));
    let square = SQUARE.get_or_init(|| Regex::new(r"\[[^\[\]]*\]").expect("valid pattern"));
    square
        .replace_all(&angle.replace_all(line, " "), " ")
        .into_owned()
}

/// Style tripwire 3: no ALL-CAPS emphasis words in help prose
/// (METADATA, OFFLINE, WITH, COHERENT, ...). A word is a violation when
/// it has 3+ letters, every letter is uppercase, and it is neither in
/// the explicit allowlist nor an env-var name.
#[test]
fn test_help_tree_contains_no_all_caps_emphasis() {
    static WORD: OnceLock<Regex> = OnceLock::new();
    let word = WORD.get_or_init(|| Regex::new(r"[A-Za-z0-9_]+").expect("valid pattern"));
    let mut violations = Vec::new();
    for page in help_pages() {
        for line in page.help.lines() {
            let prose = strip_non_prose(line);
            for m in word.find_iter(&prose) {
                let token = m.as_str();
                let letters = token.chars().filter(|c| c.is_ascii_alphabetic()).count();
                let all_upper = token
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
                if letters >= 3
                    && all_upper
                    && !CAPS_ALLOWLIST.contains(&token)
                    && !token.starts_with("SQUEEZEFS_")
                    && !token.starts_with("SUDO_")
                {
                    violations.push(format!(
                        "`squeezefs {} --help` shouts `{token}`:\n    {}",
                        page.path,
                        line.trim()
                    ));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "ALL-CAPS emphasis on display in --help ({} shouts):\n\n{}",
        violations.len(),
        violations.join("\n")
    );
}
