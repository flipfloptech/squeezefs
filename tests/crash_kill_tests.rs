//! Kill-9 remount soak over the v3 CoW KV metadata format
//! (design-cow-kv-metadata Rollout 4; grew out of the
//! design-wal-crash-consistency §4.7b v2 soak, whose leg was deleted with
//! v2 support).
//!
//! The standard re-exec pattern: the parent test spawns THIS test binary
//! as a child (`SQUEEZEFS_CRASH_CHILD_V3=1` selects the
//! `crash_child_entry_v3` branch — no production CLI surface added), lets
//! it churn create/setxattr/unlink/destroy against a file-backed volume
//! while appending to a side ledger, SIGKILLs it at a random 5–50 ms
//! deadline, then remounts and asserts the crash contract: **D0 acked
//! durability** — every ledger-ACKED op (op → `sync_device` barrier → ack
//! line) is present after remount, unless a later op-start superseded it —
//! plus whole-transaction atomicity and torn-write immunity by
//! construction (§4.10).
//!
//! Rounds: `SQUEEZEFS_CRASH_ROUNDS` (default 20, < ~30 s; the nightly soak
//! runs 500 via `tests/long_validation.py --crash-soak`).

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::process::{Command, Stdio};

use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{digest_backend, format_v3, FormatV3Options};
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};

fn ledger_append(path: &std::path::Path, line: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open ledger");
    writeln!(f, "{line}").expect("append ledger");
    // Written+fsynced BEFORE the corresponding action proceeds: start
    // records are what make invariant 3's bound assertable.
    f.sync_data().expect("fsync ledger");
}

#[derive(Debug, Default)]
struct LedgerModel {
    lines: Vec<String>,
    started_creates: u64,
    started_destroys: u64,
    unacked_starts: u64,
    acked_inos: HashSet<u64>,
    started_names: HashSet<String>,
}

fn parse_ledger(path: &std::path::Path) -> LedgerModel {
    let mut m = LedgerModel::default();
    let Ok(text) = std::fs::read_to_string(path) else {
        return m;
    };
    let mut acks: HashSet<String> = HashSet::new();
    for line in text.lines() {
        m.lines.push(line.to_string());
        let mut it = line.split_whitespace();
        let (kind, op) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
        match (kind, op) {
            ("start", "create") => {
                m.started_creates += 1;
                if let Some(name) = it.next() {
                    m.started_names.insert(name.to_string());
                }
            }
            ("start", "destroy") => m.started_destroys += 1,
            ("ack", "create") | ("ack", "unlink") => {
                if let Some(ino) = it.nth(1).and_then(|s| s.parse().ok()) {
                    m.acked_inos.insert(ino);
                }
            }
            ("ack", "setxattr") | ("ack", "destroy") => {
                if let Some(ino) = it.next().and_then(|s| s.parse().ok()) {
                    m.acked_inos.insert(ino);
                }
            }
            _ => {}
        }
        if kind == "ack" {
            acks.insert(line["ack ".len()..].to_string());
        }
    }
    for line in &m.lines {
        if let Some(rest) = line.strip_prefix("start ") {
            if !acks.contains(rest) {
                m.unacked_starts += 1;
            }
        }
    }
    m
}

/// D0 replay: compute the acked-final expectation per name (names are
/// unique per round, never reused).
fn acked_expectations(m: &LedgerModel) -> Vec<(String, u64, Expect)> {
    #[derive(Clone, Copy, PartialEq)]
    enum Last {
        AckedCreate,
        AckedUnlink,
        StartedMutation, // a later start makes the final state unknowable
    }
    let mut per_name: HashMap<String, (u64, Last)> = HashMap::new();
    let mut xattr_acked: HashMap<u64, String> = HashMap::new();
    for line in &m.lines {
        let p: Vec<&str> = line.split_whitespace().collect();
        match p.as_slice() {
            ["ack", "create", name, ino] => {
                per_name.insert(name.to_string(), (ino.parse().unwrap(), Last::AckedCreate));
            }
            ["start", "unlink", name] | ["start", "destroy", name] => {
                if let Some(e) = per_name.get_mut(*name) {
                    e.1 = Last::StartedMutation;
                }
                // destroy lines carry an ino, not a name — handled below.
                let _ = name;
            }
            ["ack", "unlink", name, ino] => {
                per_name.insert(name.to_string(), (ino.parse().unwrap(), Last::AckedUnlink));
            }
            ["ack", "setxattr", ino, _key, val] => {
                xattr_acked.insert(ino.parse().unwrap(), val.to_string());
            }
            ["start", "setxattr", ..] => {}
            _ => {}
        }
    }
    // A started destroy invalidates presence expectations for its ino.
    let mut destroyed_started: HashSet<u64> = HashSet::new();
    for line in &m.lines {
        let p: Vec<&str> = line.split_whitespace().collect();
        if let ["start", "destroy", ino] = p.as_slice() {
            if let Ok(i) = ino.parse() {
                destroyed_started.insert(i);
            }
        }
    }
    per_name
        .into_iter()
        .map(|(name, (ino, last))| {
            let expect = match last {
                Last::AckedCreate if !destroyed_started.contains(&ino) => {
                    Expect::Present(xattr_acked.get(&ino).cloned())
                }
                Last::AckedUnlink => Expect::Absent,
                _ => Expect::Unknown,
            };
            (name, ino, expect)
        })
        .collect()
}

enum Expect {
    Present(Option<String>),
    Absent,
    Unknown,
}

// ===========================================================================
// The v3 soak (design Rollout 4), the §4.4 pt 4 rollback-race case, and
// the `disabled_volumes` fail-stop escalation.
// ===========================================================================

/// v3 soak volume: 64 MiB file-backed, small nodes, a small ring so the
/// kill window crosses checkpoint boundaries.
const V3_VOL_SIZE: u64 = 64 * 1024 * 1024;

fn v3_format_opts() -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: Some(1024 * 1024),
        force: false,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// Child branch for the soak rounds: the churn protocol (start line →
/// op → `sync_device` barrier → ack line) through the `KvMetaBackend`
/// `Metadata` surface.
#[test]
fn crash_child_entry_v3() {
    if std::env::var("SQUEEZEFS_CRASH_CHILD_V3").is_err() {
        return;
    }
    let vol = std::path::PathBuf::from(std::env::var("SQUEEZEFS_CRASH_VOL").unwrap());
    let ledger = std::path::PathBuf::from(std::env::var("SQUEEZEFS_CRASH_LEDGER").unwrap());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let backend = KvMetaBackend::open(&vol).await.unwrap();

        let mut i: u64 = 0;
        loop {
            let name = format!("f{i}");

            ledger_append(&ledger, &format!("start create {name}"));
            let ino =
                match Metadata::create(backend.as_ref(), 1, &name, libc::S_IFREG | 0o644, 0, 0)
                    .await
                {
                    Ok(f) => f.ino,
                    Err(_) => break, // volume full mid-kill window — stop quietly
                };
            backend.sync_device().await.unwrap();
            ledger_append(&ledger, &format!("ack create {name} {ino}"));

            ledger_append(&ledger, &format!("start setxattr {ino} user.crash v{i}"));
            Metadata::setxattr(
                backend.as_ref(),
                ino,
                "user.crash",
                format!("v{i}").as_bytes(),
            )
            .await
            .unwrap();
            backend.sync_device().await.unwrap();
            ledger_append(&ledger, &format!("ack setxattr {ino} user.crash v{i}"));

            if i.is_multiple_of(3) {
                ledger_append(&ledger, &format!("start unlink {name}"));
                Metadata::unlink(backend.as_ref(), 1, &name).await.unwrap();
                backend.sync_device().await.unwrap();
                ledger_append(&ledger, &format!("ack unlink {name} {ino}"));

                ledger_append(&ledger, &format!("start destroy {ino}"));
                Metadata::destroy_inode(backend.as_ref(), ino)
                    .await
                    .unwrap();
                backend.sync_device().await.unwrap();
                ledger_append(&ledger, &format!("ack destroy {ino}"));
            }
            i += 1;
        }
        std::future::pending::<()>().await
    });
}

/// The v3 kill-9 soak (design §4.10 "kill-9 soak runs unchanged against
/// v3 volumes with two strengthened assertions"): every ledger-acked op
/// present **and whole** (a create's dentry+inode ride ONE journal entry,
/// so a dangling dentry is the torn shape the assertion catches), and
/// replay idempotence via the post-fold digest walk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_kill9_remount_soak_v3() {
    let rounds: u32 = std::env::var("SQUEEZEFS_CRASH_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let exe = std::env::current_exe().expect("test binary path");

    for round in 0..rounds {
        let dir = tempfile::tempdir().unwrap();
        let vol = dir.path().join("crash.v3.meta");
        let ledger = dir.path().join("ledger.log");

        // Parent formats; the child mounts + churns.
        {
            let f = std::fs::File::create(&vol).unwrap();
            f.set_len(V3_VOL_SIZE).unwrap();
            format_v3(&vol, V3_VOL_SIZE, &v3_format_opts())
                .await
                .unwrap();
        }

        let mut child = Command::new(&exe)
            .args([
                "--exact",
                "crash_child_entry_v3",
                "--test-threads=1",
                "--nocapture",
            ])
            .env("SQUEEZEFS_CRASH_CHILD_V3", "1")
            .env("SQUEEZEFS_CRASH_VOL", &vol)
            .env("SQUEEZEFS_CRASH_LEDGER", &ledger)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn v3 crash child");

        // Wait for the FIRST ACKED op, then kill inside a jittered window.
        // The kill must land mid-churn, but "child acked something within
        // 50 ms of its first ledger line" is a load-sensitive wall-clock
        // assumption, not a crash-consistency invariant: under full-suite
        // load (this binary is serial, but the box is not idle) the first
        // create+fdatasync can take hundreds of ms, the SIGKILL landed
        // before any ack, and the ≥1-ack sanity assert below flaked —
        // 4/4 green isolated, red once per full-suite run. Anchoring the
        // jitter on the first ack keeps every invariant (the kill still
        // interrupts live churn; the ack floor is guaranteed) without the
        // wall-clock bet.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut acked_seen = false;
        while std::time::Instant::now() < deadline {
            if ledger.exists()
                && std::fs::read_to_string(&ledger)
                    .map(|s| s.lines().any(|l| l.starts_with("ack ")))
                    .unwrap_or(false)
            {
                acked_seen = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(
            acked_seen,
            "round {round}: the v3 child never acked a single op in 60 s — commit pipeline dead"
        );
        let jitter: u64 = {
            use rand::Rng;
            rand::thread_rng().gen_range(5..=50)
        };
        tokio::time::sleep(std::time::Duration::from_millis(jitter)).await;
        child.kill().expect("SIGKILL v3 child");
        let _ = child.wait();

        // ---- Remount + the v3 invariants -------------------------------
        let m = parse_ledger(&ledger);
        assert!(
            m.lines.iter().any(|l| l.starts_with("ack ")),
            "round {round}: acked line vanished from the ledger between kill and parse"
        );

        // Mount #1: superblock → ledger → bitmap → replay. NEVER loud for
        // ring/window contents after a kill (§4.1).
        let m1 = KvMetaBackend::open(&vol)
            .await
            .unwrap_or_else(|e| panic!("round {round}: v3 remount failed loud: {e}"));

        // Whole-tx atomicity: every dentry in the volume resolves to a
        // live inode that agrees on the ino — a create is one entry, so a
        // dangling dentry or missing inode is a torn-transaction artifact.
        let listing = m1.readdir(1, 0, usize::MAX).await.unwrap();
        for d in &listing {
            let got = m1.getattr(d.ino).await.unwrap_or_else(|e| {
                panic!(
                    "round {round}: dentry '{}' names ino {} with no inode record \
                     (partial transaction visible): {e}",
                    d.name, d.ino
                )
            });
            assert_eq!(got.ino, d.ino);
        }

        // D0: acked ops present (unless superseded), acked xattr values
        // intact, acked unlinks stay unlinked.
        let expectations = acked_expectations(&m);
        eprintln!(
            "[kill9-v3 round {round}] ledger: {} lines, {} acked inos, {} un-acked starts; \
             {} live root dentries; replay: {} entries, {} dropped",
            m.lines.len(),
            m.acked_inos.len(),
            m.unacked_starts,
            listing.len(),
            m1.replay_stats().entries,
            m1.replay_stats().dropped_torn,
        );
        for (name, ino, expect) in expectations {
            match expect {
                Expect::Present(xattr) => {
                    let found = m1.lookup(1, &name).await.unwrap_or_else(|e| {
                        panic!("round {round}: acked create '{name}' lost after kill-9: {e}")
                    });
                    assert_eq!(found.ino, ino, "round {round}: '{name}' resolved wrong ino");
                    if let Some(val) = xattr {
                        let stored = m1
                            .getxattr(ino, "user.crash")
                            .await
                            .expect("xattr read")
                            .unwrap_or_else(|| {
                                panic!("round {round}: acked xattr on ino {ino} lost")
                            });
                        assert_eq!(
                            stored,
                            val.as_bytes(),
                            "round {round}: acked xattr value mismatch on ino {ino}"
                        );
                    }
                }
                Expect::Absent => {
                    assert!(
                        m1.lookup(1, &name).await.is_err(),
                        "round {round}: acked unlink '{name}' resurrected after kill-9"
                    );
                }
                Expect::Unknown => {}
            }
        }

        // §4.8: the recovered watermark clears every acked ino.
        if let Some(max_acked) = m.acked_inos.iter().max() {
            assert!(
                m1.next_ino() > *max_acked,
                "round {round}: next_ino {} does not clear acked ino {max_acked}",
                m1.next_ino()
            );
        }

        // Replay idempotence (§4.10): digest, clean-shutdown, remount —
        // the post-fold digest walk must be identical, and the second
        // mount's window empty (the shutdown checkpointed it away).
        let d1 = digest_backend(&m1).await.unwrap();
        m1.shutdown()
            .await
            .unwrap_or_else(|e| panic!("round {round}: post-crash shutdown failed: {e}"));
        drop(m1);
        let m2 = KvMetaBackend::open(&vol)
            .await
            .unwrap_or_else(|e| panic!("round {round}: second v3 remount failed: {e}"));
        assert_eq!(
            m2.replay_stats().entries,
            0,
            "round {round}: a clean shutdown must leave an empty replay window"
        );
        let d2 = digest_backend(&m2).await.unwrap();
        assert_eq!(
            d1, d2,
            "round {round}: replay-twice digests diverge — replay is not idempotent"
        );
    }
}

// ===========================================================================
// PR M7 (design-metadata-throughput §5.5 D5): the kill-9 soak with real
// GROUP FORMATION — the serial child above exercises batch-of-1; this
// child runs 3 concurrent lanes in one shared parent (SHARED parent
// I-stripe ⇒ co-queueable Δtime writers), so kill-9 lands on multi-tx
// conveyor batches: contiguous multi-entry reservations, one
// write_at_batch in flight, per-tx acks. Same ledger protocol per lane
// (names are lane-unique), same D0 / whole-tx / idempotence assertions.
// ===========================================================================

/// Child branch for the BATCHED soak rounds: 3 concurrent churn lanes
/// through the routed backend (the mount shape — shared parent stripe).
#[test]
fn crash_child_entry_v3_batched() {
    if std::env::var("SQUEEZEFS_CRASH_CHILD_V3_BATCHED").is_err() {
        return;
    }
    let vol = std::path::PathBuf::from(std::env::var("SQUEEZEFS_CRASH_VOL").unwrap());
    let ledger = std::path::PathBuf::from(std::env::var("SQUEEZEFS_CRASH_LEDGER").unwrap());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let be = KvMetaBackend::open(&vol).await.unwrap();
        let routed = std::sync::Arc::new(RoutedMetaBackend::new(vec![be.clone()]));

        let mut lanes = Vec::new();
        for lane in 0..3u32 {
            let routed = routed.clone();
            let be = be.clone();
            let ledger = ledger.clone();
            lanes.push(tokio::spawn(async move {
                let mut i: u64 = 0;
                loop {
                    let name = format!("l{lane}-f{i}");
                    ledger_append(&ledger, &format!("start create {name}"));
                    let ino = match routed.create(1, &name, libc::S_IFREG | 0o644, 0, 0).await {
                        Ok(f) => f.ino,
                        Err(_) => break, // volume full mid-kill window
                    };
                    be.sync_device().await.unwrap();
                    ledger_append(&ledger, &format!("ack create {name} {ino}"));

                    ledger_append(&ledger, &format!("start setxattr {ino} user.crash v{i}"));
                    routed
                        .setxattr(ino, "user.crash", format!("v{i}").as_bytes())
                        .await
                        .unwrap();
                    be.sync_device().await.unwrap();
                    ledger_append(&ledger, &format!("ack setxattr {ino} user.crash v{i}"));

                    if i.is_multiple_of(3) {
                        ledger_append(&ledger, &format!("start unlink {name}"));
                        routed.unlink(1, &name).await.unwrap();
                        be.sync_device().await.unwrap();
                        ledger_append(&ledger, &format!("ack unlink {name} {ino}"));

                        ledger_append(&ledger, &format!("start destroy {ino}"));
                        routed.destroy_inodes(&[ino]).await.unwrap();
                        be.sync_device().await.unwrap();
                        ledger_append(&ledger, &format!("ack destroy {ino}"));
                    }
                    i += 1;
                }
            }));
        }
        for l in lanes {
            let _ = l.await;
        }
        std::future::pending::<()>().await
    });
}

/// The batched-commit kill-9 soak: identical invariants to
/// [`test_kill9_remount_soak_v3`] — D0 acked durability, whole-tx
/// atomicity, acked-unlink permanence, watermark clearance, replay
/// idempotence — under kill windows that interrupt live multi-tx
/// conveyor batches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_kill9_remount_soak_v3_batched() {
    let rounds: u32 = std::env::var("SQUEEZEFS_CRASH_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let exe = std::env::current_exe().expect("test binary path");

    for round in 0..rounds {
        let dir = tempfile::tempdir().unwrap();
        let vol = dir.path().join("crash-batched.v3.meta");
        let ledger = dir.path().join("ledger.log");
        {
            let f = std::fs::File::create(&vol).unwrap();
            f.set_len(V3_VOL_SIZE).unwrap();
            format_v3(&vol, V3_VOL_SIZE, &v3_format_opts())
                .await
                .unwrap();
        }

        let mut child = Command::new(&exe)
            .args([
                "--exact",
                "crash_child_entry_v3_batched",
                "--test-threads=1",
                "--nocapture",
            ])
            .env("SQUEEZEFS_CRASH_CHILD_V3_BATCHED", "1")
            .env("SQUEEZEFS_CRASH_VOL", &vol)
            .env("SQUEEZEFS_CRASH_LEDGER", &ledger)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn v3 batched crash child");

        // First-ack anchor + jittered kill (the serial soak's rationale).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut acked_seen = false;
        while std::time::Instant::now() < deadline {
            if ledger.exists()
                && std::fs::read_to_string(&ledger)
                    .map(|s| s.lines().any(|l| l.starts_with("ack ")))
                    .unwrap_or(false)
            {
                acked_seen = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(
            acked_seen,
            "round {round}: the batched child never acked an op in 60 s — conveyor dead"
        );
        let jitter: u64 = {
            use rand::Rng;
            rand::thread_rng().gen_range(5..=50)
        };
        tokio::time::sleep(std::time::Duration::from_millis(jitter)).await;
        child.kill().expect("SIGKILL batched child");
        let _ = child.wait();

        let m = parse_ledger(&ledger);
        let m1 = KvMetaBackend::open(&vol)
            .await
            .unwrap_or_else(|e| panic!("round {round}: batched remount failed loud: {e}"));

        // Whole-tx atomicity across batch members: every dentry resolves.
        let listing = m1.readdir(1, 0, usize::MAX).await.unwrap();
        for d in &listing {
            let got = m1.getattr(d.ino).await.unwrap_or_else(|e| {
                panic!(
                    "round {round}: dentry '{}' names ino {} with no inode record \
                     (torn batch member split a tx): {e}",
                    d.name, d.ino
                )
            });
            assert_eq!(got.ino, d.ino);
        }

        // D0 acked durability per lane-unique name.
        for (name, ino, expect) in acked_expectations(&m) {
            match expect {
                Expect::Present(xattr) => {
                    let found = m1.lookup(1, &name).await.unwrap_or_else(|e| {
                        panic!(
                            "round {round}: acked create '{name}' lost after a mid-batch \
                             kill-9: {e}"
                        )
                    });
                    assert_eq!(found.ino, ino);
                    if let Some(val) = xattr {
                        let stored = m1
                            .getxattr(ino, "user.crash")
                            .await
                            .expect("xattr read")
                            .unwrap_or_else(|| {
                                panic!("round {round}: acked xattr on ino {ino} lost")
                            });
                        assert_eq!(stored, val.as_bytes());
                    }
                }
                Expect::Absent => {
                    assert!(
                        m1.lookup(1, &name).await.is_err(),
                        "round {round}: acked unlink '{name}' resurrected"
                    );
                }
                Expect::Unknown => {}
            }
        }
        if let Some(max_acked) = m.acked_inos.iter().max() {
            assert!(m1.next_ino() > *max_acked, "round {round}: watermark low");
        }

        // Replay idempotence under batches.
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
            "round {round}: clean shutdown must leave an empty replay window"
        );
        let d2 = digest_backend(&m2).await.unwrap();
        assert_eq!(d1, d2, "round {round}: replay-twice digests diverge");
        m2.shutdown().await.unwrap();
    }
}

// ===========================================================================
// PR M1 (design-metadata-throughput §5.0): kill-9 after arm ⇒ same-host
// instant reclaim. The killed daemon leaves a heartbeat-FRESH writer_claim
// (10 s cadence vs 45 s TTL) — the remount must reclaim it immediately via
// the dead-pid proof (boot id matches this boot AND kill(pid,0) == ESRCH),
// never waiting out the TTL (design §5.0 B2 "no wait ever" / R6).
// ===========================================================================

/// Child branch: mount the volume (taking the writer claim), signal
/// readiness through a marker file, then park forever holding the mount —
/// the parent SIGKILLs us mid-hold.
#[test]
fn guard_child_hold_v3() {
    if std::env::var("SQUEEZEFS_GUARD_HOLD_CHILD").is_err() {
        return;
    }
    let vol = std::path::PathBuf::from(std::env::var("SQUEEZEFS_CRASH_VOL").unwrap());
    let ready = std::path::PathBuf::from(std::env::var("SQUEEZEFS_GUARD_READY").unwrap());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let backend = KvMetaBackend::open(&vol).await.expect("child mounts");
        // Prove the claim is durable before signaling armed.
        backend.sync_device().await.expect("claim durable");
        std::fs::write(&ready, format!("{}", std::process::id())).expect("ready marker");
        std::future::pending::<()>().await
    });
}

/// Kill-9 after arm ⇒ same-host instant reclaim (crash case, PR M1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_kill9_after_arm_same_host_instant_reclaim() {
    let dir = tempfile::tempdir().unwrap();
    let vol = dir.path().join("guard.v3.meta");
    let ready = dir.path().join("armed");
    {
        let f = std::fs::File::create(&vol).unwrap();
        f.set_len(V3_VOL_SIZE).unwrap();
        format_v3(&vol, V3_VOL_SIZE, &v3_format_opts())
            .await
            .unwrap();
    }

    let exe = std::env::current_exe().expect("test binary path");
    let mut child = Command::new(&exe)
        .args([
            "--exact",
            "guard_child_hold_v3",
            "--test-threads=1",
            "--nocapture",
        ])
        .env("SQUEEZEFS_GUARD_HOLD_CHILD", "1")
        .env("SQUEEZEFS_CRASH_VOL", &vol)
        .env("SQUEEZEFS_GUARD_READY", &ready)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn guard-hold child");

    // Wait for the child to arm (claim committed + barriered).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !ready.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(ready.exists(), "child never armed in 60 s");
    // The ready-file create+write is not atomic: under load this reader
    // can observe the file before its pid write landed (a full-suite
    // ParseIntError flake, 2026-07-21). Poll until the pid parses.
    let child_pid: u32 = loop {
        if let Ok(pid) = std::fs::read_to_string(&ready)
            .unwrap_or_default()
            .trim()
            .parse()
        {
            break pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "child ready file never carried a pid"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };

    // While the child holds: this process must be refused (two daemons,
    // one meta volume — incident 4's exact shape, cross-process).
    assert!(
        KvMetaBackend::open(&vol).await.is_err(),
        "a second process must be refused while the child daemon holds the volume"
    );

    child.kill().expect("SIGKILL the armed holder");
    let _ = child.wait();

    // Instant reclaim: the claim is heartbeat-FRESH (killed seconds after
    // arming) — only the dead-pid proof can admit us, and it must do so
    // immediately (no TTL wait; kernel released the flock at kill).
    let started = std::time::Instant::now();
    let be = KvMetaBackend::open(&vol)
        .await
        .expect("kill-9'd holder must be reclaimed instantly via dead-pid proof");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "reclaim must be instant, not TTL-gated (took {:?})",
        started.elapsed()
    );
    let claim = be
        .read_writer_claim()
        .await
        .expect("the reclaiming mount re-commits the claim");
    assert_eq!(claim.pid, std::process::id(), "the claim now names us");
    assert_ne!(claim.pid, child_pid, "the dead holder's claim was replaced");

    // The reclaimed volume serves.
    Metadata::create(be.as_ref(), 1, "reclaimed", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("post-reclaim create");
    be.shutdown().await.unwrap();
}

/// Physical file offset of the FIRST byte a reservation starting at
/// logical ring position `pos` would write (the §4.4 fault-arming helper:
/// entry headers start at the reservation's first logical byte).
fn journal_physical_offset(be: &KvMetaBackend, pos: u64) -> u64 {
    let geo = *be.journal_ring().core().geometry();
    be.superblock().journal.start + geo.page_index(pos) * 4096 + 24 + geo.in_page_off(pos)
}

/// **The §4.4 pt 4 rollback-race case**: two shared-parent-lock creates
/// race; a persistent write error at the ring head fails exactly the
/// first reservation's entry write. The failed writer's seq-conditional
/// rollback must (a) roll its OWN records back, (b) leave the concurrent
/// committed Δtime (and every other committed record) standing, and (c)
/// leave RAM == replay — asserted via the post-fold digest against a
/// fresh mount of the same bytes, every round.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_rollback_race_seq_conditional() {
    // Park the checkpoint cadence: the per-round digest check mounts a
    // second (read-only-in-spirit) backend on the same bytes, and two
    // live checkpoint writers on one file is not a supported shape. With
    // the cadence at 60 s neither task writes during the test, and the
    // armed ring-head fault can only be taken by one of the two racing
    // user commits — deterministic.
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
            squeezefs::uring_fs::clear_faults();
        }
    }
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let _cleanup = Cleanup;
    let dir = tempfile::tempdir().unwrap();
    let vol = dir.path().join("rollback.v3.meta");
    std::fs::File::create(&vol)
        .unwrap()
        .set_len(V3_VOL_SIZE)
        .unwrap();
    format_v3(&vol, V3_VOL_SIZE, &v3_format_opts())
        .await
        .unwrap();

    let be = KvMetaBackend::open(&vol).await.unwrap();
    let routed = std::sync::Arc::new(RoutedMetaBackend::new(vec![be.clone()]));

    let parent = routed
        .create(1, "racedir", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;

    let mut failures_seen = 0u32;
    for round in 0..24u32 {
        // Arm a persistent single-offset write error at the CURRENT ring
        // head: the next reservation's entry write fails; every later
        // reservation (different offsets) succeeds.
        let head = be.journal_ring().core().head();
        squeezefs::uring_fs::arm_sector_write_error(journal_physical_offset(&be, head));

        let a = {
            let r = routed.clone();
            let name = format!("race-a-{round}");
            tokio::spawn(async move { r.create(parent, &name, libc::S_IFREG | 0o644, 0, 0).await })
        };
        let b = {
            let r = routed.clone();
            let name = format!("race-b-{round}");
            tokio::spawn(async move { r.create(parent, &name, libc::S_IFREG | 0o644, 0, 0).await })
        };
        let (ra, rb) = (a.await.unwrap(), b.await.unwrap());
        squeezefs::uring_fs::clear_faults();

        // The armed head offset is taken by the FIRST batch the conveyor
        // writes (the checkpoint cadence is parked, so no other
        // reservation can absorb it). PR M7 makes the failure population
        // arrival-dependent: if the two creates co-batched, the batch's
        // one `write_at_batch` takes the fault and BOTH roll back (§5.5:
        // write failure fails every batch member — the whole-batch
        // seq-conditional rollback); if they landed in separate batches,
        // the first fails and the second commits at a clean offset.
        // Either way at least one fails and never zero.
        let failed: Vec<&str> = [("a", &ra), ("b", &rb)]
            .iter()
            .filter(|(_, r)| r.is_err())
            .map(|(n, _)| *n)
            .collect();
        assert!(
            (1..=2).contains(&failed.len()),
            "round {round}: the armed ring-head fault must fail the first batch — one \
             member (split batches) or both (co-batched) (got a={ra:?} b={rb:?})"
        );
        failures_seen += failed.len() as u32;

        // Keep the round genuinely SPORADIC (fail-success alternation):
        // when both racers co-batched, the round had no successful user
        // commit, and three such rounds back-to-back are three
        // consecutive write failures — which correctly latches fail-stop
        // (a volume cannot tell per-round faults from a dying device).
        // The pre-M7 shape always interleaved a success; restore it.
        routed
            .create(
                parent,
                &format!("spacer-{round}"),
                libc::S_IFREG | 0o644,
                0,
                0,
            )
            .await
            .unwrap_or_else(|e| {
                panic!("round {round}: post-fault spacer create must succeed: {e}")
            });

        // The survivor resolves; the failed name does not (rolled back).
        for (name, res) in [
            (format!("race-a-{round}"), &ra),
            (format!("race-b-{round}"), &rb),
        ] {
            match res {
                Ok(inode) => {
                    let got = routed.lookup(parent, &name).await.unwrap_or_else(|e| {
                        panic!("round {round}: committed create '{name}' must resolve: {e}")
                    });
                    assert_eq!(got.ino, inode.ino);
                }
                Err(_) => {
                    assert!(
                        routed.lookup(parent, &name).await.is_err(),
                        "round {round}: failed create '{name}' still visible — rollback leaked"
                    );
                }
            }
        }

        // RAM == replay (the §4.4 pt 4 theorem): a fresh PROBE of the
        // same bytes folds to exactly the live in-RAM state — the failed
        // writer's hole is dropped, the concurrent committed Δtime on the
        // shared parent-key survives the rollback. (open_probe, not open:
        // `be` is still live-mounted, and the single-writer guard now
        // refuses a second write mount of one volume — which is also why
        // the old comment called this remount "read-only-in-spirit".)
        let d_live = squeezefs::meta_backend::kv::builder::digest_walk(&be.trees())
            .await
            .unwrap();
        let replayed = KvMetaBackend::open_probe(&vol).await.unwrap();
        let d_replay = digest_backend(&replayed).await.unwrap();
        assert_eq!(
            d_live, d_replay,
            "round {round}: live state and replay diverged after the rollback race"
        );
    }
    assert!(
        failures_seen > 0,
        "the armed ring-head fault never fired — the race case tested nothing"
    );

    // The single-offset failures were sporadic, not repeated: the volume
    // must NOT have escalated to fail-stop.
    assert!(
        !be.is_failed(),
        "sporadic single-write failures must not latch the volume failed"
    );
}

/// §4.4 pt 4 escalation: repeated journal-write failures (a poisoned
/// device) latch the volume failed — mutations return EIO fast, reads
/// keep serving, and the routed layer mirrors the latch into
/// `disabled_volumes` (the existing fail-stop mechanism).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_repeated_journal_failures_escalate_to_disabled_volume() {
    let dir = tempfile::tempdir().unwrap();
    let vol = dir.path().join("escalate.v3.meta");
    std::fs::File::create(&vol)
        .unwrap()
        .set_len(V3_VOL_SIZE)
        .unwrap();
    format_v3(&vol, V3_VOL_SIZE, &v3_format_opts())
        .await
        .unwrap();
    let be = KvMetaBackend::open(&vol).await.unwrap();
    let routed = std::sync::Arc::new(RoutedMetaBackend::new(vec![be.clone()]));

    routed
        .create(1, "pre-fail", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("healthy create");

    // Kill the device: the torn-write fault poisons the path — every
    // subsequent request on it fails EIO (the 'device died' model).
    let head = be.journal_ring().core().head();
    squeezefs::uring_fs::arm_torn_write(journal_physical_offset(&be, head), 0);

    let mut failures = 0;
    for i in 0..8 {
        if routed
            .create(1, &format!("dead-{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .is_err()
        {
            failures += 1;
        }
        if be.is_failed() {
            break;
        }
    }
    // The exact op count before the latch is an implementation detail
    // (a failed op's journal write AND its hole-draining checkpoint both
    // count against a dead device); the CONTRACT is: ops fail, and the
    // volume latches fail-stop.
    assert!(failures >= 1, "poisoned-path creates must fail");
    assert!(
        be.is_failed(),
        "repeated journal write failures must latch the volume failed (§4.4 pt 4)"
    );
    squeezefs::uring_fs::clear_faults();

    // The latch holds after the fault clears: EIO until remount.
    assert!(
        routed
            .create(1, "post-fail", libc::S_IFREG | 0o644, 0, 0)
            .await
            .is_err(),
        "a failed volume must refuse mutations until remount"
    );
    // The routed layer mirrored the latch into disabled_volumes.
    assert!(
        routed.disabled_volumes.contains_key(&0),
        "the failed volume must be marked in disabled_volumes"
    );
    // Reads on the already-mounted state keep serving (fail-stop is for
    // mutations; the RAM-authoritative read side is intact).
    assert!(be.lookup(1, "pre-fail").await.is_ok());
}
