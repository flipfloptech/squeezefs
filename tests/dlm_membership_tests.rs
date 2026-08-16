//! DLM **stage S6** — membership off the journal (lease-based liveness) —
//! plus **spec §6.2 item 7**, the claim-set record
//! (`docs/pre-rc-engineering-spec.md` §6.5 item 3, §6.7 "Recovery" + "Two
//! lease clocks", §6.9 S6 row; `docs/pre-rc-execution-plan.md` Phase 4).
//!
//! ## The measured problem these contracts exist to close
//!
//! §6.5 item 3, verbatim numbers: each client writes `client:{uuid}`
//! **every 10 s as a full journal transaction** under an exclusive `I{1}`
//! guard, and ino 1 routes to slot 0 → one volume unconditionally. At the
//! measured saturated `commit_tx_wait` of 2.198 ms that volume serializes
//! **455 beats/s** against the **1,500/s** that 15,000 clients require;
//! past saturation records age past the 45 s TTL and every liveness
//! consumer starts calling live mounts dead. **The read side is worse**:
//! `mount_registrations()` is `listxattr(1)` plus one `getxattr` per
//! client, each under a shared `I{1}` lock, per `squeezefs clients`, per
//! `status`, and in the format preflight.
//!
//! S6's answer: **liveness is not durable state**. A member's durable
//! footprint is written ONCE (the owner's rendezvous record, and — when
//! §6.2 item 7's claim set is engaged — one set-wide identity record
//! rewritten on membership CHANGE), and liveness rides lease renewal over
//! `cluster_wire` with the lease table in RAM. A beat therefore costs
//! **zero journal transactions**, and the census is one paged RPC off the
//! metadata plane instead of `O(clients)` xattr reads under `I{1}`.
//!
//! ## Contracts pinned here
//!
//! 1. **A heartbeat costs ZERO journal transactions** — asserted against
//!    `meta_kv_journal_entries` (the honest instrument), with a CONTRAST
//!    arm that performs the legacy `client:{uuid}` beat shape and proves
//!    the counter is live rather than dead.
//! 2. **The read side does not scale with member count**: 500 joins add
//!    no root-ino xattr keys and no journal entries; the census is served
//!    from RAM in `ceil(N/limit)` pages.
//! 3. **A reader becomes visible without performing a metadata write** —
//!    the S5 gap (`src/ro_coherence.rs`: "readers do not appear in
//!    `squeezefs clients`") closed with zero writes, and the same channel
//!    §6.8 item 3 will use: `MemberSession::ack_free_epoch` →
//!    `MembershipOwner::min_acked_free_epoch` /
//!    `members_behind_free_epoch` / `evict`.
//! 4. **Two lease clocks, and the client's is stricter** (§6.7):
//!    `T_self = T_owner − 2·skew_max − D_purge` on monotonic clocks
//!    anchored on the RPC round trip. The client fail-stops the affected
//!    objects ITSELF before the owner can grant them elsewhere; a
//!    configuration where that inequality collapses **refuses loud**
//!    instead of clamping.
//! 5. **Lease expiry → dead epoch → S7 quarantine** composes: an evicted
//!    member's epoch is declared dead, its offsets are not reallocatable,
//!    and only a drain proof releases them.
//! 6. **Claim-set membership keeps single-writer byte-identical**: no
//!    `claim_set` record is written on an un-stamped volume, `writer_claim`
//!    bytes are untouched, and every consumer reads ONE shape because the
//!    un-engaged form is a *projection* of `writer_claim`.
//! 7. **Owner failure recovers by re-assertion** (NFSv4 style, §6.7): a
//!    successor that has not bumped `term` durably cannot arm; a successor
//!    that has opens a **grace window** admitting reclaim and refusing
//!    conflicting fresh acquires, and the window closes when the prior
//!    membership has re-asserted.
//! 8. **DISC-1 discovery rides the new plane** — `peers_from_census`
//!    obeys the same selection law as `cluster_wire::peers_from_registrations`
//!    (fresh only, endpoint required, self excluded, deduped, ordered).
//! 9. **Concurrency**: many simulated clients renewing on a multi-thread
//!    runtime while one is evicted — no live member loses its lease, the
//!    evicted one is refused, and the census stays consistent.
//! 10. **Incompat bit 14** (`KV_CLAIM_SET`) is single-bit, disjoint from
//!     every other feature bit, and **never stamped** by a production
//!     format (ruling D9).
//!
//! RED against `dev` @ `1af8799c`: `squeezefs::membership`,
//! `squeezefs::membership_wire` and `squeezefs::membership_sim` do not
//! exist, and there is no incompat bit 14.
//!
//! ## What is NOT pinned here (and cannot be)
//!
//! * The **15 k row** (ruling D1 validation tier (ii)): the harness lands
//!   here and runs at a few hundred clients so its own machinery is
//!   proven; the 15,000-client row — volume-0 journal tx/s, renewal
//!   latency distribution, revoke fan-out, failover grace completion — is
//!   DEFERRED per ruling **D11** and is exactly what
//!   `membership_sim::SimReport::render` prints when the orchestrator runs
//!   it.
//! * **Cross-host** clock skew, real fabric RTT, and a real owner crash on
//!   another host: in-process we pin the decision law, the counting, the
//!   asymmetry and the refusals. A fabric row needs the venue.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::fuse_client::{CLIENT_HEARTBEAT_INTERVAL_SECS, CLIENT_STALE_TTL_SECS, METRICS};
use squeezefs::membership::{
    self, ClaimSet, ClaimSetMember, JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole,
    MemberSession, MembershipOwner, OwnerRecord, RenewOutcome, CLAIM_SET_XATTR,
    MEMBERSHIP_OWNER_XATTR,
};
use squeezefs::membership_sim::{SimConfig, SimMode};
use squeezefs::membership_wire::{
    peers_from_census, MemberClient, MembershipPlane, MembershipPlaneConfig, VerbRouter,
    RPC_MEMBERSHIP_REFUSED, RPC_MEMBERSHIP_UNKNOWN_LEASE, VERB_MEMBERSHIP_CENSUS,
};
use squeezefs::meta_backend::kv::backend::{KvMetaBackend, WriterClaim, WRITER_CLAIM_XATTR};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::meta_backend::kv::META_KV_JOURNAL_ENTRIES;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const VOL_LEN: u64 = 64 * 1024 * 1024;

/// Tests that touch PROCESS-GLOBAL state (the `METRICS` membership family,
/// the S7 poison latch, the plane registry) serialize on this — libtest
/// runs a file's tests on threads and the gate's `--test-threads=1` bounds
/// files, not tests within one. Same shape (and reasoning) as
/// `tests/dlm_data_fence_tests.rs`.
static SERIAL: AtomicBool = AtomicBool::new(false);

struct Serial;

fn serial() -> Serial {
    while SERIAL.swap(true, Ordering::AcqRel) {
        std::thread::sleep(Duration::from_millis(2));
    }
    Serial
}

impl Drop for Serial {
    fn drop(&mut self) {
        SERIAL.store(false, Ordering::Release);
    }
}

fn opts() -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: Some(1024 * 1024),
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

async fn formatted_volume() -> NamedTempFile {
    let meta = NamedTempFile::new().expect("temp volume");
    meta.as_file().set_len(VOL_LEN).expect("size the volume");
    format_v3(meta.path(), VOL_LEN, &opts())
        .await
        .expect("format v3");
    meta
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("post-epoch clock")
        .as_secs()
}

/// A manual lease clock plus its tick word (the `cluster_wire::WireClock`
/// precedent: freshness and expiry must be provable without a sleep).
fn manual_clock() -> (LeaseClock, Arc<AtomicU64>) {
    let ticks = Arc::new(AtomicU64::new(1_000));
    (LeaseClock::manual(Arc::clone(&ticks)), ticks)
}

/// The shipped clock parameters, resolved from the knobs' derived defaults
/// with a fixed observed RTT so the arithmetic is deterministic.
fn shipped_clocks() -> LeaseClocks {
    LeaseClocks::derive(Duration::from_micros(250)).expect("the shipped derivation must be safe")
}

fn owner_with(clocks: LeaseClocks, clock: LeaseClock, term: u64) -> Arc<MembershipOwner> {
    MembershipOwner::arm("owner-test", term, term.saturating_sub(1), clocks, clock)
        .expect("arming a successor term must be admitted")
}

fn join_req(id: &str, role: MemberRole, endpoint: Option<&str>) -> JoinRequest {
    JoinRequest {
        id: id.to_string(),
        role,
        endpoint: endpoint.map(str::to_string),
        pid: std::process::id(),
        boot: "boot-test".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    }
}

fn granted(out: JoinOutcome) -> membership::Grant {
    match out {
        JoinOutcome::Granted(g) => g,
        JoinOutcome::Refused {
            reason,
            retry_after_ms,
        } => panic!("join refused ({reason}, retry after {retry_after_ms} ms)"),
        JoinOutcome::UnknownLease { reason } => panic!("join answered UnknownLease ({reason})"),
    }
}

// ---------------------------------------------------------------------------
// 1. A heartbeat costs ZERO journal transactions
// ---------------------------------------------------------------------------

/// **The S6 gate's in-process face.** 500 lease renewals — the plane's
/// whole liveness mechanism — must not commit a single metadata
/// transaction, and the same volume's legacy `client:{uuid}` beat shape
/// must still move `meta_kv_journal_entries`, so the instrument is proven
/// live rather than assumed.
#[tokio::test]
async fn a_heartbeat_costs_zero_journal_transactions() {
    let _serial = serial();
    let meta = formatted_volume().await;
    let be = KvMetaBackend::open(meta.path()).await.expect("open volume");

    let (clock, ticks) = manual_clock();
    let clocks = shipped_clocks();
    let owner = owner_with(clocks.clone(), clock.clone(), 4);
    let grant = granted(owner.join(join_req("m-beat", MemberRole::Reader, None)));

    let before = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    for beat in 0..500u64 {
        // Advance by the renewal cadence — the plane's actual beat.
        ticks.store(
            1_000 + beat * clocks.renew_interval.as_millis() as u64,
            Ordering::SeqCst,
        );
        match owner.renew("m-beat", grant.epoch, 0) {
            RenewOutcome::Renewed(_) => {}
            RenewOutcome::UnknownLease { reason } => panic!("live lease refused: {reason}"),
        }
    }
    let after = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    assert_eq!(
        after,
        before,
        "500 lease renewals committed {} journal transaction(s): the S6 heartbeat MUST NOT \
         touch the journal (spec §6.5 item 3)",
        after - before
    );

    // CONTRAST: the legacy beat shape on the same volume. Without this arm
    // the assertion above would also pass against a dead counter.
    let legacy = format!("{{\"ts\":{},\"pid\":{}}}", now_secs(), std::process::id());
    be.setxattr_internal(1, "client:legacy-beat", legacy.as_bytes())
        .await
        .expect("legacy registration write");
    let with_legacy = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    assert!(
        with_legacy > after,
        "the legacy client:{{uuid}} beat must still cost journal entries — otherwise \
         meta_kv_journal_entries is not the instrument this gate thinks it is"
    );
    be.shutdown().await.expect("clean shutdown");
}

/// The owner's DURABLE footprint is written once at arm and once at
/// disarm — never per beat. (Mechanism (a): the rendezvous stays durable,
/// the liveness does not.)
#[tokio::test]
async fn the_owner_record_is_written_once_at_arm_not_per_beat() {
    let _serial = serial();
    let meta = formatted_volume().await;
    let be = KvMetaBackend::open(meta.path()).await.expect("open volume");

    let rec = OwnerRecord {
        v: 1,
        id: "owner-1".into(),
        term: 9,
        endpoint: "127.0.0.1:7000".into(),
        owner_claim_id: String::new(),
        ttl_ms: CLIENT_STALE_TTL_SECS * 1000,
        ts: now_secs(),
        pid: std::process::id(),
        boot: "boot-test".into(),
    };
    let before = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    membership::publish_owner_record(&be, &rec)
        .await
        .expect("publish the rendezvous record");
    let after_publish = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    assert!(
        after_publish > before,
        "the once-at-mount rendezvous record is a real durable commit"
    );

    let read = membership::read_owner_record(&be)
        .await
        .expect("the record must read back");
    assert_eq!(read.endpoint, rec.endpoint);
    assert_eq!(read.term, 9);
    assert_eq!(read.ttl_ms, CLIENT_STALE_TTL_SECS * 1000);

    // Exactly one key, whatever the member count: this is what makes the
    // read side stop scaling with clients.
    let keys = be.listxattr(1).await.expect("listxattr");
    assert_eq!(
        keys.iter()
            .filter(|k| k.as_str() == MEMBERSHIP_OWNER_XATTR)
            .count(),
        1
    );

    membership::clear_owner_record(&be)
        .await
        .expect("clean disarm removes the record");
    assert!(membership::read_owner_record(&be).await.is_none());
    be.shutdown().await.expect("clean shutdown");
}

// ---------------------------------------------------------------------------
// 2. The read side stops scaling with member count
// ---------------------------------------------------------------------------

/// 500 members must add zero root-ino xattr keys and zero journal
/// entries, and the census must serve them from RAM in `ceil(N/limit)`
/// pages. (§6.5 item 3's "the read side is worse" — closed by members
/// living in RAM instead of in records.)
#[tokio::test]
async fn read_side_cost_is_independent_of_member_count() {
    let _serial = serial();
    let meta = formatted_volume().await;
    let be = KvMetaBackend::open(meta.path()).await.expect("open volume");
    let keys_before = be.listxattr(1).await.expect("listxattr").len();
    let journal_before = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);

    let (clock, _ticks) = manual_clock();
    let owner = owner_with(shipped_clocks(), clock, 3);
    for i in 0..500u32 {
        let role = if i % 4 == 0 {
            MemberRole::Writer
        } else {
            MemberRole::Reader
        };
        let ep = format!("10.0.0.{}:7100", 1 + (i % 200));
        granted(owner.join(join_req(&format!("m{i}"), role, Some(&ep))));
    }
    assert_eq!(owner.len(), 500);
    assert_eq!(owner.readers(), 375);
    assert_eq!(owner.writers(), 125);

    assert_eq!(
        be.listxattr(1).await.expect("listxattr").len(),
        keys_before,
        "a member must not become a root-ino xattr key (that is the O(clients) read side)"
    );
    assert_eq!(
        META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed),
        journal_before,
        "500 joins must not commit a metadata transaction"
    );

    // The census is paged, so a 15 k census cannot become one oversized
    // frame; pages are what the read side pays instead of per-client
    // getxattr under a shared I{1} lock.
    let mut cursor = Some(0u64);
    let mut seen = 0usize;
    let mut pages = 0usize;
    while let Some(c) = cursor {
        let (rows, next) = owner.census(c, 128);
        seen += rows.len();
        pages += 1;
        cursor = next;
        assert!(pages <= 8, "paging must terminate");
    }
    assert_eq!(seen, 500, "every member must appear exactly once");
    assert_eq!(
        pages, 4,
        "500 members at limit 128 is ceil(500/128) = 4 pages"
    );
    be.shutdown().await.expect("clean shutdown");
}

// ---------------------------------------------------------------------------
// 3. Readers visible with no metadata write + the §6.8 item-3 channel
// ---------------------------------------------------------------------------

/// The S5 gap: "readers do not appear in `squeezefs clients` — the
/// `client:` registration is a metadata write, and a reader performs
/// none." A reader joins the plane, appears in the census with its role,
/// and the volume's records are untouched.
#[tokio::test]
async fn readers_become_visible_without_a_metadata_write() {
    let _serial = serial();
    let meta = formatted_volume().await;
    let be = KvMetaBackend::open(meta.path()).await.expect("open volume");
    let keys_before = be.listxattr(1).await.expect("listxattr");
    let journal_before = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);

    let (clock, _ticks) = manual_clock();
    let owner = owner_with(shipped_clocks(), clock, 2);
    granted(owner.join(join_req("reader-a", MemberRole::Reader, None)));
    granted(owner.join(join_req(
        "writer-a",
        MemberRole::Writer,
        Some("10.0.0.9:7100"),
    )));

    let (rows, _) = owner.census(0, 64);
    let reader = rows
        .iter()
        .find(|r| r.id == "reader-a")
        .expect("the reader must be visible in the census");
    assert_eq!(reader.role, MemberRole::Reader);
    assert_eq!(reader.state, "live");
    assert!(
        reader.endpoint.is_none(),
        "a reader publishes no endpoint and needs none"
    );

    assert_eq!(be.listxattr(1).await.expect("listxattr"), keys_before);
    assert_eq!(
        META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed),
        journal_before,
        "a reader's membership must cost zero metadata transactions"
    );
    be.shutdown().await.expect("clean shutdown");
}

/// **The exact API §6.8 item 3 will call.** Readers acknowledge the freed
/// offset epoch they have passed on their renewals; the writer asks the
/// owner for the minimum across LIVE members, names the laggards, and
/// fences (never waits on) one that will not acknowledge.
#[test]
fn free_epoch_acks_expose_the_minimum_across_live_members() {
    let _serial = serial();
    let (clock, _ticks) = manual_clock();
    let owner = owner_with(shipped_clocks(), clock, 2);
    let a = granted(owner.join(join_req("r-a", MemberRole::Reader, None)));
    let b = granted(owner.join(join_req("r-b", MemberRole::Reader, None)));

    // Nothing acknowledged yet: the writer may reallocate nothing.
    assert_eq!(owner.min_acked_free_epoch(), 0);

    assert!(matches!(
        owner.renew("r-a", a.epoch, 7),
        RenewOutcome::Renewed(_)
    ));
    assert!(matches!(
        owner.renew("r-b", b.epoch, 4),
        RenewOutcome::Renewed(_)
    ));
    assert_eq!(
        owner.min_acked_free_epoch(),
        4,
        "the minimum over live members is the reallocation bound"
    );
    assert_eq!(owner.members_behind_free_epoch(7), vec!["r-b".to_string()]);

    // "A reader that fails to acknowledge is fenced, not waited on."
    let evicted = owner
        .evict("r-b", "did not acknowledge free epoch 7")
        .expect("evicting a named member");
    assert_eq!(evicted.id, "r-b");
    assert_eq!(
        owner.min_acked_free_epoch(),
        7,
        "fencing the laggard advances the bound"
    );
    assert!(owner.members_behind_free_epoch(7).is_empty());
}

// ---------------------------------------------------------------------------
// 4. Two lease clocks, and the client's is stricter
// ---------------------------------------------------------------------------

/// `T_self = T_owner − 2·skew_max − D_purge`, and the anchor is the
/// client's SEND instant, so the RTT counts against the client too.
#[test]
fn the_client_lease_clock_is_strictly_stricter_than_the_owners() {
    let clocks = shipped_clocks();
    assert_eq!(
        clocks.t_owner,
        Duration::from_secs(CLIENT_STALE_TTL_SECS),
        "the owner TTL is the ONE staleness law's 45 s (spec item 7 / operations.md)"
    );
    assert_eq!(
        clocks.t_self,
        clocks.t_owner - 2 * clocks.skew_max - clocks.d_purge,
        "the client deadline is the spec's formula verbatim"
    );
    assert!(clocks.t_self < clocks.t_owner);
    assert!(
        clocks.renew_interval <= Duration::from_secs(CLIENT_HEARTBEAT_INTERVAL_SECS),
        "the renewal cadence never regresses below the shipped 10 s beat"
    );
    assert!(
        clocks.renew_interval * 3 <= clocks.t_self,
        "a member gets at least three renewal attempts before its own deadline"
    );
}

/// The asymmetry's OPERATIONAL meaning: the client fail-stops the affected
/// objects itself, and only afterwards can the owner grant them elsewhere.
#[test]
fn the_client_fail_stops_before_the_owner_can_regrant() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let clocks = shipped_clocks();
    let owner = owner_with(clocks.clone(), clock.clone(), 5);

    let anchor = clock.now_ms();
    let grant = granted(owner.join(join_req("w-1", MemberRole::Writer, None)));
    let session = MemberSession::adopt("w-1", MemberRole::Writer, &grant, anchor, clock.clone());

    let self_deadline = session.t_self_deadline_ms();
    let owner_deadline = owner
        .lease_deadline_ms("w-1")
        .expect("the owner tracks the lease");
    assert!(
        self_deadline + 2 * clocks.skew_max.as_millis() as u64 + clocks.d_purge.as_millis() as u64
            <= owner_deadline,
        "the client deadline must precede the owner's by 2·skew_max + D_purge \
         (self {self_deadline} vs owner {owner_deadline})"
    );

    // One tick before its own deadline the member is still healthy.
    ticks.store(self_deadline - 1, Ordering::SeqCst);
    assert!(!session.self_fence_due());
    assert!(
        owner.expire_due().is_empty(),
        "the owner expires nothing yet"
    );

    // At its own deadline the member fail-stops ITSELF — before the owner
    // has expired anything, so no other holder can have been granted.
    ticks.store(self_deadline, Ordering::SeqCst);
    assert!(session.self_fence_due());
    let fenced_before = METRICS.membership_self_fences.load(Ordering::Relaxed);
    let fence = session.self_fence("renewal did not complete by T_self");
    assert!(fence.poisoned_data_custody, "a WRITER must stop its DMA");
    assert!(session.fenced());
    assert_eq!(
        METRICS.membership_self_fences.load(Ordering::Relaxed),
        fenced_before + 1
    );
    assert!(
        owner.expire_due().is_empty(),
        "the owner must still be holding the lease when the client fences itself"
    );

    // Only at the OWNER's deadline does the lease become grantable
    // elsewhere.
    ticks.store(owner_deadline, Ordering::SeqCst);
    let evictions = owner.expire_due();
    assert_eq!(evictions.len(), 1);
    assert_eq!(evictions[0].id, "w-1");
    squeezefs::data_custody::test_clear_poison();
}

/// A configuration where the client cannot be stricter is a **refusal**,
/// never a clamp: `2·skew_max + D_purge ≥ T_owner` means a member could
/// still believe it holds custody the owner has re-granted.
#[test]
fn an_unsafe_clock_configuration_refuses_loud() {
    let err = LeaseClocks::with_params(
        Duration::from_millis(400),
        Duration::from_millis(150),
        Duration::from_millis(200),
    )
    .expect_err("2·150 + 200 >= 400 must refuse");
    let msg = err.to_string();
    assert!(
        msg.contains("T_self") && msg.contains("skew"),
        "the refusal must name the collapsed inequality: {msg}"
    );
    // The boundary is exclusive: equality is still unsafe.
    assert!(LeaseClocks::with_params(
        Duration::from_millis(500),
        Duration::from_millis(150),
        Duration::from_millis(200),
    )
    .is_err());
    assert!(LeaseClocks::with_params(
        Duration::from_millis(5_000),
        Duration::from_millis(150),
        Duration::from_millis(200),
    )
    .is_ok());
}

// ---------------------------------------------------------------------------
// 5. Lease expiry → dead epoch → S7 quarantine
// ---------------------------------------------------------------------------

/// §6.7 "Recovery", client-failure half: the owner's TTL fires, the client
/// epoch is marked dead, and blocks allocated under it enter the S7
/// do-not-reallocate quarantine until the epoch is proven drained.
#[tokio::test]
async fn lease_expiry_declares_a_dead_epoch_that_quarantines_offsets() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let clocks = shipped_clocks();
    let owner = owner_with(clocks.clone(), clock.clone(), 6);
    let grant = granted(owner.join(join_req("w-dead", MemberRole::Writer, None)));

    let alloc = BlockAllocator::new("s6-dead-epoch")
        .await
        .expect("allocator");
    let victim = alloc
        .allocate_block()
        .await
        .expect("a fresh offset for the dying member");
    alloc.free_block(victim).await.expect("terminal free");

    ticks.store(
        clock.now_ms() + clocks.t_owner.as_millis() as u64 + 1,
        Ordering::SeqCst,
    );
    let evictions = owner.expire_due();
    assert_eq!(evictions.len(), 1, "an expired lease is evicted");
    let ev = &evictions[0];
    assert_eq!(ev.epoch, grant.epoch);

    // The composition: the eviction's DeadEpoch is what S7 quarantines on.
    let admitted = squeezefs::data_custody::quarantine_offsets(&alloc, [victim], ev.dead);
    assert_eq!(admitted, 1);
    assert!(
        alloc.is_quarantined(victim),
        "a dead epoch's offset must not be reallocatable"
    );

    // Only a drain proof releases it.
    let released = alloc.release_quarantine(ev.dead);
    assert_eq!(released, 1);
    assert!(!alloc.is_quarantined(victim));
}

// ---------------------------------------------------------------------------
// 6. §6.2 item 7 — the claim-set record
// ---------------------------------------------------------------------------

/// Un-engaged (every volume today, ruling D9): NO `claim_set` key is
/// written, `writer_claim`'s bytes are untouched, and consumers still read
/// ONE shape because the un-engaged claim set is a *projection* of the
/// singular claim.
#[tokio::test]
async fn claim_set_membership_keeps_single_writer_byte_identical() {
    let _serial = serial();
    let meta = formatted_volume().await;
    let be = KvMetaBackend::open(meta.path()).await.expect("open volume");
    let claim = WriterClaim {
        id: "writer-solo".into(),
        ts: now_secs(),
        pid: std::process::id(),
        boot: "boot-test".into(),
        term: 3,
    };
    let encoded = claim.encode();
    be.setxattr_internal(1, WRITER_CLAIM_XATTR, &encoded)
        .await
        .expect("the gate's claim commit");

    assert!(
        !membership::claim_set_engaged(be.superblock().features_incompat),
        "a production format never stamps bit 14"
    );
    let set = ClaimSet::load(&be)
        .await
        .expect("the projection must always answer");
    assert!(!set.durable, "un-engaged sets are projected, not stored");
    assert_eq!(set.members.len(), 1);
    assert_eq!(set.members[0].identity.id, "writer-solo");
    assert_eq!(set.members[0].identity.role, MemberRole::Writer);
    assert_eq!(set.term, 3);
    assert!(set.registrant_keys().is_empty());

    // Storing is REFUSED on an un-engaged volume: the byte-identity law is
    // enforced, not merely intended.
    let refused = ClaimSet::store(&be, &set)
        .await
        .expect_err("storing without bit 14 must refuse");
    assert!(refused.to_string().contains("bit 14"));

    let keys = be.listxattr(1).await.expect("listxattr");
    assert!(
        !keys.iter().any(|k| k == CLAIM_SET_XATTR),
        "no claim_set record may exist on an un-stamped volume: {keys:?}"
    );
    assert_eq!(
        be.getxattr(1, WRITER_CLAIM_XATTR)
            .await
            .expect("read the claim")
            .as_deref(),
        Some(encoded.as_slice()),
        "writer_claim bytes must be byte-identical to the pre-S6 encoding"
    );
    be.shutdown().await.expect("clean shutdown");
}

/// Engaged (the Phase-8 window's posture, reachable here through the
/// stamping path): multiple writers are durable members of one set, their
/// NVMe registrant keys travel with them, and the record round-trips.
#[tokio::test]
async fn an_engaged_claim_set_records_every_writer_and_its_registrant_key() {
    let _serial = serial();
    let meta = formatted_volume().await;
    assert!(
        sb::set_claim_set_bit(meta.path())
            .await
            .expect("stamp bit 14 offline"),
        "the bit must be newly set"
    );
    let be = KvMetaBackend::open(meta.path()).await.expect("open volume");
    assert!(membership::claim_set_engaged(
        be.superblock().features_incompat
    ));

    let mut set = ClaimSet::empty(11);
    set.members.push(ClaimSetMember {
        identity: membership::MemberIdentity {
            id: "writer-a".into(),
            role: MemberRole::Writer,
            pid: 10,
            boot: "boot-a".into(),
            endpoint: Some("10.0.0.1:7100".into()),
            pr_key: 0xdead_beef,
        },
        ts: now_secs(),
    });
    set.members.push(ClaimSetMember {
        identity: membership::MemberIdentity {
            id: "writer-b".into(),
            role: MemberRole::Writer,
            pid: 11,
            boot: "boot-b".into(),
            endpoint: Some("10.0.0.2:7100".into()),
            pr_key: 0x1234,
        },
        ts: now_secs(),
    });
    ClaimSet::store(&be, &set).await.expect("engaged store");

    let read = ClaimSet::load(&be).await.expect("durable record");
    assert!(read.durable);
    assert_eq!(read.term, 11);
    assert_eq!(read.members.len(), 2);
    let mut keys = read.registrant_keys();
    keys.sort_unstable();
    assert_eq!(keys, vec![0x1234, 0xdead_beef]);
    assert_eq!(
        ClaimSet::decode(&set.encode()).expect("round trip").members[1]
            .identity
            .endpoint
            .as_deref(),
        Some("10.0.0.2:7100")
    );
    be.shutdown().await.expect("clean shutdown");
}

/// The live membership half of item 7: a writer joins the SET at arm and
/// withdraws at disarm, its **NVMe registrant key** travels with its entry
/// (the device-side face of set membership), and every sibling's entry
/// survives the change. On an un-engaged volume both calls write NOTHING —
/// the byte-identity law enforced at the one place a mount would otherwise
/// have created a record.
#[tokio::test]
async fn claim_set_membership_upserts_and_withdraws_one_member_at_a_time() {
    let _serial = serial();
    // Un-engaged first: both operations are silent no-ops.
    let plain = formatted_volume().await;
    let be = KvMetaBackend::open(plain.path())
        .await
        .expect("open volume");
    let ident = |id: &str, key: u64| membership::MemberIdentity {
        id: id.to_string(),
        role: MemberRole::Writer,
        pid: 7,
        boot: "boot-test".into(),
        endpoint: Some("10.0.0.7:7100".into()),
        pr_key: key,
    };
    assert!(
        !membership::upsert_writer_member(&be, &ident("w-1", 1), 4)
            .await
            .expect("un-engaged upsert must not error"),
        "an un-stamped volume must record NOTHING (ruling D9 + byte identity)"
    );
    assert!(!membership::withdraw_writer_member(&be, "w-1")
        .await
        .expect("un-engaged withdraw"));
    assert!(!be
        .listxattr(1)
        .await
        .expect("listxattr")
        .iter()
        .any(|k| k == CLAIM_SET_XATTR));
    be.shutdown().await.expect("clean shutdown");

    // Engaged: two writers, each with its own registrant key.
    let meta = formatted_volume().await;
    sb::set_claim_set_bit(meta.path())
        .await
        .expect("stamp bit 14");
    let be = KvMetaBackend::open(meta.path()).await.expect("open volume");
    assert!(
        membership::upsert_writer_member(&be, &ident("w-a", 0xaa), 5)
            .await
            .expect("engaged upsert")
    );
    assert!(
        membership::upsert_writer_member(&be, &ident("w-b", 0xbb), 6)
            .await
            .expect("engaged upsert")
    );
    let set = ClaimSet::load(&be).await.expect("durable set");
    assert!(set.durable);
    assert_eq!(set.term, 6, "the set's era climbs with its members'");
    assert_eq!(set.members.len(), 2, "the sibling entry survived");
    let mut keys = set.registrant_keys();
    keys.sort_unstable();
    assert_eq!(keys, vec![0xaa, 0xbb]);

    // An upsert of an EXISTING member replaces its entry, never duplicates.
    assert!(
        membership::upsert_writer_member(&be, &ident("w-a", 0xcc), 6)
            .await
            .expect("re-upsert")
    );
    let set = ClaimSet::load(&be).await.expect("durable set");
    assert_eq!(set.members.len(), 2);
    assert_eq!(
        set.members
            .iter()
            .find(|m| m.identity.id == "w-a")
            .expect("w-a")
            .identity
            .pr_key,
        0xcc
    );

    // Withdrawal removes one member; the record disappears when the last
    // one leaves, so a departed set presents as unclaimed.
    assert!(membership::withdraw_writer_member(&be, "w-a")
        .await
        .expect("withdraw"));
    assert!(!membership::withdraw_writer_member(&be, "w-a")
        .await
        .expect("second withdraw is a no-op"));
    assert_eq!(
        ClaimSet::load(&be).await.expect("set").members.len(),
        1,
        "w-b is still a member"
    );
    assert!(membership::withdraw_writer_member(&be, "w-b")
        .await
        .expect("withdraw the last member"));
    assert!(
        !be.listxattr(1)
            .await
            .expect("listxattr")
            .iter()
            .any(|k| k == CLAIM_SET_XATTR),
        "the record is deleted when the set empties"
    );
    be.shutdown().await.expect("clean shutdown");
}

/// Bit 14 is single-bit, disjoint, and never stamped by a production
/// format (ruling D9). The union clause lives in the two existing pins;
/// this one is the local sanity face.
///
/// **Bit 14, not 12**: this stage authored itself at 12 in parallel with the
/// §6.2 items-5/6 branch, which took 12 and 13 — the fourth parallel claim in
/// this program, renumbered at integration.
#[test]
fn incompat_bit_14_is_disjoint_and_never_stamped_by_format() {
    assert_eq!(sb::FEATURE_INCOMPAT_KV_CLAIM_SET.count_ones(), 1);
    assert_eq!(sb::FEATURE_INCOMPAT_KV_CLAIM_SET, 1 << 14);
    for (name, bit) in [
        (
            "KV_MULTI_WRITER_DATA",
            sb::FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA,
        ),
        ("KV_INO_LANES", sb::FEATURE_INCOMPAT_KV_INO_LANES),
        (
            "KV_BLOCK_KEY_INCARNATION",
            sb::FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION,
        ),
        // §6.2 item 9 (durable per-ino layout versions) took bit 15 in a
        // parallel window with this stage's bit — listed so a renumber of
        // either side turns THIS pin red instead of aliasing on disk.
        (
            "KV_LAYOUT_VERSIONS",
            sb::FEATURE_INCOMPAT_KV_LAYOUT_VERSIONS,
        ),
    ] {
        assert_eq!(
            sb::FEATURE_INCOMPAT_KV_CLAIM_SET & bit,
            0,
            "the claim-set bit collides with {name}"
        );
    }
    assert_ne!(
        sb::FEATURES_INCOMPAT_KNOWN & sb::FEATURE_INCOMPAT_KV_CLAIM_SET,
        0,
        "this binary must UNDERSTAND bit 14"
    );
    let plan = sb::SuperblockV3::plan(1 << 30, 262_144, None, [3u8; 16], 0x99).expect("plan");
    assert_eq!(
        plan.features_incompat & sb::FEATURE_INCOMPAT_KV_CLAIM_SET,
        0,
        "ruling D9: build the bit, never stamp it at format"
    );
}

// ---------------------------------------------------------------------------
// 7. Owner failure → re-assertion + grace window
// ---------------------------------------------------------------------------

/// §6.7 "Recovery", owner-failure half: the successor bumps `term`
/// durably BEFORE arming. An owner that cannot present a strictly greater
/// term is refused — that refusal IS the ordering law in code.
#[test]
fn a_successor_that_did_not_bump_the_term_cannot_arm() {
    let (clock, _ticks) = manual_clock();
    let err = MembershipOwner::arm("owner-2", 7, 7, shipped_clocks(), clock.clone())
        .expect_err("equal term must refuse");
    assert!(
        err.to_string().contains("term"),
        "the refusal must name the term: {err}"
    );
    assert!(MembershipOwner::arm("owner-2", 8, 7, shipped_clocks(), clock).is_ok());
}

/// The grace window: reclaim admitted, conflicting fresh acquires refused
/// with a retry-after, and the window closes when the prior membership has
/// re-asserted. "Without that window, failover triggers a cluster-wide
/// forced-flush storm at the worst possible moment."
#[test]
fn owner_failover_opens_a_grace_window_admitting_only_reclaim() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let clocks = shipped_clocks();
    let owner = owner_with(clocks.clone(), clock.clone(), 12);
    owner.open_grace(vec!["m-1".to_string(), "m-2".to_string()]);
    assert!(owner.grace_active());
    assert!(owner.grace_remaining_ms() > 0);

    // A conflicting FRESH acquire is refused, with an honest retry-after.
    let refusals_before = METRICS.membership_grace_refusals.load(Ordering::Relaxed);
    match owner.join(join_req("stranger", MemberRole::Writer, None)) {
        JoinOutcome::Refused {
            reason,
            retry_after_ms,
        } => {
            assert!(reason.contains("grace"), "reason: {reason}");
            assert!(retry_after_ms > 0);
        }
        JoinOutcome::Granted(_) => panic!("a fresh acquire must not be granted during grace"),
        JoinOutcome::UnknownLease { reason } => {
            panic!("a FRESH acquire is the grace REFUSAL class, never UnknownLease: {reason}")
        }
    }
    assert_eq!(
        METRICS.membership_grace_refusals.load(Ordering::Relaxed),
        refusals_before + 1
    );

    // Reclaim — a member presenting its prior epoch — IS admitted.
    let reclaims_before = METRICS.membership_grace_reclaims.load(Ordering::Relaxed);
    let mut r1 = join_req("m-1", MemberRole::Writer, None);
    r1.prior_epoch = Some(41);
    granted(owner.join(r1));
    assert!(
        owner.grace_active(),
        "one of two reclaims does not close it"
    );
    let mut r2 = join_req("m-2", MemberRole::Reader, None);
    r2.prior_epoch = Some(42);
    granted(owner.join(r2));
    assert_eq!(
        METRICS.membership_grace_reclaims.load(Ordering::Relaxed),
        reclaims_before + 2
    );
    assert!(
        !owner.grace_active(),
        "the window closes as soon as the prior membership has re-asserted"
    );

    // Post-grace, fresh acquires are admitted again.
    granted(owner.join(join_req("stranger", MemberRole::Writer, None)));

    // And a window nobody re-asserts into closes on its own deadline.
    let owner2 = owner_with(clocks.clone(), clock.clone(), 13);
    owner2.open_grace(vec!["ghost".to_string()]);
    assert!(owner2.grace_active());
    ticks.store(
        clock.now_ms() + clocks.grace.as_millis() as u64 + 1,
        Ordering::SeqCst,
    );
    assert!(!owner2.grace_active(), "the grace window is bounded");
}

// ---------------------------------------------------------------------------
// 8. DISC-1 over the new plane
// ---------------------------------------------------------------------------

/// The projection law must be IDENTICAL to
/// `cluster_wire::peers_from_registrations`: fresh only, endpoint
/// required, self excluded, deduped, ordered by id.
#[test]
fn disc1_discovery_rides_the_membership_census() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let clocks = shipped_clocks();
    let owner = owner_with(clocks.clone(), clock.clone(), 4);
    granted(owner.join(join_req(
        "b-peer",
        MemberRole::Writer,
        Some("10.0.0.2:7100"),
    )));
    granted(owner.join(join_req(
        "a-peer",
        MemberRole::Writer,
        Some("10.0.0.1:7100"),
    )));
    granted(owner.join(join_req("self", MemberRole::Writer, Some("10.0.0.3:7100"))));
    granted(owner.join(join_req("no-endpoint", MemberRole::Reader, None)));
    let stale = granted(owner.join(join_req("stale", MemberRole::Writer, Some("10.0.0.4:7100"))));
    assert!(stale.epoch > 0);

    // Age the whole census past the owner TTL, then renew everyone except
    // `stale` so exactly one member is not fresh.
    let t0 = clock.now_ms();
    ticks.store(t0 + clocks.t_owner.as_millis() as u64 - 1, Ordering::SeqCst);
    for id in ["b-peer", "a-peer", "self", "no-endpoint"] {
        let epoch = owner.epoch_of(id).expect("live member");
        assert!(matches!(
            owner.renew(id, epoch, 0),
            RenewOutcome::Renewed(_)
        ));
    }
    ticks.store(t0 + clocks.t_owner.as_millis() as u64 + 1, Ordering::SeqCst);

    let (rows, _) = owner.census(0, 64);
    let peers = peers_from_census(&rows, Some("self"));
    assert_eq!(
        peers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        vec!["a-peer", "b-peer"],
        "fresh + endpoint-bearing + self-excluded, ordered by id: {peers:?}"
    );
    assert!(peers.iter().all(|p| p.fresh));
    assert_eq!(peers[0].endpoint, "10.0.0.1:7100");
}

// ---------------------------------------------------------------------------
// 9. The wire: join / renew / census / leave over cluster_wire
// ---------------------------------------------------------------------------

/// The plane rides the ONE cluster transport (S3): storage-trust mutual
/// authentication, per-frame session MAC, pinned service lanes — and the
/// verbs still cost zero journal transactions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn membership_verbs_round_trip_over_cluster_wire() {
    let _serial = serial();
    let secret = b"s6-storage-trust-secret".to_vec();
    let (clock, _ticks) = manual_clock();
    let owner = owner_with(shipped_clocks(), clock, 5);
    let plane = MembershipPlane::start(
        MembershipPlaneConfig::loopback(),
        secret.clone(),
        Arc::clone(&owner),
    )
    .expect("the plane must bind a loopback listener");
    let endpoint = plane.endpoint().to_string();

    let journal_before = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    let mut client = MemberClient::join(
        &endpoint,
        &secret,
        join_req("wire-reader", MemberRole::Reader, None),
        LeaseClock::monotonic(),
    )
    .await
    .expect("join over the wire");
    assert!(client.authenticated(), "sessions are authenticated by S3");
    client.renew().await.expect("renew over the wire");
    let (rows, next) = client.census(0, 64).await.expect("census over the wire");
    assert!(next.is_none());
    assert!(rows.iter().any(|r| r.id == "wire-reader"));
    assert_eq!(
        META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed),
        journal_before,
        "the whole wire round trip must be journal-free"
    );
    client.leave().await.expect("leave over the wire");
    assert_eq!(owner.len(), 0, "a clean leave removes the member");
    plane.shutdown();
}

/// An unknown lease (evicted, or a survivor of an owner restart) is
/// refused with its own status so the member self-fences and re-joins
/// rather than believing it still holds custody.
#[test]
fn renewing_an_unknown_lease_is_refused_with_its_own_status() {
    let _serial = serial();
    let (clock, _ticks) = manual_clock();
    let owner = owner_with(shipped_clocks(), clock, 5);
    let grant = granted(owner.join(join_req("gone", MemberRole::Reader, None)));
    owner.evict("gone", "test").expect("evict");
    assert!(matches!(
        owner.renew("gone", grant.epoch, 0),
        RenewOutcome::UnknownLease { .. }
    ));
    // A stale EPOCH on a live member is equally unknown (the owner
    // restarted and re-granted; the old epoch is not custody).
    let fresh = granted(owner.join(join_req("gone", MemberRole::Reader, None)));
    assert!(matches!(
        owner.renew("gone", fresh.epoch - 1, 0),
        RenewOutcome::UnknownLease { .. }
    ));
    assert_ne!(RPC_MEMBERSHIP_UNKNOWN_LEASE, RPC_MEMBERSHIP_REFUSED);
}

/// The router is ADDITIVE: membership registers beside whatever else the
/// program puts on this transport (S4's lock verbs, S8's metadata verbs),
/// and an unclaimed verb still answers `RPC_UNKNOWN_VERB`.
#[test]
fn the_verb_router_composes_services_additively() {
    let _serial = serial();
    let (clock, _ticks) = manual_clock();
    let owner = owner_with(shipped_clocks(), clock, 5);
    let router = VerbRouter::new()
        .with(
            squeezefs::cluster_wire::VERB_PING,
            squeezefs::cluster_wire::VERB_PING,
            Arc::new(squeezefs::cluster_wire::PingService),
        )
        .with_membership(Arc::clone(&owner));
    let svc: Arc<dyn squeezefs::cluster_wire::RpcService> = Arc::new(router);

    let ping = svc.call(squeezefs::cluster_wire::RpcRequest {
        id: 1,
        verb: squeezefs::cluster_wire::VERB_PING,
        body: b"hi".to_vec(),
    });
    assert_eq!(ping.status, squeezefs::cluster_wire::RPC_OK);
    assert_eq!(ping.body, b"hi");

    let census = svc.call(squeezefs::cluster_wire::RpcRequest {
        id: 2,
        verb: VERB_MEMBERSHIP_CENSUS,
        body: membership_wire_census_body(),
    });
    assert_eq!(census.status, squeezefs::cluster_wire::RPC_OK);

    let unknown = svc.call(squeezefs::cluster_wire::RpcRequest {
        id: 3,
        verb: 0xfffe,
        body: Vec::new(),
    });
    assert_eq!(
        unknown.status,
        squeezefs::cluster_wire::RPC_UNKNOWN_VERB,
        "an unclaimed verb must not be silently answered OK"
    );
}

fn membership_wire_census_body() -> Vec<u8> {
    squeezefs::membership_wire::encode_census_request(0, 64).expect("encode a census request")
}

// ---------------------------------------------------------------------------
// 10. Concurrency + the D1 simulated-client harness
// ---------------------------------------------------------------------------

/// Many members renewing concurrently while one is evicted: no live
/// member's renewal is refused, the evicted one's is, and the census stays
/// internally consistent throughout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_clients_renew_while_one_is_evicted() {
    let _serial = serial();
    let (clock, _ticks) = manual_clock();
    let owner = owner_with(shipped_clocks(), clock, 7);
    let members: Vec<(String, u64)> = (0..256)
        .map(|i| {
            let id = format!("c{i}");
            let g = granted(owner.join(join_req(&id, MemberRole::Reader, None)));
            (id, g.epoch)
        })
        .collect();

    let victim = members[13].clone();
    let barrier = Arc::new(tokio::sync::Barrier::new(9));
    // The renewal loops run until the evictor has been OBSERVED, so the
    // contract is deterministic rather than a race between task schedules
    // (the first shape of this test finished its rounds before the evictor
    // ran, which proved nothing).
    let stop = Arc::new(AtomicBool::new(false));
    let refusals = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for chunk in members.chunks(32) {
        let chunk: Vec<(String, u64)> = chunk.to_vec();
        let owner = Arc::clone(&owner);
        let barrier = Arc::clone(&barrier);
        let stop = Arc::clone(&stop);
        let refusals = Arc::clone(&refusals);
        let victim_id = victim.0.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let mut refused = 0usize;
            while !stop.load(Ordering::Acquire) {
                for (id, epoch) in &chunk {
                    match owner.renew(id, *epoch, 1) {
                        RenewOutcome::Renewed(_) => {}
                        RenewOutcome::UnknownLease { .. } => {
                            assert_eq!(
                                id, &victim_id,
                                "only the evicted member may be refused ({id})"
                            );
                            refused += 1;
                            refusals.fetch_add(1, Ordering::AcqRel);
                        }
                    }
                }
                // The census must never observe a torn member.
                let (rows, _) = owner.census(0, 512);
                assert!(rows.iter().all(|r| !r.id.is_empty()));
                tokio::task::yield_now().await;
            }
            refused
        }));
    }
    let evictor = {
        let owner = Arc::clone(&owner);
        let barrier = Arc::clone(&barrier);
        let stop = Arc::clone(&stop);
        let refusals = Arc::clone(&refusals);
        let victim_id = victim.0.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            let landed = owner.evict(&victim_id, "concurrency contract").is_some();
            // Bounded wait for the renewers to observe it — then stop them.
            for _ in 0..100_000 {
                if refusals.load(Ordering::Acquire) > 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
            stop.store(true, Ordering::Release);
            landed
        })
    };
    let mut total_refused = 0usize;
    for t in tasks {
        total_refused += t.await.expect("renewal task");
    }
    assert!(evictor.await.expect("evictor task"), "the eviction landed");
    assert!(
        total_refused > 0,
        "the evicted member must be refused at least once"
    );
    assert_eq!(owner.len(), 255, "exactly one member left the census");
    assert!(owner.epoch_of(&victim.0).is_none());
}

/// The **D1 harness** (validation tier (ii)) at small scale: it must drive
/// membership, heartbeat, lease and revoke planes end to end, report the
/// four figures the deferred 15 k row publishes, and cost zero journal
/// transactions doing it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_simulated_client_harness_produces_the_deferred_rows_shape() {
    let _serial = serial();
    let report = squeezefs::membership_sim::run(SimConfig {
        clients: 300,
        readers_pct: 80,
        beats: 4,
        mode: SimMode::Direct,
        evict_fraction_permille: 10,
        failover: true,
    })
    .await
    .expect("the harness must run");

    assert_eq!(report.clients, 300);
    assert_eq!(
        report.journal_entries_delta, 0,
        "the whole membership plane is journal-free — that IS the S6 gate"
    );
    assert_eq!(report.renewals, 300 * 4);
    assert!(report.renew_p99_us >= report.renew_p50_us);
    assert!(report.evictions >= 3, "1 % of 300 members");
    assert!(
        report.revoke_fanout_us > 0.0,
        "the revoke fan-out must be measured"
    );
    assert!(
        report.grace_completion_us > 0.0,
        "the failover leg must measure grace completion"
    );
    let row = report.render();
    for field in [
        "volume-0 journal tx/s",
        "renewal latency",
        "revoke fan-out",
        "grace completion",
    ] {
        assert!(row.contains(field), "the row must name {field}:\n{row}");
    }
}

/// The harness's WIRED mode proves the same planes over the real
/// transport (small N — the wire is the thing under test, not the scale).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_harness_wired_mode_drives_the_real_transport() {
    let _serial = serial();
    let report = squeezefs::membership_sim::run(SimConfig {
        clients: 8,
        readers_pct: 50,
        beats: 3,
        mode: SimMode::Wired,
        evict_fraction_permille: 0,
        failover: false,
    })
    .await
    .expect("the wired harness must run");
    assert_eq!(report.clients, 8);
    assert_eq!(report.renewals, 24);
    assert_eq!(report.journal_entries_delta, 0);
    assert!(report.renew_p50_us > 0.0, "the wire has a real latency");
}

// ---------------------------------------------------------------------------
// KD-MW-2 (design-full-multi-writer §5.1) — the client-identity pair
// grammar on the roster/census, and OQ-5's resolved form
// ---------------------------------------------------------------------------

/// The §11 roster grammar: entries name the pair `node_{16 hex}.m{8 hex}`
/// exactly, or the bare `node_{16 hex}` form as a SLOT WILDCARD matching
/// every mount slot of that node. Non-node ids (owner uuids, claim uuids)
/// match only exactly, and a bare entry never prefix-matches a LONGER
/// node token.
#[test]
fn member_id_grammar_matches_pairs_and_bare_node_wildcards() {
    use membership::member_id_matches;
    let bare = "node_00000000deadbeef";
    let pair_a = "node_00000000deadbeef.m00c0ffee";
    let pair_b = "node_00000000deadbeef.m0badc0de";

    assert!(member_id_matches(pair_a, pair_a), "exact pair");
    assert!(
        !member_id_matches(pair_a, pair_b),
        "a pair entry is slot-exact"
    );
    assert!(member_id_matches(bare, bare), "exact bare");
    assert!(
        member_id_matches(bare, pair_a) && member_id_matches(bare, pair_b),
        "a bare entry is the slot wildcard"
    );
    assert!(
        !member_id_matches(bare, "node_00000000deadbeef00.m00c0ffee"),
        "a bare entry never prefix-matches a longer token"
    );
    assert!(
        !member_id_matches("node_1111111111111111", pair_a),
        "a different node never matches"
    );
    assert!(
        !member_id_matches(pair_a, bare),
        "a pair entry does not match the bare id (an offline process is \
         not a mount)"
    );
    let uuid = "0e2a1a4e-9c1e-4a5f-8a2e-000000000001";
    assert!(member_id_matches(uuid, uuid));
    assert!(!member_id_matches(uuid, "some-other-id"));
}

/// OQ-5's resolved form (rung-1 red-first pin, driven by the explicit
/// `client_slot` override shape): an observed mount-slot collision within
/// one claim set's membership plane REFUSES loud, naming BOTH colliding
/// mount points and the `-o client_slot=` remedy — while a re-join from
/// the SAME mount point (crash successor) stays a replace, and joiners
/// that report no mount point are never guessed about.
#[test]
fn a_mount_slot_collision_within_one_claim_set_refuses_loud() {
    let _s = serial();
    let owner = owner_with(shipped_clocks(), LeaseClock::monotonic(), 7);
    let id = "node_00000000deadbeef.m00c0ffee";

    let mut req_a = join_req(id, MemberRole::Writer, None);
    req_a.mount = Some("/mnt/train-a".to_string());
    let _grant_a = granted(owner.join(req_a));

    // A DIFFERENT mount point presenting the SAME identity: refused,
    // naming both mount points and the remedy.
    let mut req_b = join_req(id, MemberRole::Writer, None);
    req_b.mount = Some("/mnt/train-b".to_string());
    req_b.pid = std::process::id().wrapping_add(1);
    match owner.join(req_b) {
        JoinOutcome::Refused { reason, .. } => {
            assert!(reason.contains("MOUNT-SLOT COLLISION"), "{reason}");
            assert!(
                reason.contains("/mnt/train-a") && reason.contains("/mnt/train-b"),
                "the refusal names BOTH colliding mount points: {reason}"
            );
            assert!(
                reason.contains("client_slot"),
                "the refusal names the -o client_slot remedy: {reason}"
            );
        }
        JoinOutcome::Granted(_) => {
            panic!("two mount points must never share one client identity")
        }
        JoinOutcome::UnknownLease { reason } => {
            panic!("a slot collision is the REFUSAL class, never UnknownLease: {reason}")
        }
    }

    // The census still carries the LIVE member — with its mount point
    // (what `squeezefs clients` renders so the operator can see who holds
    // the identity).
    let (rows, _) = owner.census(0, 16);
    let row = rows.iter().find(|r| r.id == id).expect("member listed");
    assert_eq!(row.mount.as_deref(), Some("/mnt/train-a"));

    // The SAME mount point re-joining (a crash successor at the same
    // path) is the continuity law: a replace, never a collision.
    let mut req_a2 = join_req(id, MemberRole::Writer, None);
    req_a2.mount = Some("/mnt/train-a".to_string());
    req_a2.pid = std::process::id().wrapping_add(2);
    let _grant = granted(owner.join(req_a2));

    // A joiner with NO mount point recorded is never guessed about: the
    // collision check needs an observation, not an inference.
    let anon = join_req(id, MemberRole::Writer, None);
    let _grant = granted(owner.join(anon));
}

// ---------------------------------------------------------------------------
// 11. Rung 7 (arm S6) — the S6-b′ hung-kernel finding's repro-ports
// ---------------------------------------------------------------------------
//
// The finding (2026-08-16, found building the S6-b′ VM-pause row): a member
// whose KERNEL froze past the owner's TTL (qemu pause — the shape kill-9
// cannot produce) resumes with its MONOTONIC CLOCK never having advanced,
// so `self_fence_due()` is false; its failed renewal then re-joined as a
// RECLAIM, and the owner GRANTED a silently-fresh lease to an evicted
// member — which resumed serving from its pre-freeze caches as a live
// member, never fencing, never purging. That is verbatim the design row's
// falsifier ("victim's frozen-then-thawed writes land after fence" /
// "resuming as a live member") and contradicts the wire's own contract:
// `RPC_MEMBERSHIP_UNKNOWN_LEASE — the member's correct response is
// self-fence then re-join, not retry`.

/// The OWNER half: a reclaim presenting an epoch this owner does not hold
/// (evicted, swept, or a predecessor's) must NOT be granted as a silent
/// fresh lease outside a grace window — the member has to be TOLD its
/// lease is not custody so it can fence first.
#[test]
fn an_evicted_reclaim_is_not_granted_as_a_silent_fresh_lease() {
    let _serial = serial();
    let (clock, _ticks) = manual_clock();
    let owner = owner_with(shipped_clocks(), clock, 5);
    let grant = granted(owner.join(join_req("thawed", MemberRole::Reader, None)));
    owner
        .evict("thawed", "paused past TTL (test)")
        .expect("evict");

    let mut reclaim = join_req("thawed", MemberRole::Reader, None);
    reclaim.prior_epoch = Some(grant.epoch);
    let out = owner.join(reclaim);
    assert!(
        !matches!(out, JoinOutcome::Granted(_)),
        "a dead reclaim must not be granted as a silent fresh lease — the member must \
         learn its prior lease is not custody (got {out:?})"
    );
    match out {
        JoinOutcome::UnknownLease { reason } => {
            assert!(
                reason.contains("self-fence"),
                "the refusal states the member's correct response: {reason}"
            );
        }
        other => panic!("a dead reclaim answers the UnknownLease class, got {other:?}"),
    }

    // A FRESH join by the same identity stays admissible (the post-fence
    // re-join): the class is about the dead EPOCH, not the member.
    let _fresh = granted(owner.join(join_req("thawed", MemberRole::Reader, None)));
}

/// The MEMBER half, driving the REAL renewal tick: a member with a FROZEN
/// monotonic clock (the hung-kernel/VM-pause shape — `self_fence_due()`
/// can never fire) whose lease the owner evicted must SELF-FENCE and run
/// its purge BEFORE it holds any fresh lease — never resume as a live
/// member on its stale caches.
#[tokio::test]
async fn a_frozen_member_whose_lease_died_fences_and_purges_before_rejoining() {
    let _serial = serial();
    let secret = b"s6-b-prime-secret".to_vec();
    let (owner_clock, _oticks) = manual_clock();
    let owner = owner_with(shipped_clocks(), owner_clock, 5);
    let plane = MembershipPlane::start(
        MembershipPlaneConfig::loopback(),
        secret.clone(),
        Arc::clone(&owner),
    )
    .expect("plane binds loopback");
    let endpoint = plane.endpoint().to_string();

    // The member's clock is FROZEN — a paused guest kernel's monotonic
    // domain. T_self can never be observed as passed.
    let (member_clock, _mticks) = manual_clock();
    let req = join_req("frozen-reader", MemberRole::Reader, None);
    let mut client = MemberClient::join(&endpoint, &secret, req.clone(), member_clock.clone())
        .await
        .expect("join");
    let paused_session = Arc::clone(client.session());
    let paused_epoch = paused_session.epoch();
    assert!(
        !paused_session.self_fence_due(),
        "the frozen clock must make T_self unobservable — that is the row's shape"
    );

    let purges = Arc::new(AtomicU64::new(0));
    let on_purge: Arc<dyn Fn() + Send + Sync> = {
        let purges = Arc::clone(&purges);
        Arc::new(move || {
            purges.fetch_add(1, Ordering::SeqCst);
        })
    };
    let fences_before = METRICS.membership_self_fences.load(Ordering::Relaxed);

    // The owner expired the lease while the member was frozen.
    owner
        .evict(
            "frozen-reader",
            "lease TTL expired while the guest was paused (test)",
        )
        .expect("evict");

    let tick = membership::member_renewal_tick(
        &mut client,
        &endpoint,
        &secret,
        &req,
        &member_clock,
        Some(&on_purge),
    )
    .await;

    assert_eq!(
        METRICS.membership_self_fences.load(Ordering::Relaxed) - fences_before,
        1,
        "the member must self-fence exactly once before resuming (tick answered {tick:?})"
    );
    assert!(
        paused_session.fenced(),
        "the paused-era session must be FENCED — it is the view whose caches are stale"
    );
    assert_eq!(
        purges.load(Ordering::SeqCst),
        1,
        "the reader's purge (drop every cached block) must run BEFORE any fresh lease serves"
    );
    assert_eq!(
        tick,
        membership::RenewalTick::FencedAndRejoined,
        "the correct response is self-fence then re-join (the wire's own contract), \
         never a silent resume as a live member"
    );
    assert!(
        !client.session().fenced(),
        "the post-fence session is a FRESH, clean-view member"
    );
    assert_ne!(
        client.session().epoch(),
        paused_epoch,
        "the fresh lease is a new epoch — the dead one is never resurrected"
    );
    plane.shutdown();
}

/// The failover pin the fix must NOT break: a reclaim admitted by a
/// successor's GRACE WINDOW is continuity — the member re-asserts the
/// lease it already held and keeps its caches; forcing every member to
/// Rung-8 finding #3 (found live by the S7-b kill-matrix row, 2026-08-16):
/// **every crashed authority incarnation accreted a phantom durable
/// claim-set writer.** `arm_owner` keyed the §6.2 item-7 claim-set entry
/// on its per-mount random uuid; kill -9 never withdraws; the successor of
/// the SAME node re-upserted under a fresh uuid and `upsert_writer_member`
/// retained the dead predecessor. Observed live: member count 1 → 2 → 3
/// across two kill-9s, the allocation-partition width marching W=1 → 2 →
/// 4 with lanes reserved for dead uuids, phantom grace windows opened over
/// the authority's own dead predecessors, and the raised reservation
/// frontier read by fsck C6 as capacity-census drift — `fsck_findings != 0`
/// on a healthy volume, and unbounded width decay under crash loops.
///
/// Two laws pinned:
/// 1. the authority's claim-set identity is the DURABLE KD-MW-2 node id
///    (`owner_claim_identity`), so a successor of the same node REPLACES
///    its predecessor's entry by id — the same identity co-writers already
///    enroll under;
/// 2. the upsert PRUNES same-host provably-dead writer entries (boot-id
///    match + `kill(pid,0) == ESRCH` — the D0 dead-holder proof), which
///    heals pre-fix accretion in the field; foreign-boot entries and
///    pid-less roster enrollments are NEVER pruned (liveness unknowable /
///    deliberately process-less).
#[tokio::test]
async fn a_successor_authoritys_claim_entry_replaces_its_dead_predecessor_never_accretes() {
    let _serial = serial();
    let meta = formatted_volume().await;
    sb::set_claim_set_bit(meta.path())
        .await
        .expect("stamp bit 14");
    let be = KvMetaBackend::open(meta.path()).await.expect("open volume");
    let my_boot = squeezefs::meta_backend::kv::backend::read_boot_id();

    // A provably-dead same-host pid: spawn-and-reap a child.
    let dead_pid = {
        let mut child = std::process::Command::new("true").spawn().expect("spawn");
        let pid = child.id();
        child.wait().expect("reap");
        pid
    };

    // The dead predecessor incarnation (the pre-fix uuid-keyed shape),
    // a FOREIGN-host entry (dead pid but a different boot — liveness
    // unknowable, never prunable), and a pid-less roster enrollment
    // (deliberately process-less, never prunable).
    let predecessor = squeezefs::membership::MemberIdentity {
        id: "uuid-dead-incarnation".to_string(),
        role: MemberRole::Writer,
        pid: dead_pid,
        boot: my_boot.clone(),
        endpoint: None,
        pr_key: 0x0dead,
    };
    let foreign = squeezefs::membership::MemberIdentity {
        id: "node_00000000000000ff".to_string(),
        role: MemberRole::Writer,
        pid: dead_pid,
        boot: "some-other-boot".to_string(),
        endpoint: None,
        pr_key: 0x0f0e,
    };
    let roster = squeezefs::membership::MemberIdentity {
        id: "node_00000000000000aa.m00000001".to_string(),
        role: MemberRole::Writer,
        pid: 0,
        boot: String::new(),
        endpoint: None,
        pr_key: 0,
    };
    assert!(membership::upsert_writer_member(&be, &predecessor, 3)
        .await
        .expect("seed predecessor"));
    assert!(membership::upsert_writer_member(&be, &foreign, 3)
        .await
        .expect("seed foreign"));
    assert!(membership::upsert_writer_member(&be, &roster, 3)
        .await
        .expect("seed roster"));

    // The SUCCESSOR arms (same node, new incarnation): its upsert must
    // prune the same-host provably-dead predecessor and keep the other two.
    let successor = squeezefs::membership::MemberIdentity {
        id: "node_0000000000000001".to_string(),
        role: MemberRole::Writer,
        pid: std::process::id(),
        boot: my_boot.clone(),
        endpoint: None,
        pr_key: 0x51,
    };
    assert!(membership::upsert_writer_member(&be, &successor, 4)
        .await
        .expect("successor upsert"));
    let set = ClaimSet::load(&be).await.expect("durable set");
    let ids: Vec<&str> = set.members.iter().map(|m| m.identity.id.as_str()).collect();
    assert!(
        !ids.contains(&"uuid-dead-incarnation"),
        "the same-host provably-dead predecessor must be PRUNED at the successor's \
         upsert (the accreting-phantom-writers finding): {ids:?}"
    );
    assert!(
        ids.contains(&"node_00000000000000ff"),
        "a foreign-boot entry is never prunable blind (liveness unknowable): {ids:?}"
    );
    assert!(
        ids.contains(&"node_00000000000000aa.m00000001"),
        "a pid-less roster enrollment is deliberately process-less — never pruned: {ids:?}"
    );
    assert_eq!(
        set.members.len(),
        3,
        "successor + foreign + roster — never the accreted 4: {ids:?}"
    );

    // A LIVE same-host sibling (our own pid) must survive an upsert too.
    assert!(membership::upsert_writer_member(&be, &successor, 4)
        .await
        .expect("idempotent re-upsert"));
    assert_eq!(
        ClaimSet::load(&be).await.expect("set").members.len(),
        3,
        "re-upsert replaces, never duplicates and never prunes the living"
    );
    be.shutdown().await.expect("clean shutdown");
}

/// The identity half of finding #3: the authority's claim-set identity is
/// the DURABLE node id (stable across incarnations — what makes
/// replace-by-id heal a crash), never the per-mount uuid.
#[test]
fn the_authoritys_claim_identity_is_durable_across_incarnations() {
    let a = membership::owner_claim_identity("uuid-incarnation-1");
    let b = membership::owner_claim_identity("uuid-incarnation-2");
    if a.starts_with("node_") {
        assert_eq!(
            a, b,
            "the claim identity must be STABLE across incarnations — a per-mount uuid \
             accretes one phantom writer per crash"
        );
    } else {
        // A box with no resolvable stable node identity falls back to the
        // incarnation id LOUDLY — accretion returns there, but a wrong
        // node token would misclassify staged custody, which is worse.
        assert_eq!(
            a, "uuid-incarnation-1",
            "the fallback is the incarnation id"
        );
        assert_eq!(b, "uuid-incarnation-2");
    }
}

/// Rung-8 finding #1 (found live by the S7-a device-fence row, 2026-08-16):
/// a READER member that self-fences **by its own deadline** (T_self passed
/// — the frozen/hung-OWNER shape, the exact dual of S6-b′'s frozen member)
/// purged correctly but `RenewalTick::Fenced` EXITED the renewal loop
/// permanently: the mount stayed fenced forever, `membership_mode` kept
/// reading `member` off the stranded session, and zero renewals ever
/// happened again — the fleet's reader was silently dead until remount.
/// Only the UnknownLease class got rung-7's fence-then-rejoin ladder. The
/// law is ONE law for every fence a purge can make clean: fence FIRST,
/// purge, then re-present FRESH (a dead lease is never resurrected).
#[tokio::test]
async fn a_reader_fenced_by_its_own_deadline_purges_and_rejoins_when_the_owner_answers() {
    let _serial = serial();
    let secret = b"s7-deadline-fence-secret".to_vec();
    let (owner_clock, _oticks) = manual_clock();
    let owner = owner_with(shipped_clocks(), owner_clock, 7);
    let plane = MembershipPlane::start(
        MembershipPlaneConfig::loopback(),
        secret.clone(),
        Arc::clone(&owner),
    )
    .expect("plane binds loopback");
    let endpoint = plane.endpoint().to_string();

    let (member_clock, mticks) = manual_clock();
    let req = join_req("deadline-reader", MemberRole::Reader, None);
    let mut client = MemberClient::join(&endpoint, &secret, req.clone(), member_clock.clone())
        .await
        .expect("join");
    let stranded_session = Arc::clone(client.session());
    let stranded_epoch = stranded_session.epoch();

    // The owner swept the lease while this member could not renew, and the
    // member's OWN clock has run past T_self (SIGSTOP leaves the clock
    // running; a hung owner leaves the member's renewals unanswered).
    owner
        .evict(
            "deadline-reader",
            "lease TTL expired while the owner was unreachable (test)",
        )
        .expect("evict");
    mticks.fetch_add(60_000, Ordering::SeqCst);
    assert!(
        stranded_session.self_fence_due(),
        "the shape under test IS the deadline arm — T_self must read as passed"
    );

    let purges = Arc::new(AtomicU64::new(0));
    let on_purge: Arc<dyn Fn() + Send + Sync> = {
        let purges = Arc::clone(&purges);
        Arc::new(move || {
            purges.fetch_add(1, Ordering::SeqCst);
        })
    };
    let fences_before = METRICS.membership_self_fences.load(Ordering::Relaxed);

    let tick = membership::member_renewal_tick(
        &mut client,
        &endpoint,
        &secret,
        &req,
        &member_clock,
        Some(&on_purge),
    )
    .await;

    assert_eq!(
        METRICS.membership_self_fences.load(Ordering::Relaxed) - fences_before,
        1,
        "the member must self-fence exactly once (tick answered {tick:?})"
    );
    assert!(stranded_session.fenced(), "the dead-era session is fenced");
    assert_eq!(
        purges.load(Ordering::SeqCst),
        1,
        "the reader's purge must run before any fresh lease serves"
    );
    assert_eq!(
        tick,
        membership::RenewalTick::FencedAndRejoined,
        "a PURGED reader's deadline fence must re-present FRESH — returning Fenced \
         exits the renewal loop and strands the mount fenced-forever (the rung-8 \
         S7-a live finding)"
    );
    assert!(
        !client.session().fenced(),
        "the post-fence session is a fresh, clean-view member"
    );
    assert_ne!(
        client.session().epoch(),
        stranded_epoch,
        "the fresh lease is a new epoch — a dead lease is never resurrected"
    );
    plane.shutdown();
}

/// The same deadline fence with the owner still DARK: the tick must answer
/// `RejoinRefused` (the loop RETRIES until the owner answers) — never the
/// loop-terminal `Fenced` a purged reader got before the fix.
#[tokio::test]
async fn a_reader_fenced_by_deadline_with_the_owner_dark_keeps_retrying() {
    let _serial = serial();
    let secret = b"s7-dark-owner-secret".to_vec();
    let (owner_clock, _oticks) = manual_clock();
    let owner = owner_with(shipped_clocks(), owner_clock, 7);
    let plane = MembershipPlane::start(
        MembershipPlaneConfig::loopback(),
        secret.clone(),
        Arc::clone(&owner),
    )
    .expect("plane binds loopback");
    let endpoint = plane.endpoint().to_string();

    let (member_clock, mticks) = manual_clock();
    let req = join_req("dark-owner-reader", MemberRole::Reader, None);
    let mut client = MemberClient::join(&endpoint, &secret, req.clone(), member_clock.clone())
        .await
        .expect("join");

    // The owner goes DARK (a dead process closes its listener — the fast
    // half; a frozen one times out — same ladder, slower venue), and the
    // member's clock runs past T_self.
    plane.shutdown();
    mticks.fetch_add(60_000, Ordering::SeqCst);
    assert!(client.session().self_fence_due());

    let purges = Arc::new(AtomicU64::new(0));
    let on_purge: Arc<dyn Fn() + Send + Sync> = {
        let purges = Arc::clone(&purges);
        Arc::new(move || {
            purges.fetch_add(1, Ordering::SeqCst);
        })
    };
    let tick = membership::member_renewal_tick(
        &mut client,
        &endpoint,
        &secret,
        &req,
        &member_clock,
        Some(&on_purge),
    )
    .await;

    assert_eq!(
        purges.load(Ordering::SeqCst),
        1,
        "the purge runs at the fence, before any re-present attempt"
    );
    assert_eq!(
        tick,
        membership::RenewalTick::RejoinRefused,
        "with the owner dark the purged reader must keep RETRYING the fresh join — \
         Fenced is loop-terminal and strands the mount forever"
    );
}

/// The asymmetry the fix must NOT erase: a WRITER member's deadline fence
/// poisons process data custody (S7) — no purge can make that clean, so a
/// writer NEVER re-presents fresh. `Fenced` is the correct terminal there.
#[tokio::test]
async fn a_writer_member_fenced_by_its_own_deadline_never_re_presents() {
    let _serial = serial();
    squeezefs::data_custody::test_clear_poison();
    let secret = b"s7-writer-fence-secret".to_vec();
    let (owner_clock, _oticks) = manual_clock();
    let owner = owner_with(shipped_clocks(), owner_clock, 7);
    let plane = MembershipPlane::start(
        MembershipPlaneConfig::loopback(),
        secret.clone(),
        Arc::clone(&owner),
    )
    .expect("plane binds loopback");
    let endpoint = plane.endpoint().to_string();

    let (member_clock, mticks) = manual_clock();
    let req = join_req("deadline-writer", MemberRole::Writer, None);
    let mut client = MemberClient::join(&endpoint, &secret, req.clone(), member_clock.clone())
        .await
        .expect("join");
    owner
        .evict("deadline-writer", "lease TTL expired (test)")
        .expect("evict");
    mticks.fetch_add(60_000, Ordering::SeqCst);
    assert!(client.session().self_fence_due());

    let tick =
        membership::member_renewal_tick(&mut client, &endpoint, &secret, &req, &member_clock, None)
            .await;

    assert_eq!(
        tick,
        membership::RenewalTick::Fenced,
        "a writer's deadline fence is terminal — its S7 poison cannot be made clean"
    );
    assert!(
        squeezefs::data_custody::poisoned(),
        "the writer's self-fence must have poisoned process data custody (S7)"
    );
    squeezefs::data_custody::test_clear_poison();
    plane.shutdown();
}

/// fence+purge at owner failover would be the cluster-wide flush storm the
/// window exists to prevent (spec §6.7 Recovery).
#[tokio::test]
async fn a_grace_window_reclaim_is_continuity_and_never_fences() {
    let _serial = serial();
    let secret = b"s6-grace-secret".to_vec();
    let (clock1, _t1) = manual_clock();
    let owner1 = owner_with(shipped_clocks(), clock1, 5);
    let plane1 = MembershipPlane::start(
        MembershipPlaneConfig::loopback(),
        secret.clone(),
        Arc::clone(&owner1),
    )
    .expect("plane1 binds");
    let (member_clock, _mt) = manual_clock();
    let req = join_req("grace-reader", MemberRole::Reader, None);
    let mut client = MemberClient::join(
        &plane1.endpoint().to_string(),
        &secret,
        req.clone(),
        member_clock.clone(),
    )
    .await
    .expect("join owner1");

    // The owner dies; its successor bumps the term, opens grace over the
    // predecessor's evidence, and serves from a NEW endpoint.
    plane1.shutdown();
    let (clock2, _t2) = manual_clock();
    let owner2 = owner_with(shipped_clocks(), clock2, 6);
    owner2.open_grace(vec!["grace-reader".to_string()]);
    let plane2 = MembershipPlane::start(
        MembershipPlaneConfig::loopback(),
        secret.clone(),
        Arc::clone(&owner2),
    )
    .expect("plane2 binds");

    let purges = Arc::new(AtomicU64::new(0));
    let on_purge: Arc<dyn Fn() + Send + Sync> = {
        let purges = Arc::clone(&purges);
        Arc::new(move || {
            purges.fetch_add(1, Ordering::SeqCst);
        })
    };
    let fences_before = METRICS.membership_self_fences.load(Ordering::Relaxed);
    let reclaims_before = METRICS.membership_grace_reclaims.load(Ordering::Relaxed);

    let tick = membership::member_renewal_tick(
        &mut client,
        &plane2.endpoint().to_string(),
        &secret,
        &req,
        &member_clock,
        Some(&on_purge),
    )
    .await;

    assert_eq!(
        tick,
        membership::RenewalTick::Rejoined,
        "a grace-window reclaim is CONTINUITY — re-assert and keep serving"
    );
    assert_eq!(
        METRICS.membership_self_fences.load(Ordering::Relaxed),
        fences_before,
        "failover re-assertion must not fence"
    );
    assert_eq!(purges.load(Ordering::SeqCst), 0, "and must not purge");
    assert_eq!(
        METRICS.membership_grace_reclaims.load(Ordering::Relaxed) - reclaims_before,
        1,
        "the successor counted the reclaim"
    );
    plane2.shutdown();
}

/// The rendezvous record must advertise an endpoint members can DIAL: an
/// explicit `SQUEEZEFS_MEMBERSHIP_BIND=addr:port` on a specific interface
/// must advertise THAT address — advertising the primary-interface IP
/// names a place where nothing listens, and every member of that fleet
/// stays silently invisible (found building the S6-b netns venue, where
/// the dialable address and the primary IP genuinely differ).
#[test]
fn an_explicit_membership_bind_advertises_the_bound_address() {
    let bind: std::net::SocketAddr = "10.11.12.13:7401".parse().expect("literal");
    assert_eq!(
        membership::owner_advertise_endpoint(bind, 7401),
        "10.11.12.13:7401",
        "an explicit bind is the operator saying WHERE the plane is served"
    );
    // The `auto` posture (0.0.0.0:0) keeps today's behavior: nothing
    // listens 'at' the unspecified address, so the advertised IP is the
    // primary-interface derivation.
    let auto: std::net::SocketAddr = "0.0.0.0:0".parse().expect("literal");
    let advertised = membership::owner_advertise_endpoint(auto, 4567);
    assert!(
        advertised.ends_with(":4567"),
        "auto advertises the BOUND port ({advertised})"
    );
    assert!(
        !advertised.starts_with("0.0.0.0"),
        "the unspecified address is never advertised ({advertised})"
    );
}

/// A CLEAN member disarm (the unmount path) must deliver its leave to the
/// owner BEFORE the process can exit: `MembershipOwner::leave`'s own
/// contract is "no TTL wait for a mount that said goodbye", but the
/// stop-latch-only disarm left the leave to the renewal loop's NEXT WAKE
/// (≤ one renew interval away) — which a normal umount never reaches, so
/// every cleanly-departed reader lingered in the census (and in
/// `squeezefs clients`) as live-then-stale for a full owner TTL. Found by
/// the rung-7 S6-b rig's census-settle wait.
#[tokio::test]
async fn a_clean_member_disarm_leaves_the_census_before_process_exit() {
    let _serial = serial();
    let secret = b"s6-clean-leave-secret".to_vec();
    let (oclock, _oticks) = manual_clock();
    let owner = owner_with(shipped_clocks(), oclock, 5);
    let plane = MembershipPlane::start(
        MembershipPlaneConfig::loopback(),
        secret.clone(),
        Arc::clone(&owner),
    )
    .expect("plane binds loopback");
    let endpoint = plane.endpoint().to_string();

    // The volume carries the rendezvous record + the enroll secret — the
    // reader's whole join input (possession of volume access IS cluster
    // membership, ruling D2).
    let meta = formatted_volume().await;
    let be = KvMetaBackend::open(meta.path()).await.expect("open volume");
    let enroll = serde_json::json!({ "secret": squeezefs::cluster_wire::hex_encode(&secret) });
    be.setxattr_internal(
        1,
        squeezefs::job_wire::JOB_ENROLL_XATTR,
        enroll.to_string().as_bytes(),
    )
    .await
    .expect("write the enroll record");
    membership::publish_owner_record(
        &be,
        &OwnerRecord {
            v: 1,
            id: "owner-clean-leave".into(),
            term: 5,
            endpoint,
            owner_claim_id: String::new(),
            ttl_ms: CLIENT_STALE_TTL_SECS * 1000,
            ts: now_secs(),
            pid: std::process::id(),
            boot: "boot-test".into(),
        },
    )
    .await
    .expect("publish the rendezvous record");

    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        Arc::clone(&be),
    ]));
    let arm = membership::arm_mount_membership(&routed, true, None)
        .await
        .expect("arm must not error")
        .expect("the reader must join the plane it discovered");
    assert_eq!(arm.mode(), "member");
    assert_eq!(owner.len(), 1, "the reader is in the census");

    arm.disarm().await;
    assert_eq!(
        owner.len(),
        0,
        "a mount that said goodbye must not wait out a TTL — the disarm must \
         deliver the leave before the process can exit"
    );
    plane.shutdown();
    be.shutdown().await.expect("clean shutdown");
}
