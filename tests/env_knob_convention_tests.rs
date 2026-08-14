//! The env-knob convention's contract (ENG-10, pre-RC spec §10).
//!
//! Three incompatible conventions used to coexist: 56 silent-default sites
//! (a typo'd value was indistinguishable from an unset knob), 5 that
//! `panic!`ed and killed the mount, 3 that returned a loud error — plus 19
//! presence-based booleans where `SQUEEZEFS_FREE_FORENSICS=0` **enabled**
//! the feature, and a prefix collision (`SQUEEZEFS_RECLAIM_BATCH`, inode
//! reclaim, was a strict prefix of `SQUEEZEFS_RECLAIM_BATCH_BLOCKS`, block
//! reclaim). ~20 knobs were documented nowhere.
//!
//! Pinned here:
//!
//! * **Census completeness** — every `SQUEEZEFS_*` / `SQZ_*` literal in the
//!   tree is registered. This is what stops a new knob from being born
//!   undocumented: the registry is the documentation source
//!   (`docs/operations.md` §Environment knobs points at it).
//! * **The collision law** — a value-bearing knob may never be a strict
//!   prefix of another knob; only a family's boolean switch may be.
//! * **Loud refusal** — malformed values, out-of-range values and retired
//!   spellings produce refusals naming the knob AND the offending value;
//!   unknown names are announced, never refused.
//! * **The boolean law** — `=0` disables, everywhere, including the knobs
//!   whose default is ON.
//! * **Defaults preserved** — the rename and the convention change did not
//!   move a single knob's default.

use squeezefs::env_knobs::{self, Kind};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Source roots whose knob literals must all be registered: the product
/// crates plus the test/bench surface (harness variables are registered as
/// `Kind::Harness`, which is how they stay visible without being validated
/// as product knobs).
fn source_roots() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    vec![
        root.join("src"),
        root.join("crates/squeezefs-ipc/src"),
        root.join("crates/squeezefs-preload/src"),
        root.join("crates/squeezefs-testkit/src"),
        root.join("crates/fuse3/src"),
        root.join("tests"),
        root.join("benches"),
    ]
}

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

/// Literals that look like knob names but are not: the namespace prefixes
/// themselves (`env_knobs::is_ours`) and the deliberate fakes these very
/// tests and the core's unit tests use to exercise refusals. Everything
/// else must be registered.
const CENSUS_EXEMPT: &[&str] = &[
    "SQUEEZEFS_",
    "SQZ_",
    "SQUEEZEFS_TYPO_KNOB",
    "SQUEEZEFS_NOT_A_KNOB",
    "SQUEEZEFS_X",
    "SQUEEZEFS_N",
    "SQUEEZEFS_D",
    "SQUEEZEFS_E",
];

/// Every `"SQUEEZEFS_…"` / `"SQZ_…"` string literal in the tree.
fn knob_literals_in_tree() -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut files = Vec::new();
    for root in source_roots() {
        rust_files(&root, &mut files);
    }
    assert!(
        files.len() > 100,
        "the source census found only {} files — the roots are wrong",
        files.len()
    );
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        for (idx, _) in text.match_indices('"') {
            let rest = &text[idx + 1..];
            let Some(end) = rest.find('"') else { continue };
            let lit = &rest[..end];
            if (lit.starts_with("SQUEEZEFS_") || lit.starts_with("SQZ_"))
                && !CENSUS_EXEMPT.contains(&lit)
                && lit
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            {
                found.insert(lit.to_string());
            }
        }
    }
    found
}

/// The census law: a knob that exists in the code exists in the registry.
/// A new knob therefore cannot ship undocumented — the exact defect ENG-10
/// found for ~20 of them.
#[test]
fn every_knob_in_the_tree_is_registered() {
    let literals = knob_literals_in_tree();
    let missing: Vec<&String> = literals
        .iter()
        .filter(|l| env_knobs::lookup(l).is_none())
        .collect();
    assert!(
        missing.is_empty(),
        "unregistered env knob(s) — add them to src/env_knobs.rs (the registry \
         IS the documentation): {missing:?}"
    );
}

/// And the inverse direction, so the registry cannot rot into a list of
/// knobs nothing reads. (`Kind::BuildTime` names come from `build.rs`
/// `cargo::rustc-env`, which the census of *string literals* does see via
/// `env!(…)`; harness names appear in the suites.)
#[test]
fn every_registered_knob_appears_in_the_tree() {
    let literals = knob_literals_in_tree();
    let stale: Vec<&str> = env_knobs::KNOBS
        .iter()
        .filter(|k| !matches!(k.kind, Kind::Retired { .. }))
        .map(|k| k.key)
        .filter(|key| !literals.contains(*key))
        .collect();
    assert!(
        stale.is_empty(),
        "registered knob(s) that no code reads — delete them (no dead code): {stale:?}"
    );
}

/// The collision law. `SQUEEZEFS_RECLAIM_BATCH` (an INT, inode reclaim) was
/// a strict prefix of `SQUEEZEFS_RECLAIM_BATCH_BLOCKS` (block reclaim) —
/// two unrelated subsystems reading one prefix. The legitimate pattern is a
/// family's boolean switch prefixing its own parameters
/// (`SQUEEZEFS_NT_COPY` + `SQUEEZEFS_NT_COPY_MIN`), so the law is: only a
/// Bool may be a strict prefix of another knob.
#[test]
fn no_value_bearing_knob_is_a_prefix_of_another() {
    let live: Vec<&squeezefs::env_knobs::Knob> = env_knobs::KNOBS
        .iter()
        .filter(|k| {
            !matches!(
                k.kind,
                Kind::Retired { .. } | Kind::Harness | Kind::BuildTime
            )
        })
        .collect();
    for a in &live {
        for b in &live {
            if a.key == b.key || !b.key.starts_with(a.key) {
                continue;
            }
            assert_eq!(
                a.kind,
                Kind::Bool,
                "{} is a strict prefix of {} but is not a family switch — \
                 the ENG-10 collision shape (rename one of them)",
                a.key,
                b.key
            );
        }
    }
}

/// **Every registry name appears exactly once.** This is a merge-hygiene pin,
/// not a style rule: two `k(...)` rows for one name compile, pass every other
/// assertion in this file, and then disagree — the tree carried a duplicate
/// `SQUEEZEFS_MULTI_WRITER` whose second row still documented "incompat bit 10"
/// after that bit had been renumbered to 11, so `--help`-grade documentation
/// and the refusal text depended on which row a reader reached first. It came
/// from a hand-resolved conflict during the parallel DLM merges, which is
/// exactly the situation this assertion exists to make loud.
#[test]
fn every_registry_name_is_unique() {
    let mut seen: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for k in env_knobs::KNOBS.iter() {
        *seen.entry(k.key).or_insert(0) += 1;
    }
    let dupes: Vec<(&&str, &usize)> = seen.iter().filter(|(_, n)| **n > 1).collect();
    assert!(
        dupes.is_empty(),
        "these knob names are registered more than once — one name, one row: {dupes:?}"
    );
}

/// The retired spellings refuse loudly and name their successor — the
/// `format --meta-slots` precedent, never a silent alias.
#[test]
fn retired_spellings_refuse_naming_the_successor() {
    for (old, new) in [
        ("SQUEEZEFS_RECLAIM_BATCH", "SQUEEZEFS_INODE_RECLAIM_BATCH"),
        (
            "SQUEEZEFS_RECLAIM_BATCH_WINDOW_MS",
            "SQUEEZEFS_INODE_RECLAIM_WINDOW_MS",
        ),
        (
            "SQUEEZEFS_RECLAIM_CONCURRENCY",
            "SQUEEZEFS_INODE_RECLAIM_CONCURRENCY",
        ),
        (
            "SQUEEZEFS_FUSE_PLACED_MERGE",
            "(deleted — FUSE placed-merge was falsified; IL placed_sever is not this knob)",
        ),
    ] {
        let v = env_knobs::validate_vars([(old, "64")]);
        assert_eq!(v.errors.len(), 1, "{old} must refuse: {v:?}");
        let msg = &v.errors[0];
        assert!(msg.contains(old), "{msg}");
        assert!(
            msg.contains(new),
            "the refusal must name the successor: {msg}"
        );
        assert!(v.unknown.is_empty(), "a retired name is known, not unknown");
        // Unset means unset: a retired knob that is absent is not an error.
        assert!(env_knobs::validate_vars([(old, "")]).errors.is_empty());
    }
}

/// A malformed value is refused, naming the knob and the value — for every
/// kind. This is the ENG-10 requirement in one test.
#[test]
fn malformed_values_are_refused_with_the_value_named() {
    let cases = [
        ("SQUEEZEFS_READ_LANE", "yess"),                // Bool
        ("SQUEEZEFS_IPC_IDLE_SECS", "5s"),              // Int
        ("SQUEEZEFS_READ_TIER_ADMISSION", "sometimes"), // Enum
    ];
    for (key, bad) in cases {
        let v = env_knobs::validate_vars([(key, bad)]);
        assert_eq!(v.errors.len(), 1, "{key}={bad} must refuse: {v:?}");
        assert!(v.errors[0].contains(key), "{}", v.errors[0]);
        assert!(
            v.errors[0].contains(bad),
            "the refusal must quote the value: {}",
            v.errors[0]
        );
    }
}

/// Out of range is a refusal, not a silent clamp: "I set it and nothing
/// happened" is the failure mode the convention exists to delete.
#[test]
fn out_of_range_values_are_refused() {
    // Q_DEPTH's admissible range is 1..=32 (the transport clamp).
    let v = env_knobs::validate_vars([("SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH", "64")]);
    assert_eq!(v.errors.len(), 1, "{v:?}");
    assert!(v.errors[0].contains("1..=32"), "{}", v.errors[0]);
    // And the admissible value passes.
    assert!(
        env_knobs::validate_vars([("SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH", "32")])
            .errors
            .is_empty()
    );
}

/// Empty and whitespace-only are ABSENT, for every kind — `VAR=` is the
/// shell idiom for "not set" and must never be a refusal.
#[test]
fn empty_values_are_absent_not_malformed() {
    for key in [
        "SQUEEZEFS_READ_LANE",
        "SQUEEZEFS_IPC_IDLE_SECS",
        "SQUEEZEFS_READ_TIER_ADMISSION",
        "SQUEEZEFS_IPC_SOCKET_DIR",
    ] {
        for empty in ["", "   ", "\t"] {
            let v = env_knobs::validate_vars([(key, empty)]);
            assert!(
                v.errors.is_empty(),
                "{key}={empty:?} must be treated as unset, got {v:?}"
            );
        }
    }
}

/// An unregistered name in our namespace is ANNOUNCED, never refused: a
/// mixed-version fleet legitimately carries the next release's knobs, and
/// the shim's client-side knobs share this environment.
#[test]
fn unknown_names_are_announced_not_refused() {
    let v = env_knobs::validate_vars([
        ("SQUEEZEFS_TYPO_KNOB", "1"),
        ("PATH", "/usr/bin"),
        ("SQUEEZEFS_READ_LANE", "0"),
    ]);
    assert!(v.errors.is_empty(), "{v:?}");
    assert_eq!(v.unknown, vec!["SQUEEZEFS_TYPO_KNOB".to_string()]);
    assert!(!v.is_clean(), "an unknown name is still worth reporting");
}

/// Every offender is named at once — an operator with three typo'd knobs
/// learns about three, not one per restart.
#[test]
fn all_offenders_are_reported_together() {
    let v = env_knobs::validate_vars([
        ("SQUEEZEFS_READ_LANE", "maybe"),
        ("SQUEEZEFS_IPC_IDLE_SECS", "-5"),
        ("SQUEEZEFS_RECLAIM_BATCH", "64"),
    ]);
    assert_eq!(v.errors.len(), 3, "{v:?}");
}

/// The boolean law at the READER level (the sites, not just the parser):
/// `=0` disables and `=1` enables for a default-OFF knob and a default-ON
/// knob alike. `SQUEEZEFS_FREE_FORENSICS=0` used to ENABLE forensics.
#[test]
fn zero_disables_and_one_enables_at_the_reader() {
    // Serialized by construction: this test owns these two names.
    let off_by_default = "SQUEEZEFS_FREE_FORENSICS";
    let on_by_default = "SQUEEZEFS_READ_LANE";
    for (key, default) in [(off_by_default, false), (on_by_default, true)] {
        std::env::remove_var(key);
        assert_eq!(
            env_knobs::bool_knob(key, default),
            default,
            "{key} absent keeps its documented default"
        );
        for on in ["1", "true", "YES", "on"] {
            std::env::set_var(key, on);
            assert!(env_knobs::bool_knob(key, default), "{key}={on} must enable");
        }
        for off in ["0", "false", "NO", "off"] {
            std::env::set_var(key, off);
            assert!(
                !env_knobs::bool_knob(key, default),
                "{key}={off} must DISABLE (the presence-based bug)"
            );
        }
        // Malformed in-process: the documented default stands, loudly.
        std::env::set_var(key, "bogus");
        assert_eq!(
            env_knobs::bool_knob(key, default),
            default,
            "{key}=bogus keeps the default (the startup gate refuses the process)"
        );
        std::env::remove_var(key);
    }
}

/// The int/opt-int readers: absent ⇒ default/None, valid ⇒ the value,
/// malformed ⇒ default/None (announced, never a panic — four constructor
/// sites used to `panic!`, which is `abort` under the release profile).
#[test]
fn int_readers_never_panic_and_keep_their_defaults() {
    let key = "SQUEEZEFS_READ_RANGED_THRESHOLD";
    std::env::remove_var(key);
    assert_eq!(env_knobs::int_knob::<u64>(key, 262_144), 262_144);
    assert_eq!(env_knobs::opt_int_knob::<u64>(key), None);
    std::env::set_var(key, "4096");
    assert_eq!(env_knobs::int_knob::<u64>(key, 262_144), 4096);
    assert_eq!(env_knobs::opt_int_knob::<u64>(key), Some(4096));
    std::env::set_var(key, "4 0 9 6");
    assert_eq!(
        env_knobs::int_knob::<u64>(key, 262_144),
        262_144,
        "malformed keeps the default instead of killing the mount"
    );
    assert_eq!(env_knobs::opt_int_knob::<u64>(key), None);
    std::env::remove_var(key);
}

/// The enum reader, same law (this site used to `panic!` on an out-of-set
/// value, aborting a mount over a typo).
#[test]
fn enum_reader_keeps_its_default_on_a_bad_value() {
    let key = "SQUEEZEFS_READ_TIER_ADMISSION";
    let allowed = ["always", "second-touch", "never"];
    std::env::remove_var(key);
    assert_eq!(
        env_knobs::enum_knob(key, &allowed, "second-touch"),
        "second-touch"
    );
    std::env::set_var(key, "ALWAYS");
    assert_eq!(
        env_knobs::enum_knob(key, &allowed, "second-touch"),
        "always",
        "the canonical spelling comes back, case-insensitively"
    );
    std::env::set_var(key, "sometimes");
    assert_eq!(
        env_knobs::enum_knob(key, &allowed, "second-touch"),
        "second-touch"
    );
    std::env::remove_var(key);
}

/// Defaults were PRESERVED by this pass — the convention changed, not the
/// tuning. Spot-checks against the values the code carried before ENG-10.
#[test]
fn documented_defaults_match_the_shipped_ones() {
    for (key, want) in [
        ("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "50"),
        ("SQUEEZEFS_READ_RANGED_THRESHOLD", "262144"),
        ("SQUEEZEFS_READ_PREFETCH_SHARE_PCT", "50"),
        ("SQUEEZEFS_READ_ADMISSION_FILL_PCT", "5"),
        ("SQUEEZEFS_FOLD_MAX_EXTENTS", "64"),
        ("SQUEEZEFS_WRITEBACK_QUEUE_CAP", "4096"),
        ("SQUEEZEFS_INODE_RECLAIM_BATCH", "64"),
        ("SQUEEZEFS_INODE_RECLAIM_WINDOW_MS", "20"),
        ("SQUEEZEFS_RECLAIM_BATCH_BLOCKS", "64"),
        ("SQUEEZEFS_RECLAIM_CAP_PARK_MS", "1000"),
        ("SQUEEZEFS_IPC_IDLE_SECS", "300"),
        ("SQUEEZEFS_IPC_SPIN_US", "0"),
        // 2 -> 24: the 2026-08-13 fleet-residue recount (commit 047783a0)
        // — an intentional counted retune, not convention drift.
        ("SQUEEZEFS_IL_REAP_PARK_MAX", "24"),
        ("SQUEEZEFS_FSCK_SETTLE_MS", "2000"),
        ("SQUEEZEFS_NT_COPY_MIN", "262144"),
        ("SQUEEZEFS_TIMEOUT", "30"),
        ("SQUEEZEFS_FUSE_ATTR_TTL_MS", "1000"),
        ("SQUEEZEFS_SUPERVISE_INTERVAL_SECS", "5"),
        ("SQUEEZEFS_SUPERVISE_UNRESPONSIVE_SECS", "30"),
        ("SQUEEZEFS_PARKED_GATE_ASSIST_MS", "200"),
    ] {
        let knob = env_knobs::lookup(key).unwrap_or_else(|| panic!("{key} must be registered"));
        assert_eq!(
            knob.default, want,
            "{key}'s documented default drifted from the shipped one"
        );
    }
    // The boolean defaults, ON and OFF alike.
    for (key, on) in [
        ("SQUEEZEFS_READ_LANE", true),
        ("SQUEEZEFS_NT_COPY", true),
        ("SQUEEZEFS_NT_READ_SERVE", true),
        ("SQUEEZEFS_NUMA", true),
        ("SQUEEZEFS_IPC_ARENA_THP", true),
        ("SQUEEZEFS_IL_READ_DEST", true),
        ("SQUEEZEFS_REWRITE_SHADOW", true),
        ("SQUEEZEFS_DEVICE_OVERLAY", true),
        ("SQUEEZEFS_DISCARD_ELISION", true),
        ("SQUEEZEFS_FUSE_KMBUF", true),
        // FLIPPED ON by ruling D16 (user, 2026-08-07), superseding the
        // 0.97× all-write-rows rule the zcws-10 bracket failed on: the
        // hybrid lane gate changed the failing row's fleet meaning
        // (shim small ops ride the ring, never FUSE), so the −20 %
        // rand-4k write tax applies only to UN-shimmed kernel-lane
        // small writes — accepted, documented in operations.md, with
        // handler/worker fusion fast-tracked as its fix. Reads gain
        // +40–75 % on every armed-capable mount; wedge class closed
        // (bounded-outcome ladder); stock kernels decline loud and run
        // the bufring path byte-identically. `SQUEEZEFS_FUSE_ZC=0` is
        // the escape/A-B lever. rc-manifest §3f D16 is the ruling.
        ("SQUEEZEFS_FUSE_ZC", true),
        ("SQUEEZEFS_FREE_FORENSICS", false),
        ("SQUEEZEFS_OP_PROFILE", false),
        ("SQUEEZEFS_INPLACE_OVERWRITE", false),
        ("SQUEEZEFS_DIRECT_DEVICE_TRUE", false),
        ("SQUEEZEFS_IPC_ALLOW_DEV", false),
        ("SQUEEZEFS_FUSE_NO_KILLPRIV", false),
        ("SQUEEZEFS_ZCRX_LANE", false),
    ] {
        let knob = env_knobs::lookup(key).unwrap_or_else(|| panic!("{key} must be registered"));
        assert_eq!(knob.kind, Kind::Bool, "{key} must be a Bool knob");
        assert_eq!(
            knob.default,
            if on { "on" } else { "off" },
            "{key}'s documented default drifted"
        );
    }
}

/// ENG-11: `SQUEEZEFS_IPC_ALLOW_DEV` announces itself on BOTH ends. The
/// notices are pure text so the contract is testable without arming the
/// lever, and each names the knob, the relaxation and the risk.
#[test]
fn allow_dev_announces_on_both_ends() {
    let daemon = squeezefs::ipc_host::allow_dev_notice();
    let shim = squeezefs_il::session::allow_dev_notice();
    for (side, msg) in [("daemon", daemon), ("shim", shim)] {
        assert!(
            msg.contains("SQUEEZEFS_IPC_ALLOW_DEV"),
            "{side} notice must name the knob: {msg}"
        );
        assert!(
            msg.contains("KD-7") && msg.to_lowercase().contains("relax"),
            "{side} notice must say WHAT is relaxed: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("dev"),
            "{side} notice must say it is dev-only: {msg}"
        );
    }
    // Absent ⇒ off on both sides, and no announcement to make.
    std::env::remove_var("SQUEEZEFS_IPC_ALLOW_DEV");
    assert!(!squeezefs::ipc_host::allow_dev_lever());
    assert!(!squeezefs_il::session::allow_dev_lever());
    // The ENG-10 boolean law applies to it too: `=0` disables.
    std::env::set_var("SQUEEZEFS_IPC_ALLOW_DEV", "0");
    assert!(!squeezefs::ipc_host::allow_dev_lever());
    assert!(!squeezefs_il::session::allow_dev_lever());
    std::env::set_var("SQUEEZEFS_IPC_ALLOW_DEV", "1");
    assert!(squeezefs::ipc_host::allow_dev_lever());
    assert!(squeezefs_il::session::allow_dev_lever());
    std::env::remove_var("SQUEEZEFS_IPC_ALLOW_DEV");
}

/// The shared core is the ONE parser: the daemon, the fuse3 fork and the
/// shim all resolve `crate::env_knob_core` to the same FILE, so a spelling
/// accepted on one side is accepted on the others. Pinned by identity of
/// the accepted sets (the file is `#[path]`-shared, not copied).
#[test]
fn the_bool_spelling_set_is_shared_by_every_consumer() {
    assert_eq!(
        squeezefs::env_knob_core::BOOL_TRUE,
        squeezefs_ipc::env_knob_core::BOOL_TRUE
    );
    assert_eq!(
        squeezefs::env_knob_core::BOOL_FALSE,
        squeezefs_ipc::env_knob_core::BOOL_FALSE
    );
    assert_eq!(
        squeezefs_il::env_knob_core::BOOL_TRUE,
        squeezefs_ipc::env_knob_core::BOOL_TRUE
    );
    assert_eq!(
        fuse3::env_knob_core::BOOL_TRUE,
        squeezefs_ipc::env_knob_core::BOOL_TRUE
    );
}

// ---------------------------------------------------------------------------
// The gate itself, through the real binary (the enforcement point).
// ---------------------------------------------------------------------------

fn run_with(env: &[(&str, &str)]) -> (bool, String, String) {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_squeezefs"));
    cmd.arg("--version");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn squeezefs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// A malformed knob refuses the PROCESS — before arguments are interpreted,
/// before any volume is opened, before anything is mounted — with a nonzero
/// exit and the offender named. This is what "loud refusal" means for a
/// filesystem daemon; the 56 silent-default sites are now unreachable with a
/// bad value.
#[test]
fn the_binary_refuses_a_malformed_knob_before_doing_anything() {
    let (ok, _, stderr) = run_with(&[("SQUEEZEFS_READ_LANE", "yess")]);
    assert!(
        !ok,
        "a malformed knob must fail the process; stderr: {stderr}"
    );
    assert!(stderr.contains("SQUEEZEFS_READ_LANE"), "{stderr}");
    assert!(
        stderr.contains("yess"),
        "the value must be quoted: {stderr}"
    );
    assert!(
        stderr.contains("refusing to start"),
        "the refusal must say it refused: {stderr}"
    );
}

/// A retired spelling refuses and names its successor.
#[test]
fn the_binary_refuses_a_retired_spelling() {
    let (ok, _, stderr) = run_with(&[("SQUEEZEFS_RECLAIM_BATCH", "128")]);
    assert!(!ok, "{stderr}");
    assert!(stderr.contains("SQUEEZEFS_INODE_RECLAIM_BATCH"), "{stderr}");
}

/// An unknown name in our namespace warns and RUNS: refusing it would break
/// mixed-version fleets and the shim's knobs, which share this environment.
#[test]
fn the_binary_warns_about_an_unknown_name_and_runs() {
    let (ok, stdout, stderr) = run_with(&[("SQUEEZEFS_NOT_A_KNOB", "1")]);
    assert!(ok, "an unknown name must not refuse the process: {stderr}");
    assert!(stderr.contains("SQUEEZEFS_NOT_A_KNOB"), "{stderr}");
    assert!(stdout.contains("squeezefs "), "{stdout}");
}

/// Valid values — including every accepted boolean spelling and an empty
/// (= unset) value — run clean and silent.
#[test]
fn the_binary_accepts_valid_values_silently() {
    for (k, v) in [
        ("SQUEEZEFS_READ_LANE", "off"),
        ("SQUEEZEFS_READ_LANE", "NO"),
        ("SQUEEZEFS_READ_LANE", "1"),
        ("SQUEEZEFS_READ_LANE", ""),
        ("SQUEEZEFS_READ_TIER_ADMISSION", "never"),
        ("SQUEEZEFS_IPC_IDLE_SECS", "600"),
        ("SQUEEZEFS_INODE_RECLAIM_BATCH", "128"),
    ] {
        let (ok, _, stderr) = run_with(&[(k, v)]);
        assert!(ok, "{k}={v:?} must be accepted; stderr: {stderr}");
        assert!(
            !stderr.contains("refusing"),
            "{k}={v:?} produced a refusal: {stderr}"
        );
    }
}

/// A clean environment produces no refusal report — the gate is silent when
/// there is nothing to say (it runs on every single invocation of the CLI).
#[test]
fn a_clean_environment_produces_no_report() {
    let v = env_knobs::validate_vars([
        ("SQUEEZEFS_READ_LANE", "0"),
        ("SQUEEZEFS_IPC_IDLE_SECS", "600"),
        ("SQUEEZEFS_READ_TIER_ADMISSION", "always"),
        ("HOME", "/root"),
    ]);
    assert!(v.is_clean(), "{v:?}");
}
