//! The rip-tokio-total convention rail (user ruling 2026-08-13: the
//! tokio scheduler-bug remedy was REMOVAL, not a fork — the shipped
//! binary links no tokio). This test is what keeps it removed: any
//! `tokio::` usage in PRODUCT code (everything outside `#[cfg(test)]`)
//! across the root crate, the fuse3 fork, and squeezefs-ipc fails the
//! gate naming the file and line.
//!
//! Tokio remains a DEV-dependency only: `#[tokio::test]` harnesses and
//! test-module internals are exempt (they never ship), as is the
//! opt-in `rig`-feature measurement binary (never in the product
//! build; its A/B arms deliberately include a tokio-handoff lever).

use std::path::{Path, PathBuf};

const ROOTS: &[&str] = &["src", "crates/fuse3/src", "crates/squeezefs-ipc/src"];

/// Non-product exemptions (never linked into the shipped binary).
const EXEMPT: &[&str] = &[
    // The rig-feature two-process measurement binary (required-features
    // = ["rig"], never a default build; carries a deliberate tokio
    // handoff comparison arm).
    "crates/squeezefs-ipc/src/bin/ipc_hop_rig.rs",
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("readable source dir") {
        let entry = entry.expect("dir entry");
        let p = entry.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

#[test]
fn no_tokio_in_product_code() {
    let root = repo_root();
    let mut files = Vec::new();
    for r in ROOTS {
        walk(&root.join(r), &mut files);
    }
    assert!(files.len() > 100, "walk sanity: found {}", files.len());

    let mut violations: Vec<String> = Vec::new();
    for f in files {
        let rel = f
            .strip_prefix(&root)
            .expect("under root")
            .to_string_lossy()
            .replace('\\', "/");
        if EXEMPT.iter().any(|e| rel == *e) {
            continue;
        }
        let text = std::fs::read_to_string(&f).expect("readable source file");
        // Product half = everything before the first test-module gate
        // (`#[cfg(test)]` or the composed `#[cfg(all(test, ...))]`).
        let cut = [text.find("#[cfg(test)]"), text.find("#[cfg(all(test")]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(text.len());
        for (i, line) in text[..cut].lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with("//") {
                continue; // comments may cite tokio history
            }
            if t.contains("tokio::") {
                violations.push(format!("{rel}:{}: {}", i + 1, t));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "tokio usage in PRODUCT code (the rip-tokio-total rail; move it \
         to a first-party sqz primitive or, if genuinely test-only, \
         under #[cfg(test)]):\n{}",
        violations.join("\n")
    );
}

#[test]
fn tokio_is_not_a_product_dependency() {
    let root = repo_root();
    for (manifest, crate_name) in [("Cargo.toml", "root"), ("crates/fuse3/Cargo.toml", "fuse3")] {
        let text = std::fs::read_to_string(root.join(manifest)).expect("manifest");
        // Everything before [dev-dependencies] must not declare tokio.
        let cut = text.find("[dev-dependencies").unwrap_or(text.len());
        let head = &text[..cut];
        for line in head.lines() {
            let t = line.trim_start();
            if t.starts_with('#') {
                continue;
            }
            assert!(
                !(t.starts_with("tokio ")
                    || t.starts_with("tokio=")
                    || t == "[dependencies.tokio]"
                    || t.starts_with("tokio-rustls")),
                "{crate_name} manifest declares tokio outside dev-dependencies: {t}"
            );
        }
    }
    // squeezefs-ipc: tokio only behind the opt-in rig feature.
    let ipc = std::fs::read_to_string(root.join("crates/squeezefs-ipc/Cargo.toml")).expect("ipc");
    assert!(
        ipc.contains("optional = true") || !ipc.contains("tokio"),
        "squeezefs-ipc must keep tokio optional (rig-only)"
    );
}
