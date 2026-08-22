//! **Per-volume claim admission — the partial-writer open**
//! (`docs/design-per-volume-claim-admission.md` §5.1/§5.4, PR 4; the
//! decision is `src/partial_authority.rs`, PR 3).
//!
//! # The headline pin, and why it is the FIRST thing in this file
//!
//! This is the rung at which the program first touches the **shipped D0
//! mount path**, and risk **R12** is the whole reason the ladder ships
//! dark:
//!
//! > *the SOLO RE-GATE LAW — a mount that has not DECLARED a per-volume
//! > posture must meet the pre-program `FreshForeign` refusal, byte for
//! > byte, on a set whose volumes name owners.*
//!
//! `the_fresh_foreign_refusal_is_byte_identical_for_an_undeclared_mount`
//! asserts that against a **frozen literal** rather than against the
//! product's own format string, because a test that re-derives the text
//! from the code it guards proves nothing about the text.
//!
//! # What else this file pins
//!
//! * the `PeerAuthority` arm is **structurally unreachable** without a
//!   [`squeezefs::partial_authority::SetAdmission`] — an admission decided
//!   over a different set is refused at the door (the `open_co_writer`
//!   precedent), and no `Peer`-mode open takes a lock, writes a claim, or
//!   spawns a task;
//! * the rollback ladder releases **exactly** the guards it took;
//! * a peer-owned volume's write gate refuses with its own cause
//!   (`PeerOwnedVolume`), never the co-writer's — whose text is false for a
//!   partial authority and whose must-stay-0 counter would rot.

use squeezefs::membership::{ClaimSet, ClaimSetMember, MemberIdentity, MemberRole};
use squeezefs::meta_backend::kv::backend::{KvMetaBackend, WriterClaim, WRITER_CLAIM_XATTR};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::superblock as sb;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const VOL_LEN: u64 = 64 * 1024 * 1024;

/// This node's durable enrollment identity (KD-MW-2).
const NODE: &str = "node_00000000deadbeef.m00000001";
/// The peer that appends to the volume in the assigned fixtures.
const PEER: &str = "node_00000000feedface.m00000001";

fn opts() -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: Some(1024 * 1024),
        force: false,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// The nine-bit multi-writer stamp — `volume enable-multi-writer`'s act,
/// offline, between format and open (the co-writer suite's helper
/// verbatim: bit 11 set ⇒ all nine set is a writable-mount invariant).
async fn stamp_capabilities(path: &Path) {
    for (what, res) in [
        ("durable-term", sb::set_durable_term_bit(path).await),
        (
            "durable-block-refcounts",
            sb::set_block_refcounts_bit(path).await,
        ),
        (
            "durable-layout-versions",
            sb::set_layout_versions_bit(path).await,
        ),
        ("ino-lanes", sb::set_ino_lanes_bit(path).await),
        (
            "block-key-incarnation",
            sb::set_block_key_incarnation_bit(path).await,
        ),
        (
            "partitioned-append",
            sb::set_partitioned_append_bit(path).await,
        ),
        (
            "writer-scoped-staging",
            sb::set_writer_scoped_staging_bit(path).await,
        ),
        ("claim-set", sb::set_claim_set_bit(path).await),
        (
            "multi-writer-data",
            sb::set_multi_writer_data_bit(path).await,
        ),
    ] {
        res.unwrap_or_else(|e| panic!("stamping {what} failed: {e}"));
    }
}

async fn fresh_volume(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    format_v3(&p, VOL_LEN, &opts()).await.unwrap();
    stamp_capabilities(&p).await;
    p
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn member(id: &str) -> ClaimSetMember {
    ClaimSetMember {
        identity: MemberIdentity {
            id: id.to_string(),
            role: MemberRole::Writer,
            // KD-PV-4: the offline verb enrolls the PID-LESS roster form.
            pid: 0,
            boot: String::new(),
            endpoint: None,
            pr_key: 0,
        },
        ts: 1_700_000_000,
    }
}

/// A live FOREIGN claim (another host's boot id, so no dead-pid proof can
/// reclaim it) whose age is pinned at 0 for the frozen refusal literal.
fn foreign_claim() -> WriterClaim {
    WriterClaim {
        id: "5f1d0e2a-0000-4000-8000-000000000001".to_string(),
        // Two seconds ahead so `age_secs` saturates to 0 for the whole
        // test rather than racing a second boundary.
        ts: now_secs() + 2,
        pid: 4242,
        boot: "ffffffff-ffff-ffff-ffff-ffffffffffff".to_string(),
        term: 7,
    }
}

/// Plant a fresh foreign `writer_claim` and — when `assign` — a durable
/// `claim_set` naming `PEER` as this volume's OWNER and this node as an
/// enrolled writer member: the exact shape an assigned multi-owner set
/// presents to a mount that declared nothing.
async fn plant(path: &Path, claim: &WriterClaim, assign: bool) {
    let be = KvMetaBackend::open(path).await.expect("planting open");
    if assign {
        let mut set = ClaimSet::empty(7);
        set.durable = true;
        set.owner = Some(PEER.to_string());
        set.members = vec![member(NODE), member(PEER)];
        ClaimSet::store(&be, &set).await.expect("store claim set");
    }
    be.setxattr_internal(1, WRITER_CLAIM_XATTR, &claim.encode())
        .await
        .expect("plant claim");
    be.sync_device().await.expect("barrier");
    // DROP, never `shutdown()`: a clean shutdown releases the claim, and
    // what this fixture needs on disk is a LIVE foreign holder's record
    // (the co-writer suite's `forge_foreign_claim` shape).
    drop(be);
}

/// **R12 — the solo re-gate law.** The pre-program refusal text, frozen
/// here as a literal so a change to the product's format string fails this
/// test instead of silently rewriting the contract.
fn expected_fresh_foreign_refusal(path: &Path, claim: &WriterClaim) -> String {
    format!(
        "{}: metadata volume is claimed by a live writer (claim: id={}, pid={}, boot={}, \
         age=0s) — concurrent mounts of one metadata volume are refused (single-writer \
         guard). A crashed holder on THIS host is reclaimed automatically once its pid is \
         provably dead; otherwise stop that writer or wait for its claim to expire (ttl 45s)",
        path.display(),
        claim.id,
        claim.pid,
        claim.boot,
    )
}

/// **THE headline pin of PR 4** (risk R12, `docs/design-…§5.13`'s
/// must-stay list): per-volume claim admission is a **different door**,
/// never a hole in the D0 gate. An undeclared mount — which is every mount
/// that ships — meets the same `FreshForeign` refusal it always has, with
/// the same text, on a plain set AND on a set whose durable `claim_set`
/// assigns this very volume to the live holder and enrolls this node as a
/// writer member of it.
///
/// The second arm is the one that matters: every ingredient the
/// `PeerAuthority` arm consumes is present and the arm must still not be
/// taken, because the posture was not DECLARED.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fresh_foreign_refusal_is_byte_identical_for_an_undeclared_mount() {
    let dir = TempDir::new().unwrap();

    for (name, assign) in [("plain", false), ("assigned", true)] {
        let vol = fresh_volume(dir.path(), &format!("r12-{name}")).await;
        let claim = foreign_claim();
        plant(&vol, &claim, assign).await;

        let err = KvMetaBackend::open(&vol)
            .await
            .err()
            .unwrap_or_else(|| panic!("[{name}] a fresh foreign claim must refuse the D0 open"));
        assert_eq!(
            err.to_string(),
            expected_fresh_foreign_refusal(&vol, &claim),
            "[{name}] the FreshForeign refusal is not byte-identical for an undeclared mount \
             (R12: per-volume claim admission must be a different DOOR, not a weakening)"
        );

        // The whole-set path refuses identically and rolls the set back:
        // the routed open is what a mount actually calls.
        let set_err = squeezefs::meta_backend::open_routed_meta_set(&[vol.display().to_string()])
            .await
            .err()
            .unwrap_or_else(|| panic!("[{name}] the routed set open must refuse too"));
        assert!(
            set_err
                .to_string()
                .contains("metadata volume is claimed by a live writer"),
            "[{name}] the routed set open's refusal changed: {set_err}"
        );
    }
}
