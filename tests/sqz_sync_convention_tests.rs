//! The sqz-sync convention rail (docs/design-sqz-sync.md §Enforcement
//! rail — the env-knob-registry pattern: the convention is a test, so
//! regression is a red gate, not a review hope).
//!
//! Stage 1 ("metadata plane scheduler-free") migrated every lock in the
//! metadata-plane population off `tokio::sync::{Mutex, RwLock}` onto
//! `crate::sqz_sync::{SqzMutex, SqzRwLock}` — the barging, tick-backstopped
//! primitives on which the OQ-5 lost-wakeup wedge class is unrepresentable
//! (a never-polled waiter owns nothing and blocks nobody). A tokio lock
//! literal reappearing in the population is a scheduler-ownership
//! regression: it reintroduces the assign-to-sleeping-waiter protocol the
//! 464-at-3.0GHz wedge selected against.
//!
//! Exemptions are explicit and same-line: `// sqz-sync-exempt` marks a
//! lock that is NOT part of the metadata plane (today: `ZcWriteSlot::
//! materialized`, a per-write data-path memo — it migrates with its own
//! stage). An exemption without a marker is a failure; a marker is a
//! documented decision, greppable.

use std::path::Path;

/// The Stage-1 population: every file whose locks serialize the metadata
/// plane (P1-9 levels 1/2/3/3.5/4a and the 4b node-lock guts), plus the
/// direct-drive carrier that threads the level-3 block guard.
const POPULATION: &[&str] = &[
    "src/meta_backend/dlm.rs",
    "src/meta_backend/mod.rs",
    "src/meta_backend/kv/backend.rs",
    "src/meta_backend/kv/node_cache.rs",
    "src/meta_backend/kv/journal.rs",
    "src/meta_backend/kv/checkpoint.rs",
    "src/meta_backend/kv/superblock.rs",
    "src/meta_backend/kv/tree.rs",
    "src/meta_backend/kv/block_refs.rs",
    "src/fuse_client.rs",
    "src/routing.rs",
    "src/stripe_locks.rs",
    "src/ipc_direct.rs",
    "src/ipc_service.rs",
];

/// The forbidden literals. Guard types are covered transitively: a
/// `tokio::sync::MutexGuard` cannot be produced without one of these.
const FORBIDDEN: &[&str] = &["tokio::sync::Mutex", "tokio::sync::RwLock"];

#[test]
fn metadata_plane_population_holds_no_tokio_locks() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut violations = Vec::new();
    for rel in POPULATION {
        let path = repo.join(rel);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("population file {rel} unreadable: {e}"));
        for (idx, line) in src.lines().enumerate() {
            if line.contains("sqz-sync-exempt") {
                continue;
            }
            for lit in FORBIDDEN {
                if line.contains(lit) {
                    violations.push(format!("{rel}:{}: {}", idx + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "tokio lock literal(s) in the Stage-1 metadata-plane population — \
         use crate::sqz_sync::{{SqzMutex, SqzRwLock}} (docs/design-sqz-sync.md) \
         or mark a documented non-plane lock `// sqz-sync-exempt`:\n{}",
        violations.join("\n")
    );
}

/// The population list itself must not rot: every named file exists.
#[test]
fn population_files_exist() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    for rel in POPULATION {
        assert!(
            repo.join(rel).is_file(),
            "sqz-sync population file missing: {rel} — if it moved, update \
             POPULATION in tests/sqz_sync_convention_tests.rs"
        );
    }
}
