//! Operator-manual parity for the env-knob registry (ENG-10; user directive
//! 2026-09-12 — "make sure all documentation is ACCURATE").
//!
//! `src/env_knobs.rs` is the source of truth for every `SQUEEZEFS_*` /
//! `SQZ_*` name the tree reads; `docs/operations.md` §"Environment knobs —
//! the complete registry" is its operator rendering and calls itself
//! complete. Before this file the two drifted silently: on the 2026-09-12
//! audit the table lacked four validated test seams and two harness
//! variables, carried one knob twice with different prose, and spelled two
//! derived defaults differently from the registry.
//!
//! Pinned here (the doc is the only thing these contracts ever fail on —
//! the registry itself is pinned by `tests/env_knob_convention_tests.rs`):
//!
//! 1. Every product knob (`Bool` / `Int` / `Enum` / `Str`) is a table row.
//! 2. Every `Kind::Harness` variable is a table row marked `harness`.
//! 3. Every row names a registered knob (no ghosts), exactly once.
//! 4. No `Kind::Retired` spelling is a row of the registry table; each is
//!    listed with its successor under the parsing convention instead.
//! 5. Every row's **Default** cell is the registry's `default` string
//!    verbatim, and its **Accepts** cell names the registry kind.
//!
//! The test reads two files and registers nothing.

use squeezefs::env_knobs::{Kind, KNOBS};
use std::collections::BTreeMap;
use std::path::Path;

const SECTION_HEADING: &str = "### Environment knobs — the complete registry";
const TABLE_HEADER: &str = "| Knob | Accepts | Default | Purpose |";

fn manual() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/operations.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The registry section: from its heading to the next `### ` heading.
fn registry_section(doc: &str) -> &str {
    let start = doc
        .find(SECTION_HEADING)
        .unwrap_or_else(|| panic!("docs/operations.md lost the heading {SECTION_HEADING:?}"));
    let body = &doc[start + SECTION_HEADING.len()..];
    let end = body.find("\n### ").map_or(body.len(), |i| i + 1);
    &body[..end]
}

/// One table row: the knob name plus its first three cells (`Knob`,
/// `Accepts`, `Default`). The `Purpose` cell may itself contain `|`, so
/// only the leading cells are parsed positionally.
#[derive(Debug)]
struct Row {
    line: usize,
    name: String,
    accepts: String,
    default: String,
}

fn strip_ticks(cell: &str) -> String {
    cell.trim().trim_matches('`').trim().to_string()
}

fn rows(section: &str, section_line_offset: usize) -> Vec<Row> {
    let mut out = Vec::new();
    let mut in_table = false;
    for (i, line) in section.lines().enumerate() {
        let line_no = section_line_offset + i;
        if line.starts_with(TABLE_HEADER) {
            in_table = true;
            continue;
        }
        if line.starts_with('|') && line.contains("---") {
            continue;
        }
        if !line.starts_with('|') {
            in_table = false;
            continue;
        }
        assert!(
            in_table,
            "docs/operations.md:{line_no}: a table row outside a `{TABLE_HEADER}` table — every \
             registry table shares that header so the Default cell is always the third one"
        );
        let cells: Vec<&str> = line.trim_end().trim_end_matches('|').split('|').collect();
        assert!(
            cells.len() >= 5,
            "docs/operations.md:{line_no}: registry row has fewer than four cells: {line:?}"
        );
        let name = strip_ticks(cells[1]);
        assert!(
            name.starts_with("SQUEEZEFS_") || name.starts_with("SQZ_"),
            "docs/operations.md:{line_no}: registry row whose Knob cell is not a knob name: {name:?}"
        );
        out.push(Row {
            line: line_no,
            name,
            accepts: strip_ticks(cells[2]),
            default: strip_ticks(cells[3]),
        });
    }
    out
}

/// The word the manual's **Accepts** column uses for each registry kind.
fn accepts_word(kind: Kind) -> &'static str {
    match kind {
        Kind::Bool => "bool",
        Kind::Int { .. } => "int",
        Kind::Enum(_) => "enum",
        Kind::Str => "string",
        Kind::Retired { .. } => "retired",
        Kind::BuildTime => "build-time",
        Kind::Harness => "harness",
    }
}

fn accepts_matches(kind: Kind, cell: &str) -> bool {
    match kind {
        Kind::Int { .. } => cell.starts_with("int "),
        Kind::Enum(words) => {
            let listed: Vec<&str> = cell.split('/').map(str::trim).collect();
            listed == words
        }
        other => cell == accepts_word(other),
    }
}

struct Parsed {
    rows: Vec<Row>,
    by_name: BTreeMap<String, Vec<usize>>,
}

fn parse() -> (String, Parsed) {
    let doc = manual();
    let section = registry_section(&doc);
    let offset = doc[..doc.find(SECTION_HEADING).expect("heading located above")]
        .lines()
        .count()
        + 1;
    let rows = rows(section, offset);
    let mut by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, r) in rows.iter().enumerate() {
        by_name.entry(r.name.clone()).or_default().push(i);
    }
    (doc, Parsed { rows, by_name })
}

fn is_product(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Bool | Kind::Int { .. } | Kind::Enum(_) | Kind::Str
    )
}

#[test]
fn every_product_knob_is_a_row_of_the_complete_registry_table() {
    let (_, p) = parse();
    let missing: Vec<String> = KNOBS
        .iter()
        .filter(|k| is_product(k.kind) && !p.by_name.contains_key(k.key))
        .map(|k| format!("{} ({:?}, default `{}`)", k.key, k.kind, k.default))
        .collect();
    assert!(
        missing.is_empty(),
        "{} registered product knob(s) have no row in docs/operations.md §complete registry \
         (the heading says complete):\n  {}",
        missing.len(),
        missing.join("\n  ")
    );
}

#[test]
fn every_harness_variable_is_a_row_marked_harness() {
    let (_, p) = parse();
    let mut missing = Vec::new();
    for k in KNOBS.iter().filter(|k| matches!(k.kind, Kind::Harness)) {
        match p.by_name.get(k.key) {
            None => missing.push(format!("{} (no row)", k.key)),
            Some(idx) => {
                for &i in idx {
                    let r = &p.rows[i];
                    if r.accepts != "harness" {
                        missing.push(format!(
                            "{} (line {}: Accepts is {:?}, must be `harness`)",
                            k.key, r.line, r.accepts
                        ));
                    }
                }
            }
        }
    }
    assert!(
        missing.is_empty(),
        "{} Kind::Harness variable(s) are not rows of the harness table in docs/operations.md:\n  {}",
        missing.len(),
        missing.join("\n  ")
    );
}

#[test]
fn every_row_names_a_registered_knob_exactly_once() {
    let (_, p) = parse();
    let mut problems = Vec::new();
    for (name, idx) in &p.by_name {
        if squeezefs::env_knobs::lookup(name).is_none() {
            problems.push(format!(
                "{name} (line {}) is not in the registry — a ghost knob",
                p.rows[idx[0]].line
            ));
        }
        if idx.len() > 1 {
            let lines: Vec<String> = idx.iter().map(|&i| p.rows[i].line.to_string()).collect();
            problems.push(format!(
                "{name} appears in {} rows (lines {}) — one knob, one row",
                idx.len(),
                lines.join(", ")
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "registry-table row problems:\n  {}",
        problems.join("\n  ")
    );
}

#[test]
fn retired_spellings_are_not_rows_and_are_listed_with_their_successor() {
    let (doc, p) = parse();
    let section = registry_section(&doc);
    let convention = &doc[..doc.find(SECTION_HEADING).expect("heading located above")];
    let mut problems = Vec::new();
    for k in KNOBS.iter() {
        let Kind::Retired { successor } = k.kind else {
            continue;
        };
        if let Some(idx) = p.by_name.get(k.key) {
            problems.push(format!(
                "{} is a row of the complete-registry table (line {}) — retired spellings belong \
                 under the parsing convention with their successor, not in the live table",
                k.key, p.rows[idx[0]].line
            ));
        }
        // The retiree table under law 6 of the parsing convention.
        let listed = convention.lines().any(|l| {
            l.starts_with('|') && l.contains(&format!("`{}`", k.key)) && {
                // A real successor knob must be named on the same row; a
                // deletion's successor text is free-form prose.
                if successor.starts_with("SQUEEZEFS_") {
                    l.contains(&format!("`{successor}`"))
                } else {
                    true
                }
            }
        });
        if !listed {
            problems.push(format!(
                "{} is not listed with its successor ({successor}) in the parsing-convention \
                 retiree table above the registry section",
                k.key
            ));
        }
        assert!(
            !section.contains(&format!("| `{}` |", k.key)),
            "{} still has a table row in the registry section",
            k.key
        );
    }
    assert!(
        problems.is_empty(),
        "retired-knob placement:\n  {}",
        problems.join("\n  ")
    );
}

#[test]
fn every_row_default_and_accepts_match_the_registry_verbatim() {
    let (_, p) = parse();
    let mut problems = Vec::new();
    for r in &p.rows {
        let Some(k) = squeezefs::env_knobs::lookup(&r.name) else {
            continue; // the ghost contract reports it
        };
        if r.default != k.default {
            problems.push(format!(
                "{} (line {}): Default cell `{}` != registry default `{}`",
                r.name, r.line, r.default, k.default
            ));
        }
        if !accepts_matches(k.kind, &r.accepts) {
            problems.push(format!(
                "{} (line {}): Accepts cell `{}` does not name the registry kind {:?} ({})",
                r.name,
                r.line,
                r.accepts,
                k.kind,
                accepts_word(k.kind)
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "{} row(s) disagree with the registry (the registry is the source of truth — fix the doc):\n  {}",
        problems.len(),
        problems.join("\n  ")
    );
}
