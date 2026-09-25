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
//! 1 — the joiner's cadence dead behind a stats read). The pins are the
//! stress contracts (`sym_appender_tests::the_stats_reader_never_deadlocks_
//! against_a_grants_page_update`, `sym_n_daemon_tests::a_joiners_stats_
//! reader_never_deadlocks_against_its_own_checkpoint_page_writer`); this
//! rail is the STATIC belt over the source (the `kv_loader_lock_style_tests`
//! shape): no `page` guard's scope may acquire `grant`, in any file that
//! touches both. A fourth site that takes `page` then `grant` is RED here
//! before any schedule finds it.

use std::path::Path;

/// The files that acquire an appender region's `page` and `grant` mutexes.
const FILES: &[&str] = &[
    "src/meta_backend/kv/appender.rs",
    "src/meta_backend/kv/backend.rs",
    "src/meta_backend/kv/backend/joined.rs",
    "src/meta_backend/kv/backend/recovery.rs",
    "src/meta_backend/kv/backend/crossvol_arms.rs",
    "src/meta_backend/kv/checkpoint.rs",
    "src/meta_backend/kv/tree.rs",
    "src/meta_ship/manager.rs",
];

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

/// Every `page` guard whose enclosing block (the guard's lifetime — a `let`
/// binding lives to the end of its block) acquires `grant`.
fn page_then_grant_sites(path: &Path) -> Vec<String> {
    let src = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let lines: Vec<&str> = src.lines().collect();
    let mut violations = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let code = code_part(line);
        if !code.contains(PAGE_LOCK) {
            continue;
        }
        // A guard bound by `let` lives to its block's end; a `.page.lock()`
        // used as a temporary (`r.page.lock().unwrap().field`) dies at the
        // statement's end and orders nothing.
        let bound = code.trim_start().starts_with("let ");
        if !bound {
            continue;
        }
        // Scan the guard's block: from the binding's line, the depth must
        // never drop below the binding's own depth. The binding line's own
        // braces are counted as the scan proceeds (a `let x = ...;` line
        // is depth-neutral).
        let mut depth = 0i64;
        let mut j = i;
        loop {
            let c = code_part(lines[j]);
            if j > i {
                for needle in GRANT_LOCKS {
                    if c.contains(needle) {
                        violations.push(format!(
                            "{}:{} takes `page` (line {}) and then `grant` inside that guard's \
                             block: {}",
                            path.display(),
                            j + 1,
                            i + 1,
                            c.trim()
                        ));
                    }
                }
            }
            depth += brace_delta(c);
            if depth < 0 {
                break; // the guard's block closed
            }
            j += 1;
            if j >= lines.len() {
                break;
            }
        }
    }
    violations
}

#[test]
fn every_site_holding_a_regions_page_and_grant_takes_grant_first() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut violations = Vec::new();
    let mut sites_seen = 0usize;
    for f in FILES {
        let path = repo.join(f);
        if !path.exists() {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        sites_seen += src
            .lines()
            .filter(|l| code_part(l).contains(PAGE_LOCK))
            .count();
        violations.extend(page_then_grant_sites(&path));
    }
    assert!(
        sites_seen > 0,
        "the rail read no `.page.lock()` site — the accessor spelling moved; update PAGE_LOCK"
    );
    assert!(
        violations.is_empty(),
        "an appender region's `page` guard acquires `grant` inside its block — the reverse of \
         the region's lock order (grant BEFORE page; F-C4's deadlock class):\n{}",
        violations.join("\n")
    );
}

/// The rail's own teeth: the shape that deadlocked twice is flagged.
#[test]
fn the_rail_flags_a_page_guard_that_acquires_grant_inside_its_block() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.rs");
    std::fs::write(
        &bad,
        "fn f(r: &R) {\n    {\n        let mut page = r.page.lock().unwrap();\n        // a comment naming r.grant() orders nothing\n        let g = r.grant();\n        page.grant = g.page_runs();\n    }\n}\n",
    )
    .unwrap();
    assert_eq!(
        page_then_grant_sites(&bad).len(),
        1,
        "page then grant inside one block"
    );
    let good = dir.path().join("good.rs");
    std::fs::write(
        &good,
        "fn f(r: &R) {\n    let runs = { let g = r.grant(); g.page_runs() };\n    {\n        let mut page = r.page.lock().unwrap();\n        page.grant = runs;\n    }\n    let g2 = r.grant();\n}\n",
    )
    .unwrap();
    assert!(
        page_then_grant_sites(&good).is_empty(),
        "grant before page, or grant after the page block, is the law"
    );
}
