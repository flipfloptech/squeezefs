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
//! blob mint holding exclusive 4a `I{}` guards. A 13th caller of either
//! form joins its list here EXPLICITLY, with its lock class stated in the
//! comment beside it — never by drift.

use std::collections::BTreeSet;
use std::path::Path;

/// `(file, enclosing fn)` for every line of `src/` containing `needle`.
/// The enclosing fn is the nearest preceding `fn` item (a closure inside
/// it attributes to it — the compose arms' blob mint is one).
fn call_sites(needle: &str) -> BTreeSet<(String, String)> {
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
    let mut sites = BTreeSet::new();
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
            let t = line.trim_start();
            let t = t.strip_prefix("pub(crate) ").unwrap_or(t);
            let t = t.strip_prefix("pub ").unwrap_or(t);
            let t = t.strip_prefix("async ").unwrap_or(t);
            if let Some(rest) = t.strip_prefix("fn ") {
                if let Some(name) = rest.split(['(', '<']).next() {
                    enclosing = name.trim().to_string();
                }
            }
            if line.contains(needle) {
                sites.insert((rel.clone(), enclosing.clone()));
            }
        }
    }
    sites
}

fn expected(list: &[(&str, &str)]) -> BTreeSet<(String, String)> {
    list.iter()
        .map(|(f, func)| (f.to_string(), func.to_string()))
        .collect()
}

/// Every caller of the HOOKED act holds at most (1) and (3).
#[test]
fn every_hooked_allocation_act_caller_holds_no_meta_stripe_and_no_dlm_guard() {
    let sites = call_sites(".allocate_placed_block()");
    let expected = expected(&[
        // The pack's open-block mint: the Packer's refill, no guard.
        ("src/pack.rs", "open_block"),
        // The promotion's own-block landing (dismount sweep / pressure
        // merge worker): a block guard at most.
        ("src/routing.rs", "land_own_block"),
        // The user write's striped spill: its level-3.5 guard is DROPPED
        // before the device phase (RES-1); a staged block guard (3) may
        // be held.
        ("src/routing.rs", "write_file"),
        // The sparse stripe mint (fallocate / growth): no guard.
        ("src/routing.rs", "durable_write_sparse_blocks"),
        // The device-overlay store: under the block guard (3).
        ("src/fuse_client.rs", "try_device_overlay_store"),
        // The overlay test seam's install: under the block guard (3).
        ("src/fuse_client.rs", "test_install_overwrite_overlay"),
        // The write pipeline's UNLOCKED device phase.
        ("src/fuse_client.rs", "upload_block_dma_phase"),
        // The staging-refusal durable escalation: block guard (3) at most.
        ("src/fuse_client.rs", "upload_active_block_bytes"),
        // The W2 fold upload: block guard (3) at most.
        ("src/fuse_client.rs", "fold_upload_block"),
        // The writeback flush unit — the site that hung the chain: under
        // its block guard (3), no 3.5, no 4a.
        ("src/fuse_client.rs", "flush_one_active_block"),
    ]);
    assert_eq!(
        sites, expected,
        "the hooked allocation act's caller census drifted — a new caller joins this list \
         EXPLICITLY with its lock class stated (it must hold no level-3.5 stripe and no 4a \
         DLM guard: the pressure close's save parks on 4a), or takes \
         `allocate_placed_block_under_guard()` and joins THAT list"
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
        ("src/routing.rs", "stage_layout_save"),
        // The owner-side compose arms' blob mint (the Lever-B pass task,
        // the direct delta path, the chained spill): exclusive 4a `I{}`
        // guards held across the mint.
        ("src/multi_writer.rs", "indirect_map_io_for"),
        // The hooked act's own body.
        ("src/routing.rs", "allocate_placed_block"),
    ]);
    assert_eq!(
        sites, expected,
        "the under-guard allocation form's caller census drifted — a new caller joins this \
         list EXPLICITLY with the guard it holds stated"
    );
}
