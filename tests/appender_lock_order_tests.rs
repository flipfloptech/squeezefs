//! **One lock order over an appender region's two mutexes: `grant` before
//! `page`** (PR 13i F-C4 and its fix round 1, Issue 1).
//!
//! `AppenderRegion` carries a `page` mutex (the RAM copy of its appender
//! page) and a `grant` mutex (its extent grant). The grant writers name the
//! grant's remainder ON the page under the grant guard — the manager's
//! `ExtentGrant` (`manager_extent_grant_class`), the joiner's
//! `name_remainder_on_page`, the shrink — so they take `grant` then
//! `page`. Two readers took them the other way round and each deadlocked a
//! mount for ever: `AppenderSet::stats` (the `.stats` poll every fleet leg
//! runs at 1 Hz) against the manager's reactive refill (F-C4), and the
//! checkpoint task's own page writer `write_appender_pages` against the
//! joiner's `.stats` poll / its commit path's refill (fix round 1, Issue
//! 1 — the joiner's cadence dead behind a stats read).
//!
//! **Scope of this rail (review round 2, Issue 11 — stated, not implied):**
//! a TEXTUAL walk of every `src/**/*.rs` file (no fixed list — a new file
//! taking `.page.lock()` is in the census the day it lands). A site is a
//! line acquiring a `page` guard through one of the BINDING spellings —
//! `let` / `let mut` (the guard lives to its enclosing block's end),
//! `if let` / `while let` / `match` (the guard lives for the construct's
//! own block) — whose lifetime block contains a `grant` acquisition
//! (`.grant()` or `.grant.lock()`). A `.page.lock()` used as a temporary
//! outside a binding (`r.page.lock().unwrap().term;` as an expression
//! statement) dies at its statement and orders nothing; a `let`-bound
//! temporary (`let t = r.page.lock().unwrap().term;`) is COUNTED as a guard
//! — the rail over-approximates there (flags more, never less). What the
//! rail does NOT reach: it follows no calls — a `page` block that calls a
//! helper which takes `grant` INSIDE is invisible here, as is a guard
//! passed to a helper. The runtime pins are the belt for that class:
//! `sym_appender_tests::the_stats_reader_never_deadlocks_against_a_grants_
//! page_update` and `sym_n_daemon_tests::a_joiners_stats_reader_never_
//! deadlocks_against_its_own_checkpoint_page_writer` (both RED on the
//! unfixed product at their bounds). A fourth textual site that takes
//! `page` then `grant` is RED here before any schedule finds it.

use std::path::{Path, PathBuf};

/// A `page` guard acquisition: `<recv>.page.lock()`.
const PAGE_LOCK: &str = ".page.lock()";
/// The `grant` guard acquisitions: the accessor and the raw mutex.
const GRANT_LOCKS: &[&str] = &[".grant()", ".grant.lock()"];

/// Strip `//` comments (never inside a string in this tree's style) so a
/// doc sentence naming both locks is not a site.
fn code_part(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

fn brace_delta(code: &str) -> i64 {
    let mut d = 0i64;
    let mut in_str = false;
    let mut prev = '\0';
    for c in code.chars() {
        match c {
            '"' if prev != '\\' => in_str = !in_str,
            '{' if !in_str => d += 1,
            '}' if !in_str => d -= 1,
            _ => {}
        }
        prev = c;
    }
    d
}

/// How a binding spelling scopes its guard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GuardScope {
    /// `let` / `let mut`: to the end of the ENCLOSING block.
    EnclosingBlock,
    /// `if let` / `while let` / `match`: the construct's OWN block.
    OwnBlock,
}

/// The binding spelling of a line acquiring a `page` guard, if any.
fn guard_scope(code: &str) -> Option<GuardScope> {
    let t = code.trim_start();
    if t.starts_with("if let ") || t.starts_with("while let ") || t.starts_with("match ") {
        return Some(GuardScope::OwnBlock);
    }
    if t.starts_with("let ") {
        return Some(GuardScope::EnclosingBlock);
    }
    // `} else if let Some(p) = r.page.lock()… {` and `Some(x) => match …`
    // arms: the construct keyword mid-line.
    if t.contains("if let ") || t.contains("while let ") {
        return Some(GuardScope::OwnBlock);
    }
    None
}

/// Every `page` guard whose lifetime block acquires `grant`.
fn page_then_grant_sites(path: &Path) -> Vec<String> {
    let src = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let lines: Vec<&str> = src.lines().collect();
    let mut violations = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let code = code_part(line);
        if !code.contains(PAGE_LOCK) {
            continue;
        }
        // The binding keyword may stand up to two lines above a wrapped
        // acquisition (`let mut page =\n    r.page.lock()…`): walk back
        // through the statement's continuation lines (none ends a
        // statement or a block).
        let mut scope = None;
        for back in 0..=2usize {
            let Some(k) = i.checked_sub(back) else {
                break;
            };
            let c = code_part(lines[k]);
            if back > 0 {
                let t = c.trim_end();
                if t.ends_with(';') || t.ends_with('{') || t.ends_with('}') {
                    break; // an earlier statement or block ended here
                }
            }
            if let Some(s) = guard_scope(c) {
                scope = Some(s);
                break;
            }
        }
        let Some(scope) = scope else {
            continue;
        };
        // Scan the guard's lifetime: for an enclosing-block guard until the
        // depth drops below the binding line's; for an own-block construct
        // until the block its line opened closes again. The binding line's
        // own braces are counted as the scan proceeds.
        let mut depth = 0i64;
        let mut opened = false;
        let mut j = i;
        loop {
            let c = code_part(lines[j]);
            if j > i {
                for needle in GRANT_LOCKS {
                    if c.contains(needle) {
                        violations.push(format!(
                            "{}:{} takes `page` (line {}, {:?}) and then `grant` inside that \
                             guard's lifetime: {}",
                            path.display(),
                            j + 1,
                            i + 1,
                            scope,
                            c.trim()
                        ));
                    }
                }
            }
            depth += brace_delta(c);
            if depth > 0 {
                opened = true;
            }
            match scope {
                GuardScope::EnclosingBlock if depth < 0 => break,
                GuardScope::OwnBlock if depth <= 0 && (opened || j > i) => break,
                _ => {}
            }
            j += 1;
            if j >= lines.len() {
                break;
            }
        }
    }
    violations
}

/// Every `.rs` file under `root`, recursively.
fn rust_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn every_site_holding_a_regions_page_and_grant_takes_grant_first() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&repo.join("src"), &mut files);
    assert!(
        files.len() > 50,
        "the census walked src/**/*.rs: {}",
        files.len()
    );
    let mut violations = Vec::new();
    let mut sites_seen = 0usize;
    let mut files_with_sites = Vec::new();
    for path in &files {
        let src = std::fs::read_to_string(path).unwrap();
        let n = src
            .lines()
            .filter(|l| code_part(l).contains(PAGE_LOCK))
            .count();
        if n > 0 {
            sites_seen += n;
            files_with_sites.push(path.strip_prefix(repo).unwrap().display().to_string());
            violations.extend(page_then_grant_sites(path));
        }
    }
    assert!(
        sites_seen > 0,
        "the rail read no `.page.lock()` site — the accessor spelling moved; update PAGE_LOCK"
    );
    eprintln!("appender lock-order rail: {sites_seen} page sites in {files_with_sites:?}");
    assert!(
        violations.is_empty(),
        "an appender region's `page` guard acquires `grant` inside its lifetime — the reverse \
         of the region's lock order (grant BEFORE page; F-C4's deadlock class):\n{}",
        violations.join("\n")
    );
}

/// The rail's own teeth: the shape that deadlocked twice is flagged under
/// every binding spelling; the lawful orders are not.
#[test]
fn the_rail_flags_a_page_guard_that_acquires_grant_inside_its_block() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, body: &str| {
        let p = dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        p
    };
    let bad_let = write(
        "bad_let.rs",
        "fn f(r: &R) {\n    {\n        let mut page = r.page.lock().unwrap();\n        // a comment naming r.grant() orders nothing\n        let g = r.grant();\n        page.grant = g.page_runs();\n    }\n}\n",
    );
    assert_eq!(
        page_then_grant_sites(&bad_let).len(),
        1,
        "let-bound page then grant inside one block"
    );
    let bad_if_let = write(
        "bad_if_let.rs",
        "fn f(r: &R) {\n    if let Ok(mut page) = r.page.lock() {\n        let g = r.grant();\n        page.grant = g.page_runs();\n    }\n}\n",
    );
    assert_eq!(
        page_then_grant_sites(&bad_if_let).len(),
        1,
        "if-let-bound page then grant inside the if block"
    );
    let bad_match = write(
        "bad_match.rs",
        "fn f(r: &R) {\n    match r.page.lock() {\n        Ok(mut page) => {\n            let g = r.grant.lock().unwrap();\n            page.grant = g.page_runs();\n        }\n        Err(_) => {}\n    }\n}\n",
    );
    assert_eq!(
        page_then_grant_sites(&bad_match).len(),
        1,
        "match-scrutinee page then grant inside the match"
    );
    let good = write(
        "good.rs",
        "fn f(r: &R) {\n    let runs = { let g = r.grant(); g.page_runs() };\n    {\n        let mut page = r.page.lock().unwrap();\n        page.grant = runs;\n    }\n    let g2 = r.grant();\n    if let Ok(page) = r.page.lock() {\n        let _ = page.term;\n    }\n    let g3 = r.grant();\n    r.page.lock().unwrap().term;\n    let g4 = r.grant();\n}\n",
    );
    assert!(
        page_then_grant_sites(&good).is_empty(),
        "grant before page, grant after a closed page block, and a temporary page access \
         are the law: {:?}",
        page_then_grant_sites(&good)
    );
}
