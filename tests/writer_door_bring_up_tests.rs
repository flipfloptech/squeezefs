//! **Every direct WRITER door of the KV backend walks `writer_bring_up`
//! before its first write** (PR 14 fix round 1, Issue 1 — the record's
//! §4.4bf; the class §4.4ax closed at the mount's door).
//!
//! `KvMetaBackend::open_inner(path, OpenPosture::Writer, ..)` is the
//! bootstrap replay alone: the superblock, the ledger, the bitmap, the RAM
//! journal replay — a dead writer's uncovered window dirty in RAM, no frame
//! fence installed. Whoever opens a volume that way and then WRITES (a
//! control entry, `checkpoint_now`) flushes that window under the
//! structural `(0, 0)` frame stamp, below the leaves' leased generations,
//! and the next writer's rule-3 screen ends every such leaf's log there —
//! the dead writer's last acked records gone (§5.8.2). The mount's door
//! (`open_writer`) walks `writer_bring_up` (prime the stamps → restore the
//! own pools → cover → JOIN) between the gate and its tasks; `claim clear`
//! and the harness's claim planter walk it; `appender clear` did not, and
//! the operator's remedy lost the crashed manager's acked creates
//! (`sym_crash_matrix_tests::appender_clear_over_a_crashed_managers_window_
//! stamps_the_leases_generation` is the runtime pin).
//!
//! **Scope (stated):** a TEXTUAL walk of `src/**/*.rs` — every function
//! containing an `open_inner(` call whose posture argument is
//! `OpenPosture::Writer` must also contain a `.writer_bring_up(` call. The
//! rail follows no calls: a helper that opens and returns the backend for
//! a caller to bring up would be flagged (over-approximation — flags more,
//! never less); none exists today. Every OTHER offline writable verb
//! (`enable-symmetric`, `enable-multi-writer`, fsck's repair, defrag,
//! `set-cache-paths`, the volume verbs) opens through `KvMetaBackend::open`
//! / `open_for_sym_upgrade` — the whole D0 ladder — or through
//! `open_probe`, which writes nothing; those are not this rail's sites.

use std::path::{Path, PathBuf};

const OPEN_INNER: &str = "open_inner(";
const WRITER_POSTURE: &str = "OpenPosture::Writer";
const BRING_UP: &str = ".writer_bring_up(";

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

/// The `fn` items of `src` as `(name, body lines)`: a `fn` line opens a
/// body at its first `{` and the body runs to the matching `}`.
fn fn_bodies(src: &str) -> Vec<(String, Vec<String>)> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let code = code_part(lines[i]);
        let t = code.trim_start();
        let is_fn = t.starts_with("fn ")
            || t.starts_with("pub fn ")
            || t.starts_with("async fn ")
            || t.starts_with("pub async fn ")
            || t.starts_with("pub(super) fn ")
            || t.starts_with("pub(super) async fn ")
            || t.starts_with("pub(crate) fn ")
            || t.starts_with("pub(crate) async fn ")
            || t.starts_with("pub(in ");
        if !is_fn {
            i += 1;
            continue;
        }
        let name = t
            .split("fn ")
            .nth(1)
            .and_then(|rest| rest.split(['(', '<']).next())
            .unwrap_or("?")
            .to_string();
        // Walk to the body's opening brace (the signature may wrap), then
        // to its close.
        let mut depth = 0i64;
        let mut opened = false;
        let mut body = Vec::new();
        let mut j = i;
        while j < lines.len() {
            let c = code_part(lines[j]);
            body.push(lines[j].to_string());
            depth += brace_delta(c);
            if depth > 0 {
                opened = true;
            }
            if opened && depth <= 0 {
                break;
            }
            // A signature that ends in `;` is a trait declaration — no body.
            if !opened && c.trim_end().ends_with(';') {
                break;
            }
            j += 1;
        }
        out.push((name, body));
        i = j + 1;
    }
    out
}

/// The functions of `path` that open a WRITER door and never bring it up.
fn writer_doors_without_bring_up(path: &Path) -> Vec<String> {
    let src = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut violations = Vec::new();
    for (name, body) in fn_bodies(&src) {
        let code: Vec<&str> = body.iter().map(|l| code_part(l)).collect();
        // The posture argument may sit on the line after `open_inner(`:
        // judge the call over a three-line window.
        let opens_writer = code.iter().enumerate().any(|(k, l)| {
            l.contains(OPEN_INNER)
                && code[k..(k + 3).min(code.len())]
                    .iter()
                    .any(|w| w.contains(WRITER_POSTURE))
        });
        if !opens_writer {
            continue;
        }
        // `open_inner` itself and the posture match inside it are not doors.
        if name == "open_inner" {
            continue;
        }
        if !code.iter().any(|l| l.contains(BRING_UP)) {
            violations.push(format!(
                "{}: fn {name} opens `open_inner(.., OpenPosture::Writer, ..)` and never calls \
                 `writer_bring_up` — a flush from it stamps the replayed window `(0, 0)`",
                path.display()
            ));
        }
    }
    violations
}

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
fn every_direct_writer_door_walks_writer_bring_up() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&repo.join("src"), &mut files);
    let mut doors = 0usize;
    let mut violations = Vec::new();
    for path in &files {
        let src = std::fs::read_to_string(path).unwrap();
        let n = src
            .lines()
            .filter(|l| {
                let c = code_part(l);
                c.contains(WRITER_POSTURE) && !c.contains("matches!") && !c.contains("==")
            })
            .count();
        if n > 0 {
            doors += n;
            violations.extend(writer_doors_without_bring_up(path));
        }
    }
    assert!(
        doors >= 4,
        "the rail read {doors} `OpenPosture::Writer` door(s) — the mount's, `claim clear`'s, \
         `appender clear`'s two and the harness planter's are the census; the spelling moved"
    );
    assert!(
        violations.is_empty(),
        "a direct WRITER door writes without the writer's bring-up (the §4.4ax / §4.4bf \
         class):\n{}",
        violations.join("\n")
    );
}

/// The rail's own teeth: the verb's pre-fix shape is flagged, the ladder's
/// shape and a probe door are not.
#[test]
fn the_rail_flags_a_writer_door_that_flushes_without_the_bring_up() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, body: &str| {
        let p = dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        p
    };
    let bad = write(
        "bad.rs",
        "impl K {\n    pub async fn verb(path: &Path) -> R {\n        let mut inner = Self::open_inner(path, OpenPosture::Writer, None, false).await?;\n        let be = Arc::new(inner);\n        be.checkpoint_now().await?;\n        Ok(())\n    }\n}\n",
    );
    assert_eq!(
        writer_doors_without_bring_up(&bad).len(),
        1,
        "the pre-fix verb's shape"
    );
    let bad_wrapped = write(
        "bad_wrapped.rs",
        "impl K {\n    async fn verb(path: &Path) -> R {\n        let inner =\n            Self::open_inner(path, OpenPosture::Writer, None, true).await?;\n        Ok(())\n    }\n}\n",
    );
    assert_eq!(
        writer_doors_without_bring_up(&bad_wrapped).len(),
        1,
        "the posture on the call's continuation line"
    );
    let good = write(
        "good.rs",
        "impl K {\n    async fn verb(path: &Path) -> R {\n        let inner = Self::open_inner(path, OpenPosture::Writer, None, false).await?;\n        let be = Arc::new(inner);\n        if let Err(e) = be.writer_bring_up(started).await {\n            return Err(e);\n        }\n        be.checkpoint_now().await?;\n        Ok(())\n    }\n    async fn probe(path: &Path) -> R {\n        let inner = Self::open_inner(path, OpenPosture::NonWriter, None, false).await?;\n        Ok(())\n    }\n    async fn open_inner(path: &Path, posture: OpenPosture) -> R {\n        if matches!(posture, OpenPosture::Writer) {}\n        Ok(())\n    }\n}\n",
    );
    assert!(
        writer_doors_without_bring_up(&good).is_empty(),
        "the ladder, a probe door and `open_inner` itself are the law: {:?}",
        writer_doors_without_bring_up(&good)
    );
}
