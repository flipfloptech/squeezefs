//! Share-ledger law tests (`docs/design-nvmeof-target-management.md` §6.4,
//! landed by PR 1/N1). Every §6.4 law is pinned here: load-never-writes
//! (named for the registry truncation bug), atomic-replace + its crash
//! shape, flock serialization, forward-version refusal, strict-unknown-
//! field v1 parsing, the field-presence rules, and the write-ahead intent
//! state machine (law 6). Pure file-backed sandboxes — no root, no mocks,
//! no env vars (the `Ledger::new(path)` injection seam).

use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::sync::mpsc;
use std::time::Duration;

use proptest::prelude::*;

use squeezefs::nvmeof::ledger::{
    retire_old_registry, utc_now_rfc3339, Ledger, LEDGER_FILE, LEDGER_LOCK_FILE, LEDGER_TMP_FILE,
};
use squeezefs::nvmeof::stack::{AdoptClass, AdoptedFrom, Listener, ShareRecord, ShareState};
use squeezefs::nvmeof::StackKind;

fn rec(subnqn: &str, stack: StackKind, backing: &str) -> ShareRecord {
    ShareRecord {
        subnqn: subnqn.to_string(),
        stack,
        state: ShareState::Pending,
        backing_path: backing.to_string(),
        backing_canonical: backing.to_string(),
        nsid: None,
        ns_uuid: None,
        listeners: vec![Listener {
            ip: "127.0.0.1".to_string(),
            port: 4420,
            nvmet_port_id: None,
        }],
        bdev_name: None,
        ptpl_file: None,
        loop_device: None,
        created_utc: "2026-07-17T00:00:00Z".to_string(),
        allow_hosts: Vec::new(),
        adopted_from: None,
    }
}

fn state_of(ledger: &Ledger, subnqn: &str) -> Option<(ShareState, Option<String>)> {
    ledger
        .load()
        .expect("ledger load must succeed")
        .into_iter()
        .find(|r| r.subnqn == subnqn)
        .map(|r| (r.state, r.loop_device))
}

/// Law 1 — the regression test named in the design for the bug that made
/// the old registry never-readable-back as root: `get_shares_config_path`
/// truncated the file with `fs::write(path, "[]")` on every resolution,
/// i.e. every load destroyed the data before reading it. The ledger's
/// `load()` must be a pure read: byte-identical file across 1,000 loads,
/// success on a read-only filesystem, zero files created by loading.
#[test]
fn test_ledger_load_never_writes_truncation_bug_regression() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_dir = dir.path().join("nvmeof");

    // (a) Loading a never-written ledger neither fails nor conjures state.
    let ledger = Ledger::new(&state_dir);
    assert_eq!(
        ledger.load().expect("empty load must succeed"),
        vec![],
        "missing ledger file must read as the empty ledger"
    );
    assert!(
        !state_dir.exists(),
        "load() must not create the state dir (law 1: load never writes)"
    );

    // Seed one record through the real mutation path, then strip the
    // mutation artifacts so any file re-created below is load()'s fault.
    ledger
        .begin_share(&rec("nqn.test:lnw", StackKind::Nvmet, "/dev/lnw0"))
        .expect("begin_share");
    ledger.finalize_share("nqn.test:lnw").expect("finalize");
    let lock_path = state_dir.join(LEDGER_LOCK_FILE);
    let tmp_path = state_dir.join(LEDGER_TMP_FILE);
    let _ = fs::remove_file(&lock_path);
    let _ = fs::remove_file(&tmp_path);

    let ledger_path = state_dir.join(LEDGER_FILE);
    let before = fs::read(&ledger_path).expect("ledger bytes");

    // (b) 1,000 loads leave the file byte-identical and create nothing.
    for i in 0..1_000 {
        let records = ledger.load().expect("load must succeed");
        assert_eq!(records.len(), 1, "load #{i} must see the record");
        assert_eq!(records[0].subnqn, "nqn.test:lnw");
    }
    let after = fs::read(&ledger_path).expect("ledger bytes after loads");
    assert_eq!(
        before, after,
        "1,000 loads must leave the ledger byte-identical (the old registry \
         truncated itself to [] on every load)"
    );
    assert!(
        !lock_path.exists(),
        "load() must not create the lock file (law 1)"
    );
    assert!(
        !tmp_path.exists(),
        "load() must not create the tmp file (law 1)"
    );

    // (c) Load succeeds on a read-only filesystem shape (dir + file r/o).
    fs::set_permissions(&ledger_path, fs::Permissions::from_mode(0o444)).expect("chmod file");
    fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o555)).expect("chmod dir");
    let res = ledger.load();
    // Restore perms before asserting so a failure still cleans up.
    fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o755)).expect("restore dir");
    fs::set_permissions(&ledger_path, fs::Permissions::from_mode(0o644)).expect("restore file");
    let records = res.expect("load() on a read-only filesystem must succeed (law 1)");
    assert_eq!(records.len(), 1);
}

/// Law 2 crash shape — a leftover `shares.json.tmp` (crash between write
/// and rename) is ignored and reported by `load()`, never consumed and
/// never deleted (deletion would be a write); the next mutation replaces
/// it and the ledger stays consistent.
#[test]
fn test_ledger_atomic_replace_crash_shape_tmp_ignored() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_dir = dir.path().join("nvmeof");
    let ledger = Ledger::new(&state_dir);

    ledger
        .begin_share(&rec("nqn.test:tmp", StackKind::Nvmet, "/dev/tmp0"))
        .expect("begin_share");

    // Simulate the crash window: a half-written tmp beside the real file.
    let tmp_path = state_dir.join(LEDGER_TMP_FILE);
    fs::write(&tmp_path, b"{ definitely not valid json").expect("plant tmp");

    let records = ledger
        .load()
        .expect("load must ignore a leftover tmp file (crash shape) and succeed");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].subnqn, "nqn.test:tmp");
    assert!(
        tmp_path.exists(),
        "load() must not delete the tmp file (load never writes)"
    );
    assert_eq!(
        fs::read(&tmp_path).expect("tmp bytes"),
        b"{ definitely not valid json".to_vec(),
        "load() must not touch the tmp file's contents"
    );

    // The next mutation runs the full atomic-replace and clears the shape.
    ledger.finalize_share("nqn.test:tmp").expect("finalize");
    assert!(
        !tmp_path.exists(),
        "a completed mutation must leave no tmp file behind"
    );
    assert_eq!(
        state_of(&ledger, "nqn.test:tmp"),
        Some((ShareState::Active, None))
    );

    // Crash-before-first-rename shape: only lock + tmp exist, no ledger.
    let dir2 = tempfile::tempdir().expect("tempdir2");
    let state_dir2 = dir2.path().join("nvmeof");
    fs::create_dir_all(&state_dir2).expect("mkdir");
    fs::write(state_dir2.join(LEDGER_TMP_FILE), b"garbage").expect("tmp only");
    fs::write(state_dir2.join(LEDGER_LOCK_FILE), b"").expect("lock only");
    let ledger2 = Ledger::new(&state_dir2);
    assert_eq!(
        ledger2
            .load()
            .expect("tmp-only dir must read as the empty ledger"),
        vec![]
    );
}

/// Law 3 — forward-only: a `"format" > 1` ledger refuses loud with the
/// upgrade message, even when it carries fields v1 has never heard of
/// (the version check must come before strict field parsing).
#[test]
fn test_ledger_forward_version_refused_loud() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_dir = dir.path().join("nvmeof");
    fs::create_dir_all(&state_dir).expect("mkdir");
    fs::write(
        state_dir.join(LEDGER_FILE),
        br#"{ "format": 2, "shares": [], "quantum_links": true }"#,
    )
    .expect("write v2 ledger");

    let err = Ledger::new(&state_dir)
        .load()
        .expect_err("format 2 must refuse");
    let msg = err.to_string();
    assert!(
        msg.contains("newer"),
        "refusal must say the file was created by a newer squeezefs: {msg}"
    );
    assert!(
        msg.contains("upgrade"),
        "refusal must name the remediation (upgrade): {msg}"
    );

    // Mutations against a future-format ledger must refuse too (a write
    // would destroy the newer binary's records).
    let err = Ledger::new(&state_dir)
        .begin_share(&rec("nqn.test:v2", StackKind::Nvmet, "/dev/v2"))
        .expect_err("mutating a format-2 ledger must refuse");
    assert!(err.to_string().contains("newer"), "loud refusal: {err}");

    // And the file must be untouched by all of the above.
    let bytes = fs::read(state_dir.join(LEDGER_FILE)).expect("read back");
    assert_eq!(
        bytes,
        br#"{ "format": 2, "shares": [], "quantum_links": true }"#.to_vec(),
        "refusing must never modify the newer file"
    );
}

/// Law 3 — unknown fields *within* v1 are an error, not ignored (no
/// silent partial reads), at both the top level and the record level;
/// absent optional fields are legal.
#[test]
fn test_ledger_unknown_field_within_v1_is_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_dir = dir.path().join("nvmeof");
    fs::create_dir_all(&state_dir).expect("mkdir");
    let path = state_dir.join(LEDGER_FILE);

    // Top-level unknown field.
    fs::write(&path, br#"{ "format": 1, "shares": [], "bogus": 1 }"#).expect("write");
    assert!(
        Ledger::new(&state_dir).load().is_err(),
        "unknown top-level field within v1 must be an error"
    );

    // Record-level unknown field.
    fs::write(
        &path,
        br#"{ "format": 1, "shares": [ {
            "subnqn": "nqn.test:x", "stack": "nvmet", "state": "active",
            "backing_path": "/dev/x", "backing_canonical": "/dev/x",
            "listeners": [ { "ip": "10.0.0.1", "port": 4420 } ],
            "created_utc": "2026-07-17T00:00:00Z",
            "surprise_field": "hello"
        } ] }"#,
    )
    .expect("write");
    assert!(
        Ledger::new(&state_dir).load().is_err(),
        "unknown record field within v1 must be an error"
    );

    // Same record without the surprise (and with every optional field
    // absent) is legal — absent optionals are within the presence rules.
    fs::write(
        &path,
        br#"{ "format": 1, "shares": [ {
            "subnqn": "nqn.test:x", "stack": "nvmet", "state": "active",
            "backing_path": "/dev/x", "backing_canonical": "/dev/x",
            "listeners": [ { "ip": "10.0.0.1", "port": 4420 } ],
            "created_utc": "2026-07-17T00:00:00Z"
        } ] }"#,
    )
    .expect("write");
    let records = Ledger::new(&state_dir)
        .load()
        .expect("absent optional fields are legal");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].nsid, None);
    assert_eq!(records[0].ns_uuid, None);
    assert_eq!(records[0].bdev_name, None);
    assert_eq!(records[0].ptpl_file, None);
    assert_eq!(records[0].loop_device, None);
    assert_eq!(records[0].adopted_from, None);
    assert_eq!(records[0].listeners[0].nvmet_port_id, None);
}

/// §6.4 field-presence rules: required fields must be present, listeners
/// must carry ≥ 1 entry with ip+port each, and the full optional surface
/// (incl. `adopted_from`, parsed at N1 for ≥ N1 rollback readability)
/// round-trips losslessly.
#[test]
fn test_ledger_field_presence_rules() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_dir = dir.path().join("nvmeof");
    fs::create_dir_all(&state_dir).expect("mkdir");
    let path = state_dir.join(LEDGER_FILE);

    // Missing required field (no listeners key).
    fs::write(
        &path,
        br#"{ "format": 1, "shares": [ {
            "subnqn": "nqn.test:x", "stack": "nvmet", "state": "active",
            "backing_path": "/dev/x", "backing_canonical": "/dev/x",
            "created_utc": "2026-07-17T00:00:00Z"
        } ] }"#,
    )
    .expect("write");
    assert!(
        Ledger::new(&state_dir).load().is_err(),
        "a record without listeners must refuse"
    );

    // Empty listeners array violates the >= 1 law.
    fs::write(
        &path,
        br#"{ "format": 1, "shares": [ {
            "subnqn": "nqn.test:x", "stack": "nvmet", "state": "active",
            "backing_path": "/dev/x", "backing_canonical": "/dev/x",
            "listeners": [],
            "created_utc": "2026-07-17T00:00:00Z"
        } ] }"#,
    )
    .expect("write");
    assert!(
        Ledger::new(&state_dir).load().is_err(),
        "an empty listeners array must refuse (>= 1 entry required)"
    );

    // Listener missing its required port.
    fs::write(
        &path,
        br#"{ "format": 1, "shares": [ {
            "subnqn": "nqn.test:x", "stack": "nvmet", "state": "active",
            "backing_path": "/dev/x", "backing_canonical": "/dev/x",
            "listeners": [ { "ip": "10.0.0.1" } ],
            "created_utc": "2026-07-17T00:00:00Z"
        } ] }"#,
    )
    .expect("write");
    assert!(
        Ledger::new(&state_dir).load().is_err(),
        "a listener without a port must refuse"
    );

    // Full-optional record round-trips losslessly (schema example shape,
    // §6.4 — incl. the nvmet-only fields, the fields only the RETIRED
    // SPDK stack ever wrote (a legacy record must stay decodable so
    // `list` can name it and `unshare` can remove it — R-SYM-8), and
    // adopt provenance, which N1 must parse even though N1 never writes
    // it).
    let full = ShareRecord {
        subnqn: "nqn.2026-07.io.squeezefs:share-full".to_string(),
        stack: StackKind::Spdk,
        state: ShareState::Active,
        backing_path: "/dev/zram3".to_string(),
        backing_canonical: "/dev/zram3".to_string(),
        nsid: Some(1),
        ns_uuid: Some("e2b1c9a4-52d1-4a08-9f31-7c2b8d1e0aa1".to_string()),
        listeners: vec![
            Listener {
                ip: "10.10.10.50".to_string(),
                port: 4420,
                nvmet_port_id: None,
            },
            Listener {
                ip: "10.10.10.51".to_string(),
                port: 4421,
                nvmet_port_id: Some(53017),
            },
        ],
        bdev_name: Some("sqz_aio_6f0c1b2e".to_string()),
        ptpl_file: Some("spdk/ptpl/e2b1c9a4-52d1-4a08-9f31-7c2b8d1e0aa1.json".to_string()),
        loop_device: Some("/dev/loop7".to_string()),
        created_utc: "2026-07-17T14:02:11Z".to_string(),
        allow_hosts: vec!["nqn.2014-08.org.nvmexpress:uuid:allowed-1".to_string()],
        adopted_from: Some(AdoptedFrom {
            utc: "2026-07-18T09:00:00Z".to_string(),
            class: AdoptClass::LedgerLoss,
        }),
    };
    let file_json = serde_json::json!({
        "format": 1,
        "shares": [serde_json::to_value(&full).expect("serialize record")],
    });
    fs::write(&path, serde_json::to_vec_pretty(&file_json).expect("json")).expect("write");
    let records = Ledger::new(&state_dir)
        .load()
        .expect("full-optional record must load");
    assert_eq!(records, vec![full], "lossless round-trip of schema v1");

    // Serialization presence: optionals serialize as explicit null (the
    // schema example shape) except adopted_from, which is absent when
    // None ("absent on shares created by share").
    let minimal = rec("nqn.test:nulls", StackKind::Nvmet, "/dev/nulls");
    let val = serde_json::to_value(&minimal).expect("serialize");
    let obj = val.as_object().expect("object");
    for key in ["nsid", "ns_uuid", "bdev_name", "ptpl_file", "loop_device"] {
        assert!(
            obj.get(key).is_some_and(|v| v.is_null()),
            "{key} must serialize as explicit null on records that lack it"
        );
    }
    assert!(
        !obj.contains_key("adopted_from"),
        "adopted_from must be absent (not null) on shares created by share"
    );
    assert!(
        !obj.contains_key("allow_hosts"),
        "allow_hosts must be absent (not []) on allow-any shares — records \
         without an allowlist stay byte-compatible with N1 readers"
    );
}

/// Law 6 — the write-ahead intent state machine:
/// `begin(pending) → finalize(active)` / `mark_removing → delete`, with
/// the duplicate guards (subnqn and cross-stack backing) refusing loud at
/// `begin`, NotFound on missing records, and idempotent re-mark of an
/// already-`removing` record (resumed teardown).
#[test]
fn test_ledger_intent_state_machine_transitions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = Ledger::new(dir.path().join("nvmeof"));

    // begin → pending (and find() sees intent records).
    ledger
        .begin_share(&rec("nqn.test:a", StackKind::Nvmet, "/dev/a"))
        .expect("begin a");
    assert_eq!(
        state_of(&ledger, "nqn.test:a"),
        Some((ShareState::Pending, None))
    );
    assert_eq!(
        ledger
            .find("nqn.test:a")
            .expect("find")
            .expect("record present")
            .state,
        ShareState::Pending,
        "find() must surface pending intent records — a crash-window share is still ours"
    );

    // begin must require a pending record (the caller cannot skip the
    // intent protocol by appending an active record).
    let mut active = rec("nqn.test:skip", StackKind::Nvmet, "/dev/skip");
    active.state = ShareState::Active;
    let err = ledger
        .begin_share(&active)
        .expect_err("begin_share of a non-pending record must refuse");
    assert_eq!(
        err.kind(),
        ErrorKind::InvalidInput,
        "wrong-state begin: {err}"
    );

    // Duplicate subnqn refuses loud.
    let err = ledger
        .begin_share(&rec("nqn.test:a", StackKind::Nvmet, "/dev/other"))
        .expect_err("duplicate subnqn must refuse");
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    assert!(
        err.to_string().contains("nqn.test:a"),
        "refusal must name the holder: {err}"
    );

    // Duplicate backing refuses loud ACROSS stacks (the cross-stack
    // duplicate-backing guard is a ledger job, §6.4).
    let err = ledger
        .begin_share(&rec("nqn.test:b", StackKind::Spdk, "/dev/a"))
        .expect_err("cross-stack duplicate backing must refuse");
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    let msg = err.to_string();
    assert!(
        msg.contains("nqn.test:a") && msg.contains("nvmet"),
        "the refusal message is the runbook — it must name the live holder \
         (NQN + stack): {msg}"
    );

    // set_loop_device is ledger bookkeeping (law 5) and works mid-flight.
    ledger
        .set_loop_device("nqn.test:a", Some("/dev/loop3".to_string()))
        .expect("set loop");
    assert_eq!(
        state_of(&ledger, "nqn.test:a"),
        Some((ShareState::Pending, Some("/dev/loop3".to_string())))
    );

    // finalize: pending → active, exactly once.
    ledger.finalize_share("nqn.test:a").expect("finalize a");
    assert_eq!(
        state_of(&ledger, "nqn.test:a"),
        Some((ShareState::Active, Some("/dev/loop3".to_string())))
    );
    let err = ledger
        .finalize_share("nqn.test:a")
        .expect_err("finalize of an active record must refuse");
    assert_eq!(err.kind(), ErrorKind::InvalidInput);

    // mark_removing: active → removing, idempotent on removing.
    ledger.mark_removing("nqn.test:a").expect("mark removing");
    assert_eq!(
        state_of(&ledger, "nqn.test:a").map(|(s, _)| s),
        Some(ShareState::Removing)
    );
    ledger
        .mark_removing("nqn.test:a")
        .expect("re-mark of a removing record must be idempotent (resumed teardown)");

    // delete completes the teardown; the record is gone.
    ledger.delete("nqn.test:a").expect("delete a");
    assert_eq!(state_of(&ledger, "nqn.test:a"), None);

    // After delete, the backing is shareable again.
    ledger
        .begin_share(&rec("nqn.test:b", StackKind::Spdk, "/dev/a"))
        .expect("backing freed by delete must be shareable again");

    // A pending record can go straight to removing (unshare accepts
    // crash-window intents — §6.4 law 6 / §6.2 unshare row).
    ledger
        .begin_share(&rec("nqn.test:c", StackKind::Nvmet, "/dev/c"))
        .expect("begin c");
    ledger
        .mark_removing("nqn.test:c")
        .expect("pending → removing must be legal");

    // Missing records are NotFound everywhere.
    for (what, err) in [
        (
            "finalize",
            ledger.finalize_share("nqn.test:ghost").unwrap_err(),
        ),
        (
            "mark_removing",
            ledger.mark_removing("nqn.test:ghost").unwrap_err(),
        ),
        ("delete", ledger.delete("nqn.test:ghost").unwrap_err()),
        (
            "set_loop_device",
            ledger.set_loop_device("nqn.test:ghost", None).unwrap_err(),
        ),
    ] {
        assert_eq!(
            err.kind(),
            ErrorKind::NotFound,
            "{what} on a missing record must be NotFound: {err}"
        );
    }
    assert!(ledger.find("nqn.test:ghost").expect("find").is_none());
}

/// Law 2 — mutations serialize under `flock`: (a) a held lock blocks a
/// mutator (observed via channel non-arrival, released deterministically),
/// and (b) concurrent mutators from many threads lose no updates.
#[test]
fn test_ledger_flock_serializes_concurrent_mutators() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_dir = dir.path().join("nvmeof");
    let ledger = Ledger::new(&state_dir);
    ledger
        .begin_share(&rec("nqn.test:seed", StackKind::Nvmet, "/dev/seed"))
        .expect("seed record (creates the state dir + lock file)");

    // (a) Hold the ledger lock; a begin_share must block until release.
    let lock_path = state_dir.join(LEDGER_LOCK_FILE);
    let lock_file = fs::OpenOptions::new()
        .write(true)
        .open(&lock_path)
        .expect("open lock file");
    use std::os::unix::io::AsRawFd;
    assert_eq!(
        unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) },
        0,
        "test flock"
    );

    let (tx, rx) = mpsc::channel();
    let state_dir_clone = state_dir.clone();
    let worker = std::thread::spawn(move || {
        let ledger = Ledger::new(&state_dir_clone);
        tx.send("starting").expect("send start");
        ledger
            .begin_share(&rec("nqn.test:blocked", StackKind::Nvmet, "/dev/blocked"))
            .expect("begin after lock release");
        tx.send("done").expect("send done");
    });
    assert_eq!(rx.recv().expect("start signal"), "starting");
    assert!(
        rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "a mutation must BLOCK while the ledger flock is held by another holder"
    );
    assert_eq!(
        unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_UN) },
        0,
        "unlock"
    );
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(10))
            .expect("mutation must complete once the lock is released"),
        "done"
    );
    worker.join().expect("worker join");

    // (b) 8 threads × 8 begins each — read-modify-write under flock must
    // lose nothing and the final file must parse clean.
    let mut handles = Vec::new();
    for t in 0..8 {
        let state_dir = state_dir.clone();
        handles.push(std::thread::spawn(move || {
            let ledger = Ledger::new(&state_dir);
            for i in 0..8 {
                ledger
                    .begin_share(&rec(
                        &format!("nqn.test:t{t}i{i}"),
                        StackKind::Nvmet,
                        &format!("/dev/t{t}i{i}"),
                    ))
                    .expect("concurrent begin_share");
            }
        }));
    }
    for h in handles {
        h.join().expect("thread join");
    }
    let records = ledger.load().expect("final load");
    assert_eq!(
        records.len(),
        2 + 64,
        "no lost updates under concurrent mutation (flock serialization)"
    );
}

/// §6.4 old-registry migration: a pre-existing pre-rebuild registry file
/// is renamed to `.retired-by-rebuild` (bytes preserved — they were never
/// readable back, but they are not destroyed either), exactly once,
/// no-op when absent.
#[test]
fn test_retire_old_registry_renames_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let old = dir.path().join("nvmeof_shares.json");
    let retired = dir.path().join("nvmeof_shares.json.retired-by-rebuild");

    // Absent: no-op, conjures nothing.
    retire_old_registry(&old);
    assert!(!old.exists() && !retired.exists());

    // Present: renamed with bytes preserved.
    fs::write(&old, b"[{\"subnqn\":\"nqn.old\"}]").expect("write old registry");
    retire_old_registry(&old);
    assert!(!old.exists(), "old registry must be renamed away");
    assert_eq!(
        fs::read(&retired).expect("retired bytes"),
        b"[{\"subnqn\":\"nqn.old\"}]".to_vec(),
        "whatever bytes the old registry held are preserved by the rename"
    );

    // Second call: idempotent no-op that does not clobber the archive.
    retire_old_registry(&old);
    assert_eq!(
        fs::read(&retired).expect("retired bytes"),
        b"[{\"subnqn\":\"nqn.old\"}]".to_vec()
    );
}

/// `created_utc` stamps are RFC 3339 UTC ("2026-07-17T14:02:11Z" shape).
#[test]
fn test_created_utc_stamp_shape() {
    let stamp = utc_now_rfc3339();
    let b = stamp.as_bytes();
    assert_eq!(b.len(), 20, "RFC3339 seconds-precision Zulu: {stamp}");
    assert_eq!(b[4], b'-', "{stamp}");
    assert_eq!(b[7], b'-', "{stamp}");
    assert_eq!(b[10], b'T', "{stamp}");
    assert_eq!(b[13], b':', "{stamp}");
    assert_eq!(b[16], b':', "{stamp}");
    assert_eq!(b[19], b'Z', "{stamp}");
    let year: u32 = stamp[0..4].parse().expect("year");
    assert!((2026..2100).contains(&year), "sane year: {stamp}");
}

// ---------------------------------------------------------------------------
// Property tests (ledger laws under arbitrary inputs)
// ---------------------------------------------------------------------------

fn arb_record(idx: usize) -> impl Strategy<Value = ShareRecord> {
    (
        prop_oneof![Just(StackKind::Spdk), Just(StackKind::Nvmet)],
        prop_oneof![
            Just(ShareState::Pending),
            Just(ShareState::Active),
            Just(ShareState::Removing)
        ],
        proptest::option::of(0u32..16u32),
        proptest::option::of("[a-f0-9]{8}"),
        proptest::collection::vec(
            (
                "[0-9.]{7,15}",
                1u16..65535,
                proptest::option::of(52000u32..54000),
            ),
            1..4,
        ),
        proptest::option::of("bdev_[a-z0-9]{6}"),
        proptest::option::of("spdk/ptpl/[a-f0-9]{8}\\.json"),
        proptest::option::of("/dev/loop[0-9]{1,2}"),
        proptest::collection::vec("nqn\\.host:[a-z0-9]{4}", 0..3),
        proptest::option::of((
            Just("2026-07-18T00:00:00Z".to_string()),
            prop_oneof![
                Just(AdoptClass::PreRebuild),
                Just(AdoptClass::Foreign),
                Just(AdoptClass::LedgerLoss)
            ],
        )),
    )
        .prop_map(
            move |(
                stack,
                state,
                nsid,
                ns_uuid,
                listeners,
                bdev_name,
                ptpl_file,
                loop_device,
                allow_hosts,
                adopted,
            )| {
                ShareRecord {
                    subnqn: format!("nqn.test:prop-{idx}"),
                    stack,
                    state,
                    backing_path: format!("/dev/prop{idx}"),
                    backing_canonical: format!("/dev/prop{idx}"),
                    nsid,
                    ns_uuid,
                    listeners: listeners
                        .into_iter()
                        .map(|(ip, port, nvmet_port_id)| Listener {
                            ip,
                            port,
                            nvmet_port_id,
                        })
                        .collect(),
                    bdev_name,
                    ptpl_file,
                    loop_device,
                    created_utc: "2026-07-17T00:00:00Z".to_string(),
                    allow_hosts,
                    adopted_from: adopted.map(|(utc, class)| AdoptedFrom { utc, class }),
                }
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Any schema-v1 record set round-trips losslessly through the file.
    #[test]
    fn prop_ledger_record_roundtrip(records in proptest::collection::vec(any::<u8>(), 1..5)
        .prop_flat_map(|seeds| {
            let strategies: Vec<_> = seeds.iter().enumerate().map(|(i, _)| arb_record(i)).collect();
            strategies
        })
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_dir = dir.path().join("nvmeof");
        fs::create_dir_all(&state_dir).expect("mkdir");
        let file_json = serde_json::json!({
            "format": 1,
            "shares": serde_json::to_value(&records).expect("serialize"),
        });
        fs::write(
            state_dir.join(LEDGER_FILE),
            serde_json::to_vec_pretty(&file_json).expect("json"),
        )
        .expect("write");
        let loaded = Ledger::new(&state_dir).load().expect("load");
        prop_assert_eq!(loaded, records);
    }

    /// The intent state machine agrees with a reference model under
    /// arbitrary op sequences (begin/finalize/mark_removing/delete/
    /// set_loop_device over a small id pool with colliding backings).
    #[test]
    fn prop_ledger_intent_ops_match_model(ops in proptest::collection::vec((0u8..5, 0u8..4, proptest::option::of(0u8..3)), 1..40)) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = Ledger::new(dir.path().join("nvmeof"));
        // id → (state, loop); backing pool smaller than id pool so distinct
        // ids collide on backing (exercises the cross-backing guard).
        let mut model: HashMap<u8, (ShareState, Option<String>)> = HashMap::new();
        let backing_of = |id: u8| format!("/dev/model{}", id % 2);
        let nqn_of = |id: u8| format!("nqn.test:model-{id}");

        for (op, id, loop_sel) in ops {
            match op {
                0 => {
                    let backing_taken = model
                        .keys()
                        .any(|other| backing_of(*other) == backing_of(id));
                    let res = ledger.begin_share(&rec(&nqn_of(id), StackKind::Nvmet, &backing_of(id)));
                    if model.contains_key(&id) || backing_taken {
                        prop_assert!(res.is_err(), "begin over duplicate must refuse");
                        prop_assert_eq!(res.unwrap_err().kind(), ErrorKind::AlreadyExists);
                    } else {
                        prop_assert!(res.is_ok(), "begin: {:?}", res.err());
                        model.insert(id, (ShareState::Pending, None));
                    }
                }
                1 => {
                    let res = ledger.finalize_share(&nqn_of(id));
                    match model.get_mut(&id) {
                        Some(entry) if entry.0 == ShareState::Pending => {
                            prop_assert!(res.is_ok(), "finalize: {:?}", res.err());
                            entry.0 = ShareState::Active;
                        }
                        Some(_) => {
                            prop_assert_eq!(res.unwrap_err().kind(), ErrorKind::InvalidInput);
                        }
                        None => prop_assert_eq!(res.unwrap_err().kind(), ErrorKind::NotFound),
                    }
                }
                2 => {
                    let res = ledger.mark_removing(&nqn_of(id));
                    match model.get_mut(&id) {
                        Some(entry) => {
                            prop_assert!(res.is_ok(), "mark_removing: {:?}", res.err());
                            entry.0 = ShareState::Removing;
                        }
                        None => prop_assert_eq!(res.unwrap_err().kind(), ErrorKind::NotFound),
                    }
                }
                3 => {
                    let res = ledger.delete(&nqn_of(id));
                    if model.remove(&id).is_some() {
                        prop_assert!(res.is_ok(), "delete: {:?}", res.err());
                    } else {
                        prop_assert_eq!(res.unwrap_err().kind(), ErrorKind::NotFound);
                    }
                }
                _ => {
                    let loop_dev = loop_sel.map(|l| format!("/dev/loop{l}"));
                    let res = ledger.set_loop_device(&nqn_of(id), loop_dev.clone());
                    match model.get_mut(&id) {
                        Some(entry) => {
                            prop_assert!(res.is_ok(), "set_loop_device: {:?}", res.err());
                            entry.1 = loop_dev;
                        }
                        None => prop_assert_eq!(res.unwrap_err().kind(), ErrorKind::NotFound),
                    }
                }
            }
        }

        let final_records = ledger.load().expect("final load");
        prop_assert_eq!(final_records.len(), model.len());
        for record in final_records {
            let id: u8 = record.subnqn.rsplit('-').next().expect("id").parse().expect("id parse");
            let (state, loop_dev) = model.get(&id).expect("record must be in model");
            prop_assert_eq!(&record.state, state);
            prop_assert_eq!(&record.loop_device, loop_dev);
        }
    }
}
