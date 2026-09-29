//! The allocation act's LOCK-LAW census (record §4.4cd, review round 1
//! Issue 1) — a static rail over every production call of the two
//! placed-allocation forms:
//!
//! * `BackendRouter::allocate_placed_block()` — the HOOKED act: a
//!   `StorageFull` verdict runs the pressure close, whose per-epoch SAVE
//!   takes the epoch ino's 4a DLM guard PARKING and whose frees run at
//!   the RES-1 posture. A caller may hold `active_inode_locks` (1) and
//!   `BLOCK_FLUSH_LOCKS` (3) — and must hold NO level-3.5 stripe and NO 4a
//!   guard (the self-deadlock class `fe62cb0e`'s flip fix adjudicated:
//!   a stripe collision, or a two-task cycle against a canonical
//!   multi-ino `lock_many`).
//! * `BackendRouter::allocate_placed_block_under_guard()` — the hook-less
//!   form for a caller that HOLDS either class.
//!
//! The first build of the pressure close missed that the owner-side
//! compose arms (`layout_merge_pass`, `merge_layout_and_size_direct`,
//! `spill_oversize_chained_full`) reach `multi_writer::indirect_map_io_for`'s
//! blob mint holding exclusive 4a `I{}` guards.
//!
//! **What the rail judges**: the census is by NAME — `(file, enclosing fn
//! item, call count)` over `src/`, comment lines skipped — never by lock
//! context; the lock class beside each entry below is the reviewer's
//! claim about that fn, recorded so a drift shows up as a diff against a
//! stated law. A new call (a 13th site, or a SECOND call inside a listed
//! fn — which is why the count is part of the key) joins its list
//! EXPLICITLY with its lock class stated, never by drift.

use std::collections::BTreeMap;
use std::path::Path;

/// The Rust item grammar the rail recognises as an enclosing fn:
/// `[pub[(…)]] [const|async|unsafe|extern "…"]* fn NAME`. A closure inside
/// an item attributes to the item (the compose arms' blob mint is one).
fn fn_item_name(line: &str) -> Option<&str> {
    let mut t = line.trim_start();
    if let Some(rest) = t.strip_prefix("pub") {
        t = rest;
        if let Some(rest) = t.strip_prefix('(') {
            t = rest.split_once(')')?.1;
        }
        t = t.strip_prefix(' ')?;
    }
    loop {
        t = t.trim_start();
        if let Some(rest) = t.strip_prefix("const ") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix("async ") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix("unsafe ") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix("extern ") {
            t = rest.trim_start();
            if let Some(rest) = t.strip_prefix('"') {
                t = rest.split_once('"')?.1;
            }
        } else {
            break;
        }
    }
    let rest = t.strip_prefix("fn ")?;
    let name = rest.split(['(', '<']).next()?.trim();
    (!name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')).then_some(name)
}

/// `(file, enclosing fn) → call count` for every non-comment line of
/// `src/` containing `needle`.
fn call_sites(needle: &str) -> BTreeMap<(String, String), usize> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                rust_files(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    assert!(files.len() > 50, "census root wrong: {} files", files.len());
    let mut sites = BTreeMap::new();
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        let rel = f
            .strip_prefix(root)
            .expect("census file outside the manifest root")
            .to_string_lossy()
            .into_owned();
        let mut enclosing = String::from("<none>");
        for line in text.lines() {
            if let Some(name) = fn_item_name(line) {
                enclosing = name.to_string();
            }
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            if line.contains(needle) {
                *sites.entry((rel.clone(), enclosing.clone())).or_insert(0) += 1;
            }
        }
    }
    sites
}

fn expected(list: &[(&str, &str, usize)]) -> BTreeMap<(String, String), usize> {
    list.iter()
        .map(|(f, func, n)| ((f.to_string(), func.to_string()), *n))
        .collect()
}

/// The item grammar recognises every fn item form the tree uses and
/// nothing else (the rail's attribution depends on it).
#[test]
fn the_fn_item_grammar_recognises_every_item_form() {
    for (line, name) in [
        ("fn plain(x: u8) {", "plain"),
        ("    pub fn a_pub() {", "a_pub"),
        ("    pub(crate) async fn crate_async(", "crate_async"),
        ("    pub(super) fn super_fn<T>(t: T)", "super_fn"),
        ("pub(in crate::x) unsafe fn in_unsafe()", "in_unsafe"),
        ("    const fn a_const() -> u8 {", "a_const"),
        ("pub const unsafe fn both() {", "both"),
        ("    pub unsafe extern \"C\" fn c_abi() {", "c_abi"),
        ("    async fn plain_async() {", "plain_async"),
    ] {
        assert_eq!(fn_item_name(line), Some(name), "{line:?}");
    }
    for line in [
        "    // fn commented() {",
        "    let f = |x| fn_like(x);",
        "    self.fn_field();",
        "    // pub fn allocate_placed_block() in a comment",
        "    fnord(3);",
    ] {
        assert_eq!(fn_item_name(line), None, "{line:?}");
    }
}

/// Every caller of the HOOKED act holds at most (1) and (3) — one call
/// each.
#[test]
fn every_hooked_allocation_act_caller_holds_no_meta_stripe_and_no_dlm_guard() {
    let sites = call_sites(".allocate_placed_block()");
    let expected = expected(&[
        // The pack's open-block mint: the Packer's refill, no guard.
        ("src/pack.rs", "open_block", 1),
        // The promotion's own-block landing (dismount sweep / pressure
        // merge worker): a block guard at most.
        ("src/routing.rs", "land_own_block", 1),
        // The user write's striped spill: its level-3.5 guard is DROPPED
        // before the device phase (RES-1); a staged block guard (3) may
        // be held.
        ("src/routing.rs", "write_file", 1),
        // The sparse stripe mint (fallocate / growth): no guard.
        ("src/routing.rs", "durable_write_sparse_blocks", 1),
        // The device-overlay store: under the block guard (3).
        ("src/fuse_client.rs", "try_device_overlay_store", 1),
        // The overlay test seam's install: under the block guard (3).
        ("src/fuse_client.rs", "test_install_overwrite_overlay", 1),
        // The write pipeline's UNLOCKED device phase.
        ("src/fuse_client.rs", "upload_block_dma_phase", 1),
        // The staging-refusal durable escalation: block guard (3) at most.
        ("src/fuse_client.rs", "upload_active_block_bytes", 1),
        // The W2 fold upload: block guard (3) at most.
        ("src/fuse_client.rs", "fold_upload_block", 1),
        // The writeback flush unit — the site that hung the chain: under
        // its block guard (3), no 3.5, no 4a.
        ("src/fuse_client.rs", "flush_one_active_block", 1),
    ]);
    assert_eq!(
        sites, expected,
        "the hooked allocation act's caller census drifted — a new call (a new site, or a \
         second call inside a listed fn) joins this list EXPLICITLY with its lock class \
         stated (it must hold no level-3.5 stripe and no 4a DLM guard: the pressure close's \
         save parks on 4a), or takes `allocate_placed_block_under_guard()` and joins THAT list"
    );
}

/// Every caller of the UNDER-GUARD form holds a guard the hook's close
/// would take.
#[test]
fn every_under_guard_allocation_caller_holds_a_meta_stripe_or_a_dlm_guard() {
    let sites = call_sites(".allocate_placed_block_under_guard()");
    let expected = expected(&[
        // The layout save's CoW indirect-map blob mint: the caller holds
        // the saved ino's level-3.5 stripe (RES-1's venue).
        ("src/routing.rs", "stage_layout_save", 1),
        // The owner-side compose arms' blob mint (the Lever-B pass task,
        // the direct delta path, the chained spill): exclusive 4a `I{}`
        // guards held across the mint.
        ("src/multi_writer.rs", "indirect_map_io_for", 1),
        // The hooked act's own body: the first attempt and the retry.
        ("src/routing.rs", "allocate_placed_block", 2),
    ]);
    assert_eq!(
        sites, expected,
        "the under-guard allocation form's caller census drifted — a new call joins this \
         list EXPLICITLY with the guard it holds stated"
    );
}
