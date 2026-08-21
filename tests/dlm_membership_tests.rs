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
use squeezefs::meta_backend::kv::backend::{
    xattr_name_allowed, KvMetaBackend, WriterClaim, WRITER_CLAIM_XATTR,
};
use squeezefs::meta_backend::kv::builder::{format_v3_single_writer, FormatV3Options};
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

/// A SINGLE-WRITER (unstamped-class) volume: this suite exercises the
/// bit-14 claim set in isolation — engaged arms stamp it explicitly, and
/// the projection arms need it absent (the rung-10b DEFAULT format would
/// stamp the whole nine-bit set).
async fn formatted_volume() -> NamedTempFile {
    let meta = NamedTempFile::new().expect("temp volume");
    meta.as_file().set_len(VOL_LEN).expect("size the volume");
    format_v3_single_writer(meta.path(), VOL_LEN, &opts())
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

// ---------------------------------------------------------------------------
// 12. Per-volume claim admission (PR 2) — the durable ownership fields
// ---------------------------------------------------------------------------
//
// `docs/design-per-volume-claim-admission.md` §5.2 (KD-PV-2): a set's
// metadata volumes may be owned by DIFFERENT nodes, and the truth lives in
// **each volume's own** `claim_set` record — `owner` names the durable
// member id that appends to THIS volume, `successors` are KD-PV-12's
// ordered, statically-declared adoption candidates (ownership does NOT
// fail over by default). One record per volume cannot disagree with
// itself, which a set-wide table could.
//
// PR 2 lands the fields and the `owner_assign:` bracket record and
// **nothing else**: no mount reads them to make a decision (PR 3's ladder,
// PR 5's derivation) and no production path writes them (PR 7's offline
// `volume set-owners` verb is the first writer). So the whole rung is a
// byte-identity + RMW-preservation contract:
//
// * an UNASSIGNED set's record is byte-identical to the pre-program image
//   and sector 0 never moves — this program takes no incompat bit, it
//   gates on bit 14;
// * every RMW site that rewrites the record preserves the fields, because
//   that is where an assignment can be silently LOST — and a loss reads as
//   a legitimate legacy posture rather than as damage (§5.2.2, Issue 16);
// * an undecodable record on a volume that may carry an assignment
//   **refuses** instead of resetting to the legacy shape.

/// The **pre-program** `claim_set` image, frozen as a literal: exactly
/// what `dev`'s `ClaimSet::encode` produced before `owner`/`successors`
/// existed. A hand-written literal is the point — comparing against
/// `encode()` would pin the encoder to itself and could not catch the
/// field that must not appear.
const PRE_PROGRAM_CLAIM_SET: &str = concat!(
    r#"{"members":[{"boot":"","endpoint":null,"id":"node_0000000000000001.m00000001","#,
    r#""pid":0,"pr_key":0,"role":"writer","ts":1700000000},"#,
    r#"{"boot":"boot-b","endpoint":"10.0.0.2:7100","id":"node_0000000000000002.m00000001","#,
    r#""pid":4242,"pr_key":4660,"role":"writer","ts":1700000001}],"term":7,"v":1}"#,
);

/// The set whose encoding is [`PRE_PROGRAM_CLAIM_SET`]: the pid-less
/// roster form KD-PV-4 enrolls plus one live writer.
fn pre_program_set() -> ClaimSet {
    let mut set = ClaimSet::empty(7);
    set.members.push(ClaimSetMember {
        identity: membership::MemberIdentity {
            id: "node_0000000000000001.m00000001".into(),
            role: MemberRole::Writer,
            pid: 0,
            boot: String::new(),
            endpoint: None,
            pr_key: 0,
        },
        ts: 1_700_000_000,
    });
    set.members.push(ClaimSetMember {
        identity: membership::MemberIdentity {
            id: "node_0000000000000002.m00000001".into(),
            role: MemberRole::Writer,
            pid: 4242,
            boot: "boot-b".into(),
            endpoint: Some("10.0.0.2:7100".into()),
            pr_key: 0x1234,
        },
        ts: 1_700_000_001,
    });
    set
}

/// A bit-14 volume with no `claim_set` record yet.
async fn engaged_volume() -> NamedTempFile {
    let meta = formatted_volume().await;
    assert!(
        sb::set_claim_set_bit(meta.path())
            .await
            .expect("stamp bit 14 offline"),
        "the bit must be newly set"
    );
    meta
}

/// **The headline byte-identity law**: a set that names no owner encodes
/// to the pre-program image, byte for byte. An existing volume must not
/// change on disk because this program's code exists.
#[test]
fn a_set_with_no_owner_encodes_byte_identically_to_the_pre_program_record() {
    let set = pre_program_set();
    assert!(set.owner.is_none(), "an unassigned set names no owner");
    assert!(set.successors.is_empty());
    assert_eq!(
        String::from_utf8(set.encode()).expect("utf-8 record"),
        PRE_PROGRAM_CLAIM_SET,
        "an unassigned claim_set must encode BYTE-IDENTICALLY to the pre-program image \
         (design-per-volume-claim-admission §5.2.2): the fields are emitted only when \
         non-empty, so every volume in the field keeps its exact bytes"
    );

    // …and the assigned shape is a strict addition that round-trips.
    let mut assigned = pre_program_set();
    assigned.owner = Some("node_0000000000000002.m00000001".into());
    assigned.successors = vec!["node_0000000000000001.m00000001".into()];
    let img = assigned.encode();
    assert_ne!(img, set.encode(), "an assignment must be durable");
    let back = ClaimSet::decode(&img).expect("assigned record round-trips");
    assert_eq!(
        back.owner.as_deref(),
        Some("node_0000000000000002.m00000001")
    );
    assert_eq!(back.successors, vec!["node_0000000000000001.m00000001"]);
    assert_eq!(back.members, assigned.members);
    assert_eq!(back.term, assigned.term);

    // The `--clear` law (PR 7's `volume set-owners --clear`): clearing the
    // assignment restores the pre-program bytes exactly.
    let mut cleared = assigned;
    cleared.owner = None;
    cleared.successors.clear();
    assert_eq!(
        String::from_utf8(cleared.encode()).expect("utf-8 record"),
        PRE_PROGRAM_CLAIM_SET,
        "clearing an assignment must restore the unassigned image byte-identically"
    );
}

/// Forward tolerance: a record written by a pre-program binary decodes
/// with no owner and no successors — never a guess, never a refusal.
#[test]
fn a_pre_program_record_decodes_with_owner_none() {
    let set = ClaimSet::decode(PRE_PROGRAM_CLAIM_SET.as_bytes()).expect("pre-program record");
    assert!(set.durable);
    assert_eq!(set.members.len(), 2);
    assert!(
        set.owner.is_none(),
        "a pre-program record names no owner — the legacy shape is 'the set's sole \
         authority appends to everything'"
    );
    assert!(set.successors.is_empty());
    assert_eq!(set.term, 7);
    // An owner-carrying image written by a NEWER binary decodes here too
    // (the same tolerant `get(...).and_then(...)` law, both directions).
    let raw = br#"{"members":[],"owner":"node_00000000000000ab.m00000001","successors":["node_00000000000000cd.m00000002"],"term":9,"v":1}"#;
    let newer = ClaimSet::decode(raw).expect("a newer record decodes");
    assert_eq!(
        newer.owner.as_deref(),
        Some("node_00000000000000ab.m00000001")
    );
    assert_eq!(newer.successors, vec!["node_00000000000000cd.m00000002"]);
}

/// Projection purity: the singleton projection of `writer_claim` — what
/// every un-engaged volume answers with — can never carry an owner. An
/// unstamped volume has no assignment by construction, and a projection
/// that invented one would be an ownership claim nothing wrote.
#[tokio::test]
async fn the_singleton_projection_never_carries_an_owner() {
    let _serial = serial();
    let meta = formatted_volume().await;
    let be = KvMetaBackend::open(meta.path()).await.expect("open volume");
    be.setxattr_internal(
        1,
        WRITER_CLAIM_XATTR,
        &WriterClaim {
            id: "writer-solo".into(),
            ts: now_secs(),
            pid: std::process::id(),
            boot: "boot-test".into(),
            term: 3,
        }
        .encode(),
    )
    .await
    .expect("the gate's claim commit");

    let set = ClaimSet::load(&be).await.expect("the projection answers");
    assert!(!set.durable);
    assert!(
        set.owner.is_none() && set.successors.is_empty(),
        "the projection of a singular writer_claim names no per-volume owner"
    );
    assert!(membership::ClaimSet::from_writer_claim(&WriterClaim {
        id: "w".into(),
        ts: 1,
        pid: 1,
        boot: "b".into(),
        term: 1,
    })
    .owner
    .is_none());
    be.shutdown().await.expect("clean shutdown");
}

/// The bit-14 law extends to ownership: an assignment on an un-engaged
/// volume is refused loud, so no unstamped volume can grow a record.
#[tokio::test]
async fn store_refuses_an_owner_without_bit_14() {
    let _serial = serial();
    let meta = formatted_volume().await;
    let be = KvMetaBackend::open(meta.path()).await.expect("open volume");
    let mut set = ClaimSet::empty(4);
    set.owner = Some("node_00000000000000ab.m00000001".into());
    let err = ClaimSet::store(&be, &set)
        .await
        .expect_err("storing an owner without bit 14 must refuse");
    assert!(err.to_string().contains("bit 14"), "{err}");

    let err = membership::set_volume_owner(&be, Some("node_00000000000000ab.m00000001"), &[], 4)
        .await
        .expect_err("the assignment writer must refuse an un-engaged volume");
    assert!(err.to_string().contains("bit 14"), "{err}");
    assert!(
        !be.listxattr(1)
            .await
            .expect("listxattr")
            .iter()
            .any(|k| k == CLAIM_SET_XATTR),
        "a refused assignment writes NOTHING"
    );
    be.shutdown().await.expect("clean shutdown");
}

/// The gate's second half: assigning an owner takes **no incompat bit**,
/// so sector 0 is byte-identical across the assignment, and `--clear`
/// restores the record's bytes exactly.
#[tokio::test]
async fn an_assignment_moves_no_superblock_byte_and_clears_byte_identically() {
    let _serial = serial();
    let meta = engaged_volume().await;

    // The unassigned baseline, written and settled.
    let be = KvMetaBackend::open(meta.path()).await.expect("open volume");
    membership::upsert_writer_member(
        &be,
        &membership::MemberIdentity {
            id: "node_0000000000000001.m00000001".into(),
            role: MemberRole::Writer,
            pid: 0,
            boot: String::new(),
            endpoint: None,
            pr_key: 0,
        },
        6,
    )
    .await
    .expect("enroll the pid-less roster member");
    be.checkpoint_now().await.expect("settle");
    let baseline_record = be
        .getxattr(1, CLAIM_SET_XATTR)
        .await
        .expect("read")
        .expect("the record exists");
    be.shutdown().await.expect("clean shutdown");
    let sector0_before =
        std::fs::read(meta.path()).expect("read the volume")[..sb::SUPERBLOCK_V3_LEN].to_vec();

    // Assign, then clear.
    let be = KvMetaBackend::open(meta.path()).await.expect("reopen");
    membership::set_volume_owner(
        &be,
        Some("node_0000000000000001.m00000001"),
        &["node_0000000000000002.m00000001".to_string()],
        6,
    )
    .await
    .expect("assign this volume's owner");
    be.checkpoint_now().await.expect("settle");
    let assigned = ClaimSet::load(&be).await.expect("durable set");
    assert_eq!(
        assigned.owner.as_deref(),
        Some("node_0000000000000001.m00000001")
    );
    assert_eq!(assigned.successors, vec!["node_0000000000000002.m00000001"]);
    be.shutdown().await.expect("clean shutdown");
    let sector0_assigned =
        std::fs::read(meta.path()).expect("read the volume")[..sb::SUPERBLOCK_V3_LEN].to_vec();
    assert_eq!(
        sector0_before, sector0_assigned,
        "per-volume ownership takes NO incompat bit (§7: gated on bit 14) — sector 0 \
         must not move because a volume was assigned"
    );

    let be = KvMetaBackend::open(meta.path()).await.expect("reopen");
    membership::set_volume_owner(&be, None, &[], 6)
        .await
        .expect("--clear");
    be.checkpoint_now().await.expect("settle");
    assert_eq!(
        be.getxattr(1, CLAIM_SET_XATTR)
            .await
            .expect("read")
            .expect("the record survives a clear"),
        baseline_record,
        "`volume set-owners --clear` must restore the unassigned record BYTE-IDENTICALLY"
    );
    be.shutdown().await.expect("clean shutdown");
    assert_eq!(
        std::fs::read(meta.path()).expect("read the volume")[..sb::SUPERBLOCK_V3_LEN].to_vec(),
        sector0_before,
        "…and sector 0 still has not moved"
    );
}

/// **RMW pin 1** (§5.2.2, Issue 16): the enrollment upsert decodes,
/// mutates and stores the whole record — an owner it does not know about
/// must survive it. This is the site an assignment is most likely to be
/// silently lost at, because every mount performs it.
#[tokio::test]
async fn an_upsert_preserves_the_owner_and_successors() {
    let _serial = serial();
    let meta = engaged_volume().await;
    let be = KvMetaBackend::open(meta.path()).await.expect("open volume");
    let ident = |id: &str| membership::MemberIdentity {
        id: id.to_string(),
        role: MemberRole::Writer,
        pid: std::process::id(),
        boot: "boot-test".into(),
        endpoint: None,
        pr_key: 0x77,
    };
    membership::upsert_writer_member(&be, &ident("node_0000000000000001.m00000001"), 5)
        .await
        .expect("seed a member");
    membership::set_volume_owner(
        &be,
        Some("node_0000000000000001.m00000001"),
        &["node_0000000000000002.m00000001".to_string()],
        5,
    )
    .await
    .expect("assign");

    // A peer enrolls (KD-PV-4's roster shape) — the assignment is not its
    // business and must be untouched.
    membership::upsert_writer_member(&be, &ident("node_0000000000000002.m00000001"), 6)
        .await
        .expect("peer upsert");
    let set = ClaimSet::load(&be).await.expect("durable set");
    assert_eq!(set.members.len(), 2);
    assert_eq!(
        set.owner.as_deref(),
        Some("node_0000000000000001.m00000001"),
        "upsert_writer_member's decode→mutate→store RMW must PRESERVE the owner"
    );
    assert_eq!(set.successors, vec!["node_0000000000000002.m00000001"]);

    // Withdrawal is the same RMW.
    membership::withdraw_writer_member(&be, "node_0000000000000002.m00000001")
        .await
        .expect("withdraw");
    let set = ClaimSet::load(&be).await.expect("durable set");
    assert_eq!(set.members.len(), 1);
    assert_eq!(
        set.owner.as_deref(),
        Some("node_0000000000000001.m00000001"),
        "a withdrawal must not carry the assignment away with the member"
    );
    be.shutdown().await.expect("clean shutdown");
}

/// **RMW pin 2**: the rung-8 same-boot dead-writer prune rewrites the
/// member list under the D0 dead-holder proof. It must not drop the
/// ownership fields — a crashed peer's residue and a volume's assignment
/// are different facts.
#[tokio::test]
async fn the_dead_writer_prune_preserves_the_owner_and_successors() {
    let _serial = serial();
    let meta = engaged_volume().await;
    let be = KvMetaBackend::open(meta.path()).await.expect("open volume");
    let my_boot = squeezefs::meta_backend::kv::backend::read_boot_id();
    let dead_pid = {
        let mut child = std::process::Command::new("true").spawn().expect("spawn");
        let pid = child.id();
        child.wait().expect("reap");
        pid
    };
    membership::upsert_writer_member(
        &be,
        &membership::MemberIdentity {
            id: "uuid-dead-incarnation".into(),
            role: MemberRole::Writer,
            pid: dead_pid,
            boot: my_boot.clone(),
            endpoint: None,
            pr_key: 0x0dead,
        },
        3,
    )
    .await
    .expect("seed the dead predecessor");
    membership::set_volume_owner(
        &be,
        Some("node_0000000000000009.m00000001"),
        &["node_000000000000000a.m00000001".to_string()],
        3,
    )
    .await
    .expect("assign");

    membership::upsert_writer_member(
        &be,
        &membership::MemberIdentity {
            id: "node_0000000000000009.m00000001".into(),
            role: MemberRole::Writer,
            pid: std::process::id(),
            boot: my_boot,
            endpoint: None,
            pr_key: 0x51,
        },
        4,
    )
    .await
    .expect("the successor's upsert prunes the dead entry");

    let set = ClaimSet::load(&be).await.expect("durable set");
    let ids: Vec<&str> = set.members.iter().map(|m| m.identity.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["node_0000000000000009.m00000001"],
        "engagement: the prune must actually have fired"
    );
    assert_eq!(
        set.owner.as_deref(),
        Some("node_0000000000000009.m00000001"),
        "the rung-8 prune rewrites MEMBERS; the assignment is not membership residue"
    );
    assert_eq!(set.successors, vec!["node_000000000000000a.m00000001"]);
    be.shutdown().await.expect("clean shutdown");
}

/// **RMW pin 3 — the fail-closed direction.** The decode-failure fallback
/// (`ClaimSet::decode(&raw).unwrap_or_else(|| ClaimSet::empty(term))`)
/// would silently drop `owner` and convert a multi-owner volume into the
/// legacy shape: an ownership LOSS that reads as a legitimate posture. On
/// a volume that may carry an assignment — an armed ownership plane, or an
/// open `owner_assign:` bracket — the fallback is a loud refusal instead.
/// Unassigned volumes keep today's behaviour exactly.
#[tokio::test]
async fn an_undecodable_claim_set_on_an_assigned_volume_refuses_rather_than_resetting() {
    use squeezefs::meta_ship::owners::{self as ship, OwnerMap};
    let _serial = serial();
    let meta = engaged_volume().await;
    let be = KvMetaBackend::open(meta.path()).await.expect("open volume");
    let ident = membership::MemberIdentity {
        id: "node_0000000000000001.m00000001".into(),
        role: MemberRole::Writer,
        pid: std::process::id(),
        boot: "boot-test".into(),
        endpoint: None,
        pr_key: 0,
    };
    async fn garble(be: &KvMetaBackend) {
        be.setxattr_internal(1, CLAIM_SET_XATTR, b"{\"members\":")
            .await
            .expect("write a torn record");
    }

    // (a) No assignment evidence: today's behaviour, unchanged — the
    //     unattributable record is replaced by this member's own set.
    garble(&be).await;
    assert!(membership::upsert_writer_member(&be, &ident, 5)
        .await
        .expect("an unassigned volume still self-heals"));
    assert_eq!(
        ClaimSet::load(&be).await.expect("set").members.len(),
        1,
        "the unassigned fallback must still reset — nothing is at risk there"
    );

    // (b) An open `owner_assign:` bracket ⇒ refuse, and touch nothing.
    garble(&be).await;
    be.setxattr_internal(1, squeezefs::OWNER_ASSIGN_MARKER_XATTR, b"any-image")
        .await
        .expect("write the bracket marker");
    let err = membership::upsert_writer_member(&be, &ident, 6)
        .await
        .expect_err("an undecodable record under an assignment bracket must REFUSE");
    let text = err.to_string();
    assert!(
        text.contains("claim_set") && text.contains("owner"),
        "the refusal must name the record and what is at risk: {text}"
    );
    assert_eq!(
        be.getxattr(1, CLAIM_SET_XATTR)
            .await
            .expect("read")
            .as_deref(),
        Some(&b"{\"members\":"[..]),
        "a refusal must leave the evidence in place, never overwrite it"
    );
    be.removexattr_internal(1, squeezefs::OWNER_ASSIGN_MARKER_XATTR)
        .await
        .expect("drop the bracket");

    // (c) An ARMED ownership plane ⇒ refuse for the same reason, with no
    //     marker anywhere: a running multi-owner fleet is itself the
    //     evidence that a volume may be assigned.
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        Arc::clone(&be),
    ]));
    ship::arm_ownership(OwnerMap::for_volumes(&routed, Vec::new()).expect("owner map"));
    let armed = membership::upsert_writer_member(&be, &ident, 7).await;
    ship::disarm_ownership();
    let err = armed.expect_err("an undecodable record under an armed plane must REFUSE");
    assert!(err.to_string().contains("claim_set"), "{err}");

    // …and with the plane down and no marker, the fallback returns.
    assert!(membership::upsert_writer_member(&be, &ident, 8)
        .await
        .expect("the unassigned path is unchanged"));
    be.shutdown().await.expect("clean shutdown");
}

/// **RMW pin 4 (the fifth site, found in the code rather than the design
/// — see the PR report): the last member's departure.** `withdraw` deletes
/// the record once the set empties, so a departing last member would carry
/// the volume's assignment away with it. Under D19 the assignment is
/// durable OPERATOR state that outlives every mount: it survives, and only
/// `volume set-owners --clear` removes it. An UNASSIGNED set still
/// vanishes exactly as before.
#[tokio::test]
async fn a_last_member_withdrawal_preserves_the_assignment() {
    let _serial = serial();
    let meta = engaged_volume().await;
    let be = KvMetaBackend::open(meta.path()).await.expect("open volume");
    let ident = membership::MemberIdentity {
        id: "node_0000000000000003.m00000001".into(),
        role: MemberRole::Writer,
        pid: std::process::id(),
        boot: "boot-test".into(),
        endpoint: None,
        pr_key: 0,
    };
    membership::upsert_writer_member(&be, &ident, 5)
        .await
        .expect("enroll");
    membership::set_volume_owner(&be, Some("node_0000000000000003.m00000001"), &[], 5)
        .await
        .expect("assign");
    assert!(
        membership::withdraw_writer_member(&be, "node_0000000000000003.m00000001")
            .await
            .expect("the last member departs")
    );
    let set = ClaimSet::load(&be).await.expect("the record survives");
    assert!(set.durable, "an assigned volume's record is not deleted");
    assert!(set.members.is_empty());
    assert_eq!(
        set.owner.as_deref(),
        Some("node_0000000000000003.m00000001"),
        "a mount's departure must not unassign the volume it was mounted on"
    );

    // The unassigned set still disappears — the departed-set-presents-as-
    // unclaimed law is untouched where no assignment exists.
    membership::set_volume_owner(&be, None, &[], 5)
        .await
        .expect("--clear");
    membership::upsert_writer_member(&be, &ident, 5)
        .await
        .expect("re-enroll");
    assert!(
        membership::withdraw_writer_member(&be, "node_0000000000000003.m00000001")
            .await
            .expect("withdraw")
    );
    assert!(
        !be.listxattr(1)
            .await
            .expect("listxattr")
            .iter()
            .any(|k| k == CLAIM_SET_XATTR),
        "an UNASSIGNED set still deletes its record when it empties"
    );
    be.shutdown().await.expect("clean shutdown");
}

/// **RMW pin 5**: `claim_set` is a PER-VOLUME control record and
/// `is_pinned_control_record` already excludes it from a slot's travel set
/// (`slot_migration.rs:275`) — pinned here as a CONTRACT rather than left
/// as an inference, because an owner field that travelled into another
/// volume's guest keyspace would be an assignment nobody wrote, on the one
/// record whose whole safety argument is "one record, one volume".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_owner_field_survives_a_migrate_slot_of_any_other_slot() {
    use squeezefs::meta_backend::slot_migration::{
        migrate_slot, MigrationOptions, MigrationTestHooks,
    };
    use squeezefs::meta_backend::{
        guest_local_ino, open_routed_meta_set, plan_meta_slot_set_with_width,
    };
    let _serial = serial();
    const MIG_VOL_LEN: u64 = 256 * 1024 * 1024;

    let dir = tempfile::tempdir().expect("tempdir");
    let metas: Vec<std::path::PathBuf> = ["m0", "m1"]
        .iter()
        .map(|n| {
            let p = dir.path().join(n);
            std::fs::File::create(&p)
                .expect("create")
                .set_len(MIG_VOL_LEN)
                .expect("size");
            p
        })
        .collect();
    // W = 2N so a flip never leaves a member hostless (the VL5b fixture).
    let plan = plan_meta_slot_set_with_width(metas.len(), 4).expect("plan admits the bounds");
    for (i, m) in metas.iter().enumerate() {
        squeezefs::meta_backend::kv::builder::format_v3_stamped(
            m,
            MIG_VOL_LEN,
            &FormatV3Options {
                node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
                journal_len_override: None,
                force: true,
                full_wipe: false,
                format_config_xattr: None,
            },
            plan.stamps[i].clone(),
        )
        .await
        .expect("format a stamped member");
    }
    let paths: Vec<String> = metas.iter().map(|p| p.display().to_string()).collect();

    let routed = open_routed_meta_set(&paths).await.expect("open the set");
    assert!(
        membership::claim_set_engaged(routed.volumes[1].superblock().features_incompat),
        "the default format is multi-writer-capable: bit 14 is stamped"
    );
    // Volume 1 is assigned to a peer; volume 0 to this node (D20's set
    // authority hosts slot 0).
    membership::set_volume_owner(
        &routed.volumes[0],
        Some("node_000000000000000a.m00000001"),
        &[],
        3,
    )
    .await
    .expect("assign volume 0");
    membership::set_volume_owner(
        &routed.volumes[1],
        Some("node_000000000000000b.m00000001"),
        &["node_000000000000000a.m00000001".to_string()],
        3,
    )
    .await
    .expect("assign volume 1");

    // Migrate slot 1 — volume 1's NATIVE keyspace — onto volume 0.
    let report = migrate_slot(
        &routed,
        1,
        0,
        &MigrationOptions::default(),
        &MigrationTestHooks::default(),
    )
    .await
    .expect("the migration succeeds");
    assert!(
        report.records_copied > 0,
        "engagement: the migration must have copied records"
    );

    // The source volume keeps its own assignment…
    let src = ClaimSet::load(&routed.volumes[1])
        .await
        .expect("volume 1's record");
    assert_eq!(
        src.owner.as_deref(),
        Some("node_000000000000000b.m00000001"),
        "a slot migration must not carry a volume's OWNER away from the only place \
         anything looks for it (is_pinned_control_record)"
    );
    assert_eq!(src.successors, vec!["node_000000000000000a.m00000001"]);
    // …the destination keeps its own, unchanged…
    let dst = ClaimSet::load(&routed.volumes[0])
        .await
        .expect("volume 0's record");
    assert_eq!(
        dst.owner.as_deref(),
        Some("node_000000000000000a.m00000001"),
        "the destination's assignment is its own and must not be overwritten by the \
         travelling slot"
    );
    // …and no copy landed in the host's guest keyspace, where it would be
    // an assignment nobody wrote.
    assert_eq!(
        routed.volumes[0]
            .getxattr(guest_local_ino(1, 1), CLAIM_SET_XATTR)
            .await
            .expect("read the guest keyspace root"),
        None,
        "a claim_set copy in a guest keyspace is residue that names an owner nobody \
         assigned"
    );

    for vol in &routed.volumes {
        vol.shutdown().await.expect("clean shutdown");
    }
}

/// The `owner_assign:` bracket record (KD-PV-2, §7): the `mw_upgrade:`
/// mechanism verbatim — versioned, checksummed, and refusing every image
/// it cannot fully interpret, because presence alone is the refusal
/// predicate a writable mount will read (PR 4) and a misread would name
/// the wrong remedy.
#[test]
fn the_owner_assign_marker_round_trips_and_refuses_torn_or_foreign_images() {
    use squeezefs::config_ops::{OwnerAssignMarker, OwnerAssignment, OWNER_ASSIGN_MARKER_VERSION};
    let marker = OwnerAssignMarker {
        assignments: vec![
            OwnerAssignment {
                volume_id: "vol-0a1b2c3d4e5f6071".into(),
                owner: Some("node_000000000000000a.m00000001".into()),
                successors: vec!["node_000000000000000b.m00000001".into()],
            },
            OwnerAssignment {
                volume_id: "vol-1122334455667788".into(),
                owner: Some("node_000000000000000b.m00000001".into()),
                successors: Vec::new(),
            },
        ],
    };
    let img = marker.encode();
    assert_eq!(
        OwnerAssignMarker::decode(&img).expect("round trip"),
        marker,
        "the bracket must round-trip exactly — a resume compares it against the act it \
         is completing"
    );

    // The `--clear` act is expressible: an assignment with no owner.
    let cleared = OwnerAssignMarker {
        assignments: vec![OwnerAssignment {
            volume_id: "vol-0a1b2c3d4e5f6071".into(),
            owner: None,
            successors: Vec::new(),
        }],
    };
    assert_eq!(
        OwnerAssignMarker::decode(&cleared.encode()).expect("round trip"),
        cleared
    );

    // Torn: one flipped byte anywhere fails the checksum.
    let mut torn = img.clone();
    torn[5] ^= 0xff;
    assert!(OwnerAssignMarker::decode(&torn)
        .expect_err("a torn image must refuse")
        .contains("checksum"));

    // Truncated in every prefix — never a panic, never a partial answer.
    for cut in 0..img.len() {
        assert!(
            OwnerAssignMarker::decode(&img[..cut]).is_err(),
            "a truncated image must refuse (cut at {cut})"
        );
    }

    // Trailing bytes: foreign or torn, never silently ignored.
    let mut trailing = img.clone();
    let sum = trailing.split_off(trailing.len() - 8);
    trailing.push(0);
    trailing.extend_from_slice(&sum);
    assert!(OwnerAssignMarker::decode(&trailing).is_err());

    // A future version refuses loud instead of guessing (forward-only).
    let mut future = img.clone();
    future[0] = OWNER_ASSIGN_MARKER_VERSION + 1;
    let sum = xxhash_rust::xxh3::xxh3_64(&future[..future.len() - 8]);
    let n = future.len();
    future[n - 8..].copy_from_slice(&sum.to_le_bytes());
    let err = OwnerAssignMarker::decode(&future).expect_err("a future version must refuse");
    assert!(err.contains("version"), "{err}");

    // A duplicate volume id cannot be interpreted (which assignment wins?).
    let dup = OwnerAssignMarker {
        assignments: vec![
            OwnerAssignment {
                volume_id: "vol-0a1b2c3d4e5f6071".into(),
                owner: Some("node_000000000000000a.m00000001".into()),
                successors: Vec::new(),
            },
            OwnerAssignment {
                volume_id: "vol-0a1b2c3d4e5f6071".into(),
                owner: Some("node_000000000000000b.m00000001".into()),
                successors: Vec::new(),
            },
        ],
    };
    assert!(OwnerAssignMarker::decode(&dup.encode()).is_err());
}

/// The new record is invisible through the mount: `owner_assign:` is
/// outside the VAL-2 positive allowlist, so it can never be read, written,
/// listed or removed from an unprivileged shell — the same posture
/// `mw_upgrade:` / `job:` / `alloc_lane:` / `claim_set` hold.
#[test]
fn the_owner_assign_marker_is_invisible_through_the_fuse_boundary() {
    for name in [
        squeezefs::OWNER_ASSIGN_MARKER_XATTR,
        squeezefs::MW_UPGRADE_MARKER_XATTR,
        CLAIM_SET_XATTR,
    ] {
        assert!(
            !xattr_name_allowed(name),
            "{name} carries SqueezeFS-internal durable state and must never cross the \
             FUSE boundary (VAL-2 allowlist)"
        );
    }
    assert!(
        squeezefs::OWNER_ASSIGN_MARKER_XATTR.starts_with("owner_assign:"),
        "the record name is the design's (§5.2.1/§7) — the mount gate and the operator \
         verb both name it"
    );
}
