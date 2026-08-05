//! §3d.2 (docs/rc-manifest.md — "Latent same-class flags", item 2): the
//! PRODUCT may not use process-global env mutation as a runtime config
//! channel. The flagged population: `config_ops.rs` (offline drain),
//! `defrag.rs` (`build_offline_router`) and `fsck.rs` (`run_offline_body`)
//! each did `std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE",
//! cfg.block_size)` to teach downstream `default_block_size()` readers the
//! set's configured block size. Latent, not live (the shipped CLI is
//! one-verb-one-process) — but two different-block-size volume sets in ONE
//! process would fight over it, and env writes race every concurrent env
//! read in the process (the same class the test-side DUR-8e fallback fix,
//! `f3237c5f`, closed by deriving per fixture).
//!
//! The registry-style cleanup (owed by §3d.2): the offline verbs route the
//! configured block size through the SAME per-router seams a mount uses —
//! `DataRouter::set_block_size` (already present at all three sites) plus
//! `DataRouter::set_crypto` for the DUR-8e plaintext bound
//! (`init_scratch_pool` records it per state; the mount path's exact
//! discipline) — and never touch the process environment.
//!
//! Two contracts:
//! 1. **The teeth** (the env-knob census pattern): no product source may
//!    contain `set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE"` — the channel is
//!    structurally unrepresentable, so a fourth site cannot be born.
//! 2. **The behavior**: an offline verb (`fsck::run_offline`) on a volume
//!    set whose configured block size differs from the ambient env leaves
//!    the process environment UNTOUCHED.

use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
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

/// Contract 1 — the §3d.2 teeth. Product source roots only (tests and
/// benches legitimately pin the knob per fixture; the harness convention
/// is ENG-10's, not this test's).
#[test]
fn product_code_never_writes_the_block_size_knob_into_the_process_env() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let product_roots = [
        root.join("src"),
        root.join("crates/squeezefs-ipc/src"),
        root.join("crates/squeezefs-preload/src"),
        root.join("crates/fuse3/src"),
    ];
    let mut files = Vec::new();
    for r in &product_roots {
        rust_files(r, &mut files);
    }
    assert!(
        files.len() > 50,
        "source scan must actually see the tree (got {} files)",
        files.len()
    );
    let mut offenders = Vec::new();
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            if line.contains("set_var(\"SQUEEZEFS_DEFAULT_BLOCK_SIZE\"")
                || line.contains("set_var(SQUEEZEFS_DEFAULT_BLOCK_SIZE")
            {
                offenders.push(format!("{}:{}", f.display(), i + 1));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "§3d.2 (docs/rc-manifest.md): product code may not use \
         SQUEEZEFS_DEFAULT_BLOCK_SIZE as a process-global runtime channel — \
         route the configured block size through DataRouter::set_block_size \
         + set_crypto (the mount path's seams). Offenders:\n  {}",
        offenders.join("\n  ")
    );
}

/// Contract 2 — the behavior: `fsck::run_offline` on a 64 KiB-block volume
/// set must not mutate the process environment (pre-fix it stamped the
/// set's block size over whatever the process held).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_fsck_leaves_the_process_env_untouched() {
    const VOLUME_BS: u64 = 65_536;
    const SENTINEL: &str = "2097152"; // valid, ≠ VOLUME_BS, ≠ default

    let dir = tempfile::tempdir().expect("scratch dir");
    let meta = dir.path().join("meta.bin");
    let data = dir.path().join("data.bin");
    std::fs::File::create(&meta)
        .expect("create meta")
        .set_len(256 * 1024 * 1024)
        .expect("size meta");
    std::fs::File::create(&data)
        .expect("create data")
        .set_len(64 * 1024 * 1024)
        .expect("size data");

    let cfg = squeezefs::FormatConfig {
        name: "squeezefs".to_string(),
        block_size: VOLUME_BS,
        capacity: 64 * 1024 * 1024,
        inodes: 1_000_000,
        compression: "none".to_string(),
        encrypt_algo: "none".to_string(),
        encrypt_key: None,
        encrypt_key_ref: None,
        mem_cache_size: None,
        disk_cache_size: None,
        disk_cache_paths: None,
        data_lv: Some(vec![data.display().to_string()]),
        data_volumes: None,
        read_cache_size: None,
        write_cache_size: None,
        read_mem_cache_size: None,
        write_mem_cache_size: None,
        dismount_wait: None,
        upload_delay: None,
        fuse_io_uring_sqpoll_idle_ms: None,
        meta_routing_width: None,
        meta_slot_runs: None,
        meta_volumes: None,
    };
    squeezefs::meta_backend::kv::builder::format_v3(
        &meta,
        256 * 1024 * 1024,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: Some(serde_json::to_vec(&cfg).expect("cfg json")),
        },
    )
    .await
    .expect("format v3 meta volume");

    // The sentinel: a legitimate, distinct ambient value. Set BEFORE the
    // verb; the contract is that the verb never rewrites it. (This test
    // file is its own process; nothing else in it reads the knob.)
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", SENTINEL);

    let report = squeezefs::fsck::run_offline(
        &[meta.display().to_string()],
        &squeezefs::fsck::FsckOptions::offline(),
    )
    .await
    .expect("offline fsck on a fresh volume set");
    assert_eq!(
        report.findings.len(),
        0,
        "fresh volume set must fsck clean (fixture sanity)"
    );

    let after = std::env::var("SQUEEZEFS_DEFAULT_BLOCK_SIZE")
        .expect("the sentinel must still be present");
    assert_eq!(
        after, SENTINEL,
        "§3d.2: the offline verb mutated the process environment — \
         SQUEEZEFS_DEFAULT_BLOCK_SIZE was rewritten from the sentinel to \
         the volume set's block size (the process-global config channel; \
         two different-block-size sets in one process would fight over it)"
    );
}
