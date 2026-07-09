//! Kill-9 remount soak (design-wal-crash-consistency §4.7b, PR 3).
//!
//! The standard re-exec pattern: the parent test spawns THIS test binary as
//! a child (`SQUEEZEFS_CRASH_CHILD=1` selects the `crash_child_entry`
//! branch — no production CLI surface added), lets it churn
//! create/setxattr/unlink/destroy against a file-backed volume while
//! appending to a side ledger, SIGKILLs it at a random 5–50 ms deadline,
//! then remounts and asserts the crash contract:
//!
//! 1. **D0 acked durability** — every ledger-ACKED op (op → `sync_device`
//!    barrier → ack line) is present after remount, unless a later op-start
//!    superseded it.
//! 2. **Per-sector consistency** — full-table sweep: every inode slot magic
//!    ∈ {0, NODE}; the dentry index builds; every indexed dentry's
//!    child_ino resolves to a magic-valid slot.
//! 3. **Explained reconciliation** — the on-disk bitmap is written only at
//!    mount/clean unmount, so post-kill healed bits reflect the whole
//!    session's net table delta: (a) no wild bits — every differing bit
//!    (quarantine range masked, §4.4) is explained by the session ledger;
//!    (b) `meta_inode_alloc_reconciled` ≤ the ledger's started creates +
//!    destroys. Op-START records exist precisely so (a)/(b) are assertable.
//!
//! Rounds: `SQUEEZEFS_CRASH_ROUNDS` (default 20, < ~30 s; the nightly soak
//! runs 500 via `tests/long_validation.py --crash-soak`).

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::process::{Command, Stdio};

use squeezefs::meta_backend::inode::read_inode;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{digest_backend, format_v3, FormatV3Options};
use squeezefs::meta_backend::storage::MetaLvStorage;
use squeezefs::meta_backend::{MetaLvBackend, Metadata, RoutedMetaBackend, VolumeBackend};

/// 130 MiB: covers the full journal region (108 MiB) and the quarantine
/// range (limit = (130−72) MiB / 32 KiB = 1856 inos > 1152) while keeping
/// the per-round wipe + table sweep fast.
const VOL_SIZE: u64 = 130 * 1024 * 1024;
const QUARANTINE: std::ops::Range<u64> = 1024..1152;

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

/// Child branch: churn until killed. A no-op under a normal test run.
#[test]
fn crash_child_entry() {
    if std::env::var("SQUEEZEFS_CRASH_CHILD").is_err() {
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
        let storage = MetaLvStorage::open(&vol, VOL_SIZE).unwrap();
        // Mimic the real mount sequence (main.rs): seed + refresh before ops.
        storage.seed_inode_alloc_from_table().await.unwrap();
        storage.refresh_bitmap_from_table().await.unwrap();
        let backend = MetaLvBackend::new(storage);

        let mut i: u64 = 0;
        loop {
            let name = format!("f{i}");

            ledger_append(&ledger, &format!("start create {name}"));
            let ino = match backend.create(1, &name, libc::S_IFREG | 0o644, 0, 0).await {
                Ok(f) => f.ino,
                Err(_) => break, // volume full mid-kill window — stop quietly
            };
            backend.sync_device().await.unwrap();
            ledger_append(&ledger, &format!("ack create {name} {ino}"));

            ledger_append(&ledger, &format!("start setxattr {ino} user.crash v{i}"));
            squeezefs::meta_backend::xattr::set_xattr(
                &backend.storage,
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
                backend.unlink(1, &name).await.unwrap();
                backend.sync_device().await.unwrap();
                ledger_append(&ledger, &format!("ack unlink {name} {ino}"));

                ledger_append(&ledger, &format!("start destroy {ino}"));
                backend.destroy_inode(ino).await.unwrap();
                backend.sync_device().await.unwrap();
                ledger_append(&ledger, &format!("ack destroy {ino}"));
            }
            i += 1;
        }
        // Churn ended early (volume full): idle until the parent's SIGKILL.
        std::future::pending::<()>().await
    });
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_kill9_remount_soak() {
    let rounds: u32 = std::env::var("SQUEEZEFS_CRASH_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let exe = std::env::current_exe().expect("test binary path");

    for round in 0..rounds {
        let dir = tempfile::tempdir().unwrap();
        let vol = dir.path().join("crash.meta");
        let ledger = dir.path().join("ledger.log");

        // Parent formats; the child mounts + churns.
        {
            let storage = MetaLvStorage::open(&vol, VOL_SIZE).unwrap();
            MetaLvBackend::format_v2_for_tests(&storage, true, true, None)
                .await
                .unwrap();
        }

        let mut child = Command::new(&exe)
            .args([
                "--exact",
                "crash_child_entry",
                "--test-threads=1",
                "--nocapture",
            ])
            .env("SQUEEZEFS_CRASH_CHILD", "1")
            .env("SQUEEZEFS_CRASH_VOL", &vol)
            .env("SQUEEZEFS_CRASH_LEDGER", &ledger)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn crash child");

        // Wait for churn to actually begin (re-exec startup is not the
        // interesting window), then kill inside 5–50 ms of live traffic.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ledger.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(
            ledger.exists(),
            "round {round}: child never started churning"
        );
        let jitter: u64 = {
            use rand::Rng;
            rand::thread_rng().gen_range(5..=50)
        };
        tokio::time::sleep(std::time::Duration::from_millis(jitter)).await;
        child.kill().expect("SIGKILL child");
        let _ = child.wait();

        // ---- Remount + invariants -------------------------------------
        let storage = MetaLvStorage::open(&vol, VOL_SIZE).unwrap();
        storage
            .validate_superblock()
            .await
            .expect("superblock must validate after kill-9");

        // Bitmap snapshot BEFORE the refresh (the child wrote it at its
        // mount; nothing since).
        let mut prior = [0u8; 4096];
        storage.read_blocks_direct(4096, &mut prior).await.unwrap();

        // Invariant 2a: every inode slot magic ∈ {0, NODE}.
        let limit = storage.inode_alloc.limit();
        let mut valid_inos: HashSet<u64> = HashSet::new();
        {
            let mut sector = [0u8; 4096];
            let mut ino = 0u64;
            while ino < limit {
                let off = 8192 + (ino / 16) * 4096;
                storage.read_blocks_direct(off, &mut sector).await.unwrap();
                for slot in 0..16 {
                    let cur = ino + slot as u64;
                    if cur >= limit {
                        break;
                    }
                    let base = slot * 256;
                    let magic =
                        u32::from_le_bytes(sector[base + 48..base + 52].try_into().unwrap());
                    assert!(
                        magic == 0 || magic == 0x4E4F4445,
                        "round {round}: ino {cur} slot magic {magic:#x} is neither empty nor NODE"
                    );
                    if magic == 0x4E4F4445 {
                        valid_inos.insert(cur);
                    }
                }
                ino += 16;
            }
        }

        // Invariant 2b: dentry index builds; every dentry resolves.
        storage
            .ensure_dentry_index()
            .await
            .expect("dentry index must build after kill-9");
        // The index is parent-keyed and the churn lives entirely in root.
        let mut dentry_names: HashMap<u64, String> = HashMap::new();
        storage.dentry_index.read_sync(&1u64, |_, chain| {
            for (_off, d) in chain {
                dentry_names.insert(d.child_ino, d.get_name());
            }
        });
        for (child_ino, name) in &dentry_names {
            assert!(
                valid_inos.contains(child_ino),
                "round {round}: dentry '{name}' points at ino {child_ino} with no valid slot"
            );
        }

        // Invariant 3: reconciliation is explained, not merely small.
        use std::sync::atomic::Ordering;
        let m = parse_ledger(&ledger);
        storage.seed_inode_alloc_from_table().await.unwrap();
        let before = squeezefs::fuse_client::METRICS
            .meta_inode_alloc_reconciled
            .load(Ordering::Relaxed);
        storage.refresh_bitmap_from_table().await.unwrap();
        let healed = squeezefs::fuse_client::METRICS
            .meta_inode_alloc_reconciled
            .load(Ordering::Relaxed)
            - before;
        assert!(
            healed <= m.started_creates + m.started_destroys,
            "round {round}: healed {healed} bits > ledger-started creates {} + destroys {}",
            m.started_creates,
            m.started_destroys
        );

        let mut post = [0u8; 4096];
        storage.read_blocks_direct(4096, &mut post).await.unwrap();
        let mut unexplained = 0u64;
        for ino in 2..limit {
            if QUARANTINE.contains(&ino) {
                continue; // §4.4 mask: format/mount-set, no table backing
            }
            let bit = |bm: &[u8; 4096]| bm[(ino / 8) as usize] & (1 << (ino % 8)) != 0;
            if bit(&prior) == bit(&post) {
                continue;
            }
            let explained = m.acked_inos.contains(&ino)
                || dentry_names
                    .get(&ino)
                    .map(|n| m.started_names.contains(n))
                    .unwrap_or(false);
            if !explained {
                unexplained += 1;
            }
        }
        assert!(
            unexplained <= m.unacked_starts,
            "round {round}: {unexplained} wild bitmap bits not explained by the \
             {} un-acked ledger starts — allocator/reconciliation bug or corruption",
            m.unacked_starts
        );

        // Invariant 1 (D0): acked ops present unless superseded.
        let expectations = acked_expectations(&m);
        eprintln!(
            "[kill9 round {round}] ledger: {} lines, {} started creates, {} started destroys, \
             {} acked inos, {} un-acked starts; checks: {} valid slots, {} dentries, \
             {healed} healed bits, {unexplained} unexplained, {} D0 expectations",
            m.lines.len(),
            m.started_creates,
            m.started_destroys,
            m.acked_inos.len(),
            m.unacked_starts,
            valid_inos.len(),
            dentry_names.len(),
            expectations.len(),
        );
        let backend = MetaLvBackend::new(storage);
        for (name, ino, expect) in expectations {
            match expect {
                Expect::Present(xattr) => {
                    let found = backend.lookup(1, &name).await.unwrap_or_else(|e| {
                        panic!("round {round}: acked create '{name}' lost after kill-9: {e}")
                    });
                    assert_eq!(found.ino, ino, "round {round}: '{name}' resolved wrong ino");
                    let got = read_inode(&backend.storage, ino).await.unwrap_or_else(|e| {
                        panic!("round {round}: acked ino {ino} unreadable: {e}")
                    });
                    assert_eq!(got.ino, ino);
                    if let Some(val) = xattr {
                        let stored = squeezefs::meta_backend::xattr::get_xattr(
                            &backend.storage,
                            ino,
                            "user.crash",
                        )
                        .await
                        .expect("xattr read")
                        .unwrap_or_else(|| panic!("round {round}: acked xattr on ino {ino} lost"));
                        assert_eq!(
                            stored,
                            val.as_bytes(),
                            "round {round}: acked xattr value mismatch on ino {ino}"
                        );
                    }
                }
                Expect::Absent => {
                    assert!(
                        backend.lookup(1, &name).await.is_err(),
                        "round {round}: acked unlink '{name}' resurrected after kill-9"
                    );
                }
                Expect::Unknown => {}
            }
        }
    }
}

// ===========================================================================
// PR K6b — the dual-format half of the soak (design Rollout 4: "the kill-9
// soak runs against BOTH formats from PR K6b on"), the §4.4 pt 4
// rollback-race case, and the `disabled_volumes` fail-stop escalation.
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

/// Child branch for the v3 rounds: the same churn protocol as the v2
/// child (start line → op → `sync_device` barrier → ack line), through
/// the `KvMetaBackend` `Metadata` surface.
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

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ledger.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(
            ledger.exists(),
            "round {round}: v3 child never started churning"
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
            "round {round}: the v3 child never acked a single op — commit pipeline dead"
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
    let routed = std::sync::Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V3(
        be.clone(),
    )]));

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

        // Exactly one op takes the armed offset (the checkpoint cadence
        // is parked, so no other reservation can absorb it): the first
        // reservation's entry covers the head byte and fails; the
        // second's range starts past it and commits.
        let failed: Vec<&str> = [("a", &ra), ("b", &rb)]
            .iter()
            .filter(|(_, r)| r.is_err())
            .map(|(n, _)| *n)
            .collect();
        assert_eq!(
            failed.len(),
            1,
            "round {round}: exactly one racing create must take the armed ring-head fault \
             (got a={ra:?} b={rb:?})"
        );
        failures_seen += failed.len() as u32;

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

        // RAM == replay (the §4.4 pt 4 theorem): a fresh mount of the
        // same bytes folds to exactly the live in-RAM state — the failed
        // writer's hole is dropped, the concurrent committed Δtime on the
        // shared parent-key survives the rollback.
        let d_live = squeezefs::meta_backend::kv::builder::digest_walk(&be.trees())
            .await
            .unwrap();
        let replayed = KvMetaBackend::open(&vol).await.unwrap();
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
    let routed = std::sync::Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V3(
        be.clone(),
    )]));

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
    assert!(failures >= 3, "poisoned-path creates must fail");
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
