//! Write-commit-economy campaign (2026-07-30) — the **kill-9 soak with
//! mid-window deaths** over the layout-delta block-publish commit
//! (`.benchmarks/2026-07-30-write-commit-economy.md`).
//!
//! The crash contract being pinned (design-cow-kv-metadata §4.10, applied
//! to the new commit shape):
//!
//! 1. **Acked durability (D0)** — every ledger-ACKED publish (publish →
//!    `sync_device` barrier → ack line) survives the kill: the folded
//!    layout maps every acked block and its size covers them. This is
//!    the "a synced byte is durable" face of the coalesce window —
//!    fsync/barrier completion means the batch COMMITTED.
//! 2. **SIZE-NEVER-LEADS-DATA** (the generic/795 law, crash face) —
//!    whatever prefix of un-acked publishes survives, the replayed size
//!    NEVER exceeds the mapped coverage: a delta record carries its
//!    batch's size and map entries in ONE record inside ONE checksummed
//!    journal entry, so no crash boundary can separate them.
//! 3. **Replay idempotence** — digest, clean-shutdown, remount: the
//!    post-fold digest walk is identical (layout-delta folds are
//!    byte-deterministic — the canonical map encoding).
//! 4. **Mount never fails loud** on ring/window contents after kill-9
//!    (§4.1 torn-tail classification handles any in-flight entry).
//!
//! The standard re-exec pattern (`crash_kill_tests` lineage): the parent
//! spawns THIS binary as a child (`SQUEEZEFS_WCE_CRASH_CHILD=1`), the
//! child streams block publishes through the EXACT campaign commit
//! (`merge_layout_and_size` — delta records after the first full Put)
//! with a barrier+ack ledger, and the parent SIGKILLs it mid-churn.
//!
//! Rounds: `SQUEEZEFS_WCE_CRASH_ROUNDS` (default 10).

use std::io::Write;
use std::process::{Command, Stdio};

use squeezefs::layout_wire::{LayoutDelta, LayoutMetadata};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{digest_backend, format_v3, FormatV3Options};
use squeezefs::meta_backend::Metadata;

const VOL_SIZE: u64 = 256 * 1024 * 1024;
const BS: u64 = 4 * 1024 * 1024;

fn ledger_append(path: &std::path::Path, line: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open ledger");
    f.write_all(format!("{line}\n").as_bytes())
        .expect("append ledger");
    f.sync_data().expect("fsync ledger");
}

fn v3_format_opts() -> FormatV3Options {
    FormatV3Options {
        node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
        journal_len_override: None,
        force: false,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// Child branch: stream block publishes through the campaign commit —
/// `start publish b` → `merge_layout_and_size` (delta + full fallback,
/// exactly the coalescing pass's save shape) → `sync_device` barrier →
/// `ack publish b`. Every 64th publish sends a full re-base (the
/// routing chain cap's shape), so the journal carries mixed
/// full-Put/delta-chain windows for the kill to land on.
#[test]
fn wce_crash_child_entry() {
    if std::env::var("SQUEEZEFS_WCE_CRASH_CHILD").is_err() {
        return;
    }
    let vol = std::path::PathBuf::from(std::env::var("SQUEEZEFS_WCE_VOL").unwrap());
    let ledger = std::path::PathBuf::from(std::env::var("SQUEEZEFS_WCE_LEDGER").unwrap());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let backend = KvMetaBackend::open(&vol).await.unwrap();
        let ino = Metadata::create(backend.as_ref(), 1, "streamed", libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap()
            .ino;
        backend.sync_device().await.unwrap();
        ledger_append(&ledger, &format!("ack create {ino}"));

        let mut layout = LayoutMetadata {
            file_type: "striped".into(),
            size: 0,
            block_map_id: Some(format!("block_map_{ino}")),
            block_prefix: None,
            file_id: None,
            data_key: None,
            block_map: Some(std::collections::HashMap::new()),
        };
        let mut b: u32 = 0;
        loop {
            let key = format!("oss0://{}", b as u64 * BS);
            layout.block_map.as_mut().unwrap().insert(b, key.clone());
            layout.size = (b as u64 + 1) * BS;
            let full = bincode::serialize(&layout).unwrap();
            ledger_append(&ledger, &format!("start publish {b}"));
            if b > 0 && b.is_multiple_of(64) {
                // The routing chain cap's re-base shape: a full save.
                backend
                    .set_layout_and_size(ino, &full, layout.size, &[])
                    .await
                    .unwrap();
            } else {
                let delta = LayoutDelta::from_final_state(
                    &layout.file_type,
                    layout.size,
                    layout.block_map_id.as_deref(),
                    None,
                    None,
                    None,
                    vec![(b, key)],
                );
                backend
                    .merge_layout_and_size(
                        ino,
                        ino,
                        &delta,
                        bytes::Bytes::from(full.clone()),
                        layout.size,
                        Vec::new(),
                    )
                    .await
                    .unwrap();
            }
            backend.sync_device().await.unwrap();
            ledger_append(&ledger, &format!("ack publish {b}"));
            b += 1;
        }
    });
}

/// The soak: kill-9 at a jittered deadline after the first acked
/// publish; remount and assert the four invariants above.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill9_mid_publish_window_soak() {
    let rounds: u32 = std::env::var("SQUEEZEFS_WCE_CRASH_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let exe = std::env::current_exe().expect("test binary path");

    for round in 0..rounds {
        let dir = tempfile::tempdir().unwrap();
        let vol = dir.path().join("wce.v3.meta");
        let ledger = dir.path().join("ledger.log");
        {
            let f = std::fs::File::create(&vol).unwrap();
            f.set_len(VOL_SIZE).unwrap();
            format_v3(&vol, VOL_SIZE, &v3_format_opts()).await.unwrap();
        }

        let mut child = Command::new(&exe)
            .args([
                "--exact",
                "wce_crash_child_entry",
                "--test-threads=1",
                "--nocapture",
            ])
            .env("SQUEEZEFS_WCE_CRASH_CHILD", "1")
            .env("SQUEEZEFS_WCE_VOL", &vol)
            .env("SQUEEZEFS_WCE_LEDGER", &ledger)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn wce crash child");

        // Anchor the kill jitter on the first acked publish (the
        // crash_kill_tests wall-clock lesson — never bet on load).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut acked_seen = false;
        while std::time::Instant::now() < deadline {
            if ledger.exists()
                && std::fs::read_to_string(&ledger)
                    .map(|s| s.lines().any(|l| l.starts_with("ack publish ")))
                    .unwrap_or(false)
            {
                acked_seen = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(
            acked_seen,
            "round {round}: the child never acked a publish in 60 s — commit pipeline dead"
        );
        let jitter: u64 = {
            use rand::Rng;
            rand::thread_rng().gen_range(5..=50)
        };
        tokio::time::sleep(std::time::Duration::from_millis(jitter)).await;
        child.kill().expect("SIGKILL wce child");
        let _ = child.wait();

        // Ledger model: highest acked publish + the created ino.
        let text = std::fs::read_to_string(&ledger).unwrap();
        let mut max_acked: Option<u32> = None;
        let mut ino: Option<u64> = None;
        for line in text.lines() {
            let toks: Vec<&str> = line.split_whitespace().collect();
            match toks.as_slice() {
                ["ack", "create", i] => ino = i.parse().ok(),
                ["ack", "publish", b] => {
                    let b: u32 = b.parse().unwrap();
                    max_acked = Some(max_acked.map_or(b, |m| m.max(b)));
                }
                _ => {}
            }
        }
        let ino = ino.expect("create acked before any publish");

        // Invariant 4: mount never fails loud after kill-9.
        let m1 = KvMetaBackend::open(&vol)
            .await
            .unwrap_or_else(|e| panic!("round {round}: remount failed loud after kill-9: {e}"));

        let layout_bytes = m1
            .getxattr(ino, "layout")
            .await
            .unwrap_or_else(|e| panic!("round {round}: layout read failed: {e}"));
        let replay = m1.replay_stats();
        eprintln!(
            "[wce-kill9 round {round}] max acked publish: {max_acked:?}; replay: {} entries, \
             {} dropped-torn",
            replay.entries, replay.dropped_torn
        );

        if let Some(max_acked) = max_acked {
            // Invariant 1: acked durability.
            let bytes = layout_bytes.unwrap_or_else(|| {
                panic!("round {round}: acked publishes but no layout xattr after remount")
            });
            let layout: LayoutMetadata =
                bincode::deserialize(&bytes).expect("folded layout decodes");
            let map = layout.block_map.as_ref().expect("map present");
            for b in 0..=max_acked {
                assert!(
                    map.contains_key(&b),
                    "round {round}: ACKED publish of block {b} lost after kill-9 \
                     (acked-durability violation; max acked {max_acked})"
                );
            }
            assert!(
                layout.size >= (max_acked as u64 + 1) * BS,
                "round {round}: acked size regressed: {} < {}",
                layout.size,
                (max_acked as u64 + 1) * BS
            );
            // Invariant 2: SIZE-NEVER-LEADS-DATA — every block the
            // replayed size covers is mapped (whole-record atomicity of
            // the delta: size rides its batch's entries).
            let blocks_needed = layout.size.div_ceil(BS) as u32;
            for b in 0..blocks_needed {
                assert!(
                    map.contains_key(&b),
                    "round {round}: replayed size {} covers block {b} but the map \
                     does not — size led its data across the crash boundary",
                    layout.size
                );
            }
        }

        // Invariant 3: replay idempotence (post-fold digest walk —
        // deterministic layout-delta folds included).
        let d1 = digest_backend(&m1).await.unwrap();
        m1.shutdown()
            .await
            .unwrap_or_else(|e| panic!("round {round}: post-crash shutdown failed: {e}"));
        drop(m1);
        let m2 = KvMetaBackend::open(&vol)
            .await
            .unwrap_or_else(|e| panic!("round {round}: second remount failed: {e}"));
        assert_eq!(
            m2.replay_stats().entries,
            0,
            "round {round}: a clean shutdown must leave an empty replay window"
        );
        let d2 = digest_backend(&m2).await.unwrap();
        assert_eq!(
            d1, d2,
            "round {round}: replay-twice digests diverge — the layout-delta fold \
             is not idempotent/deterministic"
        );
        m2.shutdown().await.unwrap();
    }
}
