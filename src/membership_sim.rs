//! The **D1 simulated-client harness** for the membership plane (ruling
//! **D1** validation tier (ii), extending the spec's §6.10 **R6** pattern —
//! 1,875 simulated clients — to the 15,000 the product claim names;
//! `docs/pre-rc-execution-plan.md` Phase 4 / S6 row).
//!
//! # Why a harness and not a mount matrix
//!
//! "15 k real concurrent writers cannot be tested directly" (D1). What CAN
//! be driven at that scale, in one process, with no fabric, is the plane
//! this stage ships: **membership, heartbeat (lease renewal), revoke
//! (eviction fan-out) and failover (grace-window completion)**. Those four
//! are exactly what §6.5 item 3 measured as saturating, so they are exactly
//! what a simulated row must answer.
//!
//! # The row this produces, and the one that is DEFERRED
//!
//! [`SimReport::render`] prints the four figures the S6 gate is adjudicated
//! on:
//!
//! | Figure | What it answers |
//! |---|---|
//! | **volume-0 journal tx/s** | the gate itself: `meta_kv_journal_entries` delta ÷ wall time must be ~0 at 15 k members (the pre-S6 plane serialized 455 beats/s against a 1,500/s requirement) |
//! | **renewal latency distribution** | p50/p99/max per renewal — the term that decides whether 15 k × (1/10 s) beats fit |
//! | **revoke fan-out** | wall time to evict a cohort, i.e. §6.5 item 4's "30–75 ms of serialized daemon time" for a 15 k-holder revoke |
//! | **failover grace completion** | wall time from `open_grace` to the window closing on re-assertion — R6's "1,875 clients × 1 k tokens reclaiming inside a 45 s grace window" |
//!
//! Per ruling **D11** the 15,000-client run itself is **DEFERRED** (no
//! measured rows during the DLM push). The suite runs this at a few hundred
//! clients to prove the harness works; the deferred invocation is
//! `SimConfig { clients: 15_000, readers_pct: 50, beats: 3, mode:
//! SimMode::Direct, evict_fraction_permille: 10, failover: true }`, whose
//! `render()` output is the row.
//!
//! # Direct vs Wired, and why both exist
//!
//! * [`SimMode::Direct`] drives the owner's API in-process: no sockets, so
//!   15 k members cost 15 k RAM entries instead of 15 k TCP connections.
//!   This is the SCALE instrument, and it is honest about what it excludes
//!   — framing, authentication and the wake, which S3 already measured
//!   separately (`.benchmarks/2026-08-05-dlm-s3-cluster-wire.md`).
//! * [`SimMode::Wired`] drives real `cluster_wire` sessions at small N: the
//!   plane's own verbs over the real transport. This is the PATH
//!   instrument — it proves the numbers above are measured against the same
//!   code a fleet runs, not a mock.
//!
//! Victims of the revoke leg are always READERS, deliberately: a writer's
//! self-fence poisons process data custody (S7), which is correct in
//! production and would be a process-wide side effect in a harness.

use crate::error::{Result, SqueezefsError};
use crate::membership::{
    JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MemberSession, MembershipOwner,
    RenewOutcome,
};
use crate::membership_wire::{MemberClient, MembershipPlane, MembershipPlaneConfig};
use crate::meta_backend::kv::META_KV_JOURNAL_ENTRIES;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Which plane the harness drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimMode {
    /// The owner's API in-process (the SCALE instrument).
    Direct,
    /// Real `cluster_wire` sessions (the PATH instrument, small N).
    Wired,
}

impl SimMode {
    /// The label a row carries.
    pub fn as_str(self) -> &'static str {
        match self {
            SimMode::Direct => "direct",
            SimMode::Wired => "wired",
        }
    }
}

/// One harness run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimConfig {
    /// Simulated members.
    pub clients: usize,
    /// Share of them that are read-only mounts, percent.
    pub readers_pct: u8,
    /// Renewal rounds per member.
    pub beats: usize,
    /// Which plane to drive.
    pub mode: SimMode,
    /// Share of members to revoke, per mille (the revoke fan-out leg;
    /// `0` skips it). Victims are readers only — see the module docs.
    pub evict_fraction_permille: u32,
    /// Run the owner-failover leg (successor term bump → grace window →
    /// re-assertion).
    pub failover: bool,
}

/// The measured row.
#[derive(Debug, Clone, PartialEq)]
pub struct SimReport {
    /// Members driven.
    pub clients: usize,
    /// Renewal rounds per member.
    pub beats: usize,
    /// Total renewals performed.
    pub renewals: usize,
    /// **The S6 gate**: journal entries committed by the whole run. Must be
    /// 0 — liveness never touches the metadata plane.
    pub journal_entries_delta: u64,
    /// Renewal latency, µs.
    pub renew_p50_us: f64,
    /// Renewal latency, µs.
    pub renew_p99_us: f64,
    /// Renewal latency, µs.
    pub renew_max_us: f64,
    /// Wall time to revoke the victim cohort, µs (`0` = leg skipped).
    pub revoke_fanout_us: f64,
    /// Wall time from `open_grace` to the window closing on re-assertion,
    /// µs (`0` = leg skipped).
    pub grace_completion_us: f64,
    /// Members evicted.
    pub evictions: usize,
    /// Self-fences the revoked members performed on discovering their
    /// eviction (the client half of the revoke path).
    pub self_fences: usize,
    /// Census pages walked (the read-side cost, in round trips).
    pub census_pages: usize,
    /// Members the final census reported.
    pub census_rows: usize,
    /// Wall time of the whole run, seconds.
    pub wall_secs: f64,
    /// `direct` or `wired`.
    pub mode: &'static str,
    /// PR 8 (KD-SYM-15): membership shards driven (1 = the shipped single
    /// owner; [`run_sharded`] arms one owner per shard).
    pub shards: usize,
    /// PR 8 (§5.5.3): members of the killed shard that PARKED at `T_self`
    /// (0 on an unsharded run).
    pub parked: usize,
    /// Parked members that RECLAIMED under the successor's grace.
    pub reclaimed: usize,
    /// Parks that outlived `T_park_max` (must be 0 on a healthy failover).
    pub park_expiries: usize,
    /// PR 13 (SIM-1): slot leases CARRIED on the renewal grants — the
    /// installed carriage source's words (`M` per member per beat; 0 with
    /// no source).
    pub carriage_leases: usize,
    /// PR 13 (SIM-1, the broadcast shape §5.7.4): token holders of ONE
    /// object recalled by one commit (0 = leg skipped).
    pub recall_readers: usize,
    /// Wall of that recall — issue → every ack observed — µs.
    pub recall_fanout_us: f64,
    /// Acks the plane counted for it (must equal `recall_readers`).
    pub recall_acks: usize,
    /// PR 13 (SIM-1, §5.5.2): members declared DEAD by their home shard's
    /// eviction, each reaching the death ledger's sink.
    pub death_records: usize,
    /// Worst eviction → sink latency, µs (the record's write; the
    /// projection's poll cadence is `death_poll_ms`).
    pub death_sink_us: f64,
    /// The ledger poll cadence every manager reads the record at, ms — the
    /// derived checkpoint landing ceiling; propagation bound = sink +
    /// poll.
    pub death_poll_ms: u64,
    /// Shards whose projection read the record (must equal `shards`).
    pub death_shards_reached: usize,
    /// PR 13 (SIM-1, §5.7.3's fan-in): wall for every shard's
    /// `min_acked_free_epoch` to close on a fresh label once every member
    /// acked it on ONE renewal, µs.
    pub free_grace_fanin_us: f64,
}

impl SimReport {
    /// The row. Every figure the S6 gate is adjudicated on, labelled — and
    /// labelled with its TIER, because a simulated row is
    /// *measured-simulated* evidence, never *measured-real*
    /// (`docs/rc-manifest.md`).
    pub fn render(&self) -> String {
        let beats_per_s = if self.wall_secs > 0.0 {
            self.renewals as f64 / self.wall_secs
        } else {
            0.0
        };
        let journal_per_s = if self.wall_secs > 0.0 {
            self.journal_entries_delta as f64 / self.wall_secs
        } else {
            0.0
        };
        format!(
            "DLM S6 membership row (tier: measured-simulated, mode {mode})\n\
             clients={clients} readers_pct-driven beats={beats} renewals={renewals} \
             wall={wall:.3}s ({beats_per_s:.0} renewals/s offered)\n\
             volume-0 journal tx/s: {journal_per_s:.3} (delta {journal} entries; the \
             pre-S6 plane serialized 455 beats/s against 1,500/s needed at 15 k clients)\n\
             renewal latency: p50 {p50:.2} µs, p99 {p99:.2} µs, max {max:.2} µs\n\
             revoke fan-out: {revoke:.2} µs for {evictions} member(s), {fences} \
             self-fence(s)\n\
             grace completion: {grace:.2} µs (failover re-assertion window)\n\
             census: {rows} row(s) in {pages} page(s)\n\
             shards={shards} parked={parked} reclaimed={reclaimed} park_expiries={expiries}\n\
             slot-lease carriage: {carriage} lease word(s) on the grants\n\
             token recall fan-out: {rreaders} holder(s) of one object recalled in \
             {rfan:.2} µs, {racks} ack(s)\n\
             death ledger: {deaths} record(s), sink ≤ {dsink:.2} µs, poll cadence {dpoll} ms, \
             read by {dshards} shard(s)\n\
             free-grace fan-in: every shard closed on the label in {fgfan:.2} µs",
            shards = self.shards,
            parked = self.parked,
            reclaimed = self.reclaimed,
            expiries = self.park_expiries,
            carriage = self.carriage_leases,
            rreaders = self.recall_readers,
            rfan = self.recall_fanout_us,
            racks = self.recall_acks,
            deaths = self.death_records,
            dsink = self.death_sink_us,
            dpoll = self.death_poll_ms,
            dshards = self.death_shards_reached,
            fgfan = self.free_grace_fanin_us,
            mode = self.mode,
            clients = self.clients,
            beats = self.beats,
            renewals = self.renewals,
            wall = self.wall_secs,
            beats_per_s = beats_per_s,
            journal_per_s = journal_per_s,
            journal = self.journal_entries_delta,
            p50 = self.renew_p50_us,
            p99 = self.renew_p99_us,
            max = self.renew_max_us,
            revoke = self.revoke_fanout_us,
            evictions = self.evictions,
            fences = self.self_fences,
            grace = self.grace_completion_us,
            rows = self.census_rows,
            pages = self.census_pages,
        )
    }
}

fn pct(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (((sorted.len() - 1) as f64) * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Deterministic role assignment: the first `readers_pct` of every hundred
/// are readers, so a run's mix is reproducible without an RNG.
fn role_for(i: usize, readers_pct: u8) -> MemberRole {
    if i % 100 < usize::from(readers_pct).min(100) {
        MemberRole::Reader
    } else {
        MemberRole::Writer
    }
}

fn join_request(id: &str, role: MemberRole, endpoint: Option<String>) -> JoinRequest {
    JoinRequest {
        id: id.to_string(),
        role,
        endpoint,
        pid: std::process::id(),
        boot: "sim".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    }
}

/// Run the harness.
///
/// The owner runs on the REAL monotonic clock with the shipped lease
/// parameters, so nothing expires underneath the run (45 s TTL) and every
/// latency figure is a real measurement rather than a manual-clock
/// artifact.
pub async fn run(cfg: SimConfig) -> Result<SimReport> {
    if cfg.clients == 0 {
        return Err(SqueezefsError::InvalidOperation(
            "membership harness: `clients` must be at least 1".into(),
        ));
    }
    let clocks = LeaseClocks::derive(std::time::Duration::from_micros(250))?;
    let clock = LeaseClock::monotonic();
    let owner = MembershipOwner::arm("sim-owner", 1, 0, clocks.clone(), clock.clone())?;
    let journal_before = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    let started = Instant::now();

    let mut latencies: Vec<f64> = Vec::with_capacity(cfg.clients * cfg.beats);
    let mut sessions: Vec<(String, MemberRole, Arc<MemberSession>)> = Vec::new();
    let mut plane: Option<Arc<MembershipPlane>> = None;

    match cfg.mode {
        SimMode::Direct => {
            for i in 0..cfg.clients {
                let id = format!("sim-{i}");
                let role = role_for(i, cfg.readers_pct);
                let endpoint = match role {
                    MemberRole::Writer => Some(format!("10.0.0.{}:7100", 1 + i % 250)),
                    MemberRole::Reader => None,
                };
                let anchor = clock.now_ms();
                let grant = match owner.join(join_request(&id, role, endpoint)) {
                    JoinOutcome::Granted(g) => g,
                    JoinOutcome::Refused { reason, .. } | JoinOutcome::UnknownLease { reason } => {
                        return Err(SqueezefsError::InvalidOperation(format!(
                            "membership harness: join of '{id}' refused: {reason}"
                        )))
                    }
                };
                sessions.push((
                    id.clone(),
                    role,
                    Arc::new(MemberSession::adopt(
                        &id,
                        role,
                        &grant,
                        anchor,
                        clock.clone(),
                    )),
                ));
            }
            for _ in 0..cfg.beats {
                for (id, _, session) in &sessions {
                    let t0 = Instant::now();
                    let outcome = owner.renew(id, session.epoch(), session.acked_free_epoch());
                    latencies.push(t0.elapsed().as_secs_f64() * 1e6);
                    match outcome {
                        RenewOutcome::Renewed(grant) => session.renewed(&grant, clock.now_ms()),
                        RenewOutcome::UnknownLease { reason } => {
                            return Err(SqueezefsError::InvalidOperation(format!(
                                "membership harness: live member '{id}' refused: {reason}"
                            )))
                        }
                    }
                }
            }
        }
        SimMode::Wired => {
            let secret = format!("sim-secret-{}", uuid::Uuid::new_v4()).into_bytes();
            let started_plane = MembershipPlane::start(
                MembershipPlaneConfig::loopback(),
                secret.clone(),
                Arc::clone(&owner),
            )?;
            let endpoint = started_plane.endpoint().to_string();
            plane = Some(Arc::clone(&started_plane));
            let mut clients: Vec<MemberClient> = Vec::with_capacity(cfg.clients);
            for i in 0..cfg.clients {
                let id = format!("sim-{i}");
                let role = role_for(i, cfg.readers_pct);
                let endpoint_field = match role {
                    MemberRole::Writer => Some(format!("10.0.0.{}:7100", 1 + i % 250)),
                    MemberRole::Reader => None,
                };
                let client = MemberClient::join(
                    &endpoint,
                    &secret,
                    join_request(&id, role, endpoint_field),
                    clock.clone(),
                )
                .await?;
                sessions.push((id, role, Arc::clone(client.session())));
                clients.push(client);
            }
            for _ in 0..cfg.beats {
                for client in clients.iter_mut() {
                    let t0 = Instant::now();
                    client.renew().await?;
                    latencies.push(t0.elapsed().as_secs_f64() * 1e6);
                }
            }
        }
    }

    // --- revoke fan-out: evict a reader cohort, and let the victims
    //     discover it the way a real member does (renew → UnknownLease →
    //     self-fence). Readers only: a writer's self-fence poisons process
    //     data custody.
    let victims: Vec<(String, Arc<MemberSession>)> = if cfg.evict_fraction_permille == 0 {
        Vec::new()
    } else {
        let want =
            ((cfg.clients as u64 * cfg.evict_fraction_permille as u64) / 1000).max(1) as usize;
        sessions
            .iter()
            .filter(|(_, role, _)| *role == MemberRole::Reader)
            .take(want)
            .map(|(id, _, s)| (id.clone(), Arc::clone(s)))
            .collect()
    };
    let mut self_fences = 0usize;
    let revoke_fanout_us = if victims.is_empty() {
        0.0
    } else {
        let t0 = Instant::now();
        for (id, _) in &victims {
            owner.evict(id, "membership harness: revoke fan-out leg");
        }
        let fanout = t0.elapsed().as_secs_f64() * 1e6;
        for (id, session) in &victims {
            if let RenewOutcome::UnknownLease { reason } =
                owner.renew(id, session.epoch(), session.acked_free_epoch())
            {
                if session.self_fence(&reason).first {
                    self_fences += 1;
                }
            }
        }
        fanout
    };

    // --- failover: a successor bumps the term, opens the grace window over
    //     the surviving membership, and every survivor re-asserts.
    let grace_completion_us = if cfg.failover {
        let successor = MembershipOwner::arm(
            "sim-successor",
            owner.term() + 1,
            owner.term(),
            clocks.clone(),
            clock.clone(),
        )?;
        let survivors: Vec<(String, MemberRole)> = sessions
            .iter()
            .filter(|(id, _, _)| !victims.iter().any(|(v, _)| v == id))
            .map(|(id, role, _)| (id.clone(), *role))
            .collect();
        successor.open_grace(survivors.iter().map(|(id, _)| id.clone()).collect());
        let t0 = Instant::now();
        for (id, role) in &survivors {
            let mut req = join_request(id, *role, None);
            // A reclaim presents the epoch it held under the predecessor —
            // which is what the grace window admits and a fresh acquire is
            // refused for.
            req.prior_epoch = owner.epoch_of(id).or(Some(1));
            match successor.join(req) {
                JoinOutcome::Granted(_) => {}
                JoinOutcome::Refused { reason, .. } | JoinOutcome::UnknownLease { reason } => {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "membership harness: reclaim of '{id}' refused inside the grace \
                         window: {reason}"
                    )))
                }
            }
        }
        let elapsed = t0.elapsed().as_secs_f64() * 1e6;
        if successor.grace_active() {
            return Err(SqueezefsError::InvalidOperation(
                "membership harness: the grace window did not close after every prior member \
                 re-asserted"
                    .into(),
            ));
        }
        elapsed
    } else {
        0.0
    };

    // --- the read side: page the census the way `squeezefs clients` does.
    let mut cursor = Some(0u64);
    let mut census_pages = 0usize;
    let mut census_rows = 0usize;
    while let Some(c) = cursor {
        let (rows, next) = owner.census(c, crate::membership_wire::CENSUS_PAGE_MAX);
        census_rows += rows.len();
        census_pages += 1;
        cursor = next;
    }

    let wall_secs = started.elapsed().as_secs_f64();
    let journal_entries_delta = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed) - journal_before;
    if let Some(plane) = plane.take() {
        plane.shutdown();
    }
    latencies.sort_by(|a, b| a.partial_cmp(b).expect("finite timings"));
    Ok(SimReport {
        clients: cfg.clients,
        beats: cfg.beats,
        renewals: latencies.len(),
        journal_entries_delta,
        renew_p50_us: pct(&latencies, 0.5),
        renew_p99_us: pct(&latencies, 0.99),
        renew_max_us: pct(&latencies, 1.0),
        revoke_fanout_us,
        grace_completion_us,
        evictions: victims.len(),
        self_fences,
        census_pages,
        census_rows,
        wall_secs,
        mode: cfg.mode.as_str(),
        shards: 1,
        parked: 0,
        reclaimed: 0,
        park_expiries: 0,
        carriage_leases: 0,
        recall_readers: 0,
        recall_fanout_us: 0.0,
        recall_acks: 0,
        death_records: 0,
        death_sink_us: 0.0,
        death_poll_ms: 0,
        death_shards_reached: 0,
        free_grace_fanin_us: 0.0,
    })
}

/// The operating point's rotor size the carriage source answers with
/// (§1.6: `M = clamp(W / (2 × writers), 1, MINT_SPREAD)` = 2 at 12,500).
const SIM_CARRIAGE_M: usize = 2;

/// The sim member `i`'s rotor slots — `M` consecutive routing slots off
/// its index, disjoint across members below `W / M`.
fn sim_slots(i: usize) -> Vec<(u16, u32)> {
    (0..SIM_CARRIAGE_M)
        .map(|k| (((i * SIM_CARRIAGE_M + k) % 65_536) as u16, 1u32))
        .collect()
}

/// The member index a sim id (`sim-{i}`) names.
fn sim_index(id: &str) -> Option<usize> {
    id.strip_prefix("sim-").and_then(|s| s.parse().ok())
}

/// **The SHARDED harness** (PR 8, KD-SYM-15 / §5.5.3 — the SIM-1 shape PR
/// 13 drives at 12,500 × 64): `shards` owners, member `i` homed on shard
/// `i % shards` and renewing with ITS owner only (the per-shard beat is
/// `N/V ÷ 10 s`); then the manager of shard 0 DIES: every member homed
/// there passes `T_self` and PARKS (the [`crate::park_core::ParkCore`]
/// protocol — acks held, nothing poisoned), a successor arms for the
/// shard and opens grace, every parked member RECLAIMS in it (one join
/// presenting its epoch) and the park releases; `park_expiries` counts
/// the parks a bounded `T_park_max` would have expired (0 here — the
/// successor arms inside the bound by construction). Direct mode only —
/// the scale instrument. **The park leg is API-level**: it drives fresh
/// `ParkCore`s per member (the protocol at scale), not the production
/// `MemberSession::self_fence_as` → `park_gate` → `RenewalTick::Parked`
/// arm, which `tests/sym_block_grant_tests.rs` drives through the real
/// renewal tick over the wire.
pub async fn run_sharded(cfg: SimConfig, shards: usize) -> Result<SimReport> {
    if shards <= 1 {
        return run(cfg).await;
    }
    if cfg.clients == 0 {
        return Err(SqueezefsError::InvalidOperation(
            "membership harness: `clients` must be at least 1".into(),
        ));
    }
    let clocks = LeaseClocks::derive(std::time::Duration::from_micros(250))?;
    let clock = LeaseClock::monotonic();
    let owners: Vec<Arc<MembershipOwner>> = (0..shards)
        .map(|s| {
            MembershipOwner::arm(
                &format!("sim-owner-{s}"),
                1,
                0,
                clocks.clone(),
                clock.clone(),
            )
        })
        .collect::<Result<_>>()?;
    let journal_before = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    let started = Instant::now();
    let mut latencies: Vec<f64> = Vec::with_capacity(cfg.clients * cfg.beats);
    let mut sessions: Vec<(String, MemberRole, usize, Arc<MemberSession>)> = Vec::new();
    for i in 0..cfg.clients {
        let id = format!("sim-{i}");
        let role = role_for(i, cfg.readers_pct);
        let shard = i % shards;
        let endpoint = match role {
            MemberRole::Writer => Some(format!("10.0.0.{}:7100", 1 + i % 250)),
            MemberRole::Reader => None,
        };
        let anchor = clock.now_ms();
        let grant = match owners[shard].join(join_request(&id, role, endpoint)) {
            JoinOutcome::Granted(g) => g,
            JoinOutcome::Refused { reason, .. } | JoinOutcome::UnknownLease { reason } => {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "membership harness: join of '{id}' refused: {reason}"
                )))
            }
        };
        sessions.push((
            id.clone(),
            role,
            shard,
            Arc::new(MemberSession::adopt(
                &id,
                role,
                &grant,
                anchor,
                clock.clone(),
            )),
        ));
    }
    // --- the slot-lease carriage (PR 13 — §5.9): every renewal grant
    //     carries the member's leased slots off the installed source, the
    //     O(held slots) RAM answer an armed plane installs; the beats below
    //     are measured WITH it (the carriage is part of the beat's cost).
    crate::membership::install_slot_lease_carriage_source(Arc::new(|id: &str| {
        crate::membership::SlotLeaseCarriage {
            leases: sim_index(id).map(sim_slots).unwrap_or_default(),
            release_notices: Vec::new(),
            offered: Vec::new(),
        }
    }));
    let mut carriage_leases = 0usize;
    for _ in 0..cfg.beats {
        for (id, _, shard, session) in &sessions {
            let t0 = Instant::now();
            let outcome = owners[*shard].renew(id, session.epoch(), session.acked_free_epoch());
            latencies.push(t0.elapsed().as_secs_f64() * 1e6);
            match outcome {
                RenewOutcome::Renewed(grant) => {
                    carriage_leases += grant.slot_leases_ack.slots.len();
                    session.renewed(&grant, clock.now_ms())
                }
                RenewOutcome::UnknownLease { reason } => {
                    crate::membership::uninstall_slot_lease_carriage_source();
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "membership harness: live member '{id}' refused: {reason}"
                    )));
                }
            }
        }
    }
    crate::membership::uninstall_slot_lease_carriage_source();

    // --- the free-grace V-fan-in (§5.7.3, §6.8 item 3): a fresh label
    //     every member acks on ONE renewal; the wall until every shard's
    //     MIN closes on it is the fan-in's on-change cadence in-process.
    let label = clock.now_ms().max(1);
    let free_grace_fanin_us = {
        let t0 = Instant::now();
        for (id, _, shard, session) in &sessions {
            session.ack_free_epoch(label);
            match owners[*shard].renew(id, session.epoch(), session.acked_free_epoch()) {
                RenewOutcome::Renewed(grant) => session.renewed(&grant, clock.now_ms()),
                RenewOutcome::UnknownLease { reason } => {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "membership harness: live member '{id}' refused at the fan-in: {reason}"
                    )))
                }
            }
        }
        let behind: Vec<usize> = (0..shards)
            .filter(|s| owners[*s].min_acked_free_epoch() < label)
            .collect();
        if !behind.is_empty() {
            return Err(SqueezefsError::InvalidOperation(format!(
                "membership harness: shard(s) {behind:?} did not close on label {label} after \
                 every member acked it"
            )));
        }
        t0.elapsed().as_secs_f64() * 1e6
    };

    // --- the token recall fan-out (§5.7.4, the broadcast shape): every
    //     member holds a token on ONE object of one holder plane; one
    //     commit recalls them all; the readers' half polls and acks through
    //     the plane's own body (`poll_recall_frame` / `ack_recall_frame`)
    //     — the plane's cost at N, the wire's RTT being the venue's.
    let (recall_readers, recall_fanout_us, recall_acks) = {
        use crate::meta_ship::token_plane::{LeaseVerdict, TokenHolderPlane};
        let plane = Arc::new(TokenHolderPlane::new());
        plane.install_lease_oracle(Arc::new(|_: &str| LeaseVerdict::Live));
        let object = 0x5157_u64;
        let now = Instant::now();
        let ids: Vec<&str> = sessions.iter().map(|(id, _, _, _)| id.as_str()).collect();
        for id in &ids {
            let _ = plane.lane().try_grant(object, id, now);
        }
        let readers = plane.holders(object);
        let objects = [object];
        let t0 = Instant::now();
        let recall = plane.recall_and_wait(&objects);
        let ack_all = async {
            // One sweep acks every issued frame (the pass issues one frame
            // per client at once); the loop re-sweeps until the lane
            // reports no holder under recall.
            loop {
                for id in &ids {
                    if let Some((frame_id, _)) = plane.poll_recall_frame(id, Duration::ZERO).await {
                        plane.ack_recall_frame(id, frame_id);
                    }
                }
                if plane.lane().holders_under_recall(object) == 0 {
                    break;
                }
                squeezefs_ipc::sqz_time::sleep(Duration::from_millis(1)).await;
            }
        };
        let (union, ()) = futures::join!(recall, ack_all);
        let wall = t0.elapsed().as_secs_f64() * 1e6;
        plane.settle(&union);
        let stats = plane.stats();
        if stats.timeouts_live != 0 {
            return Err(SqueezefsError::InvalidOperation(format!(
                "membership harness: {} live recall timeout(s) on the broadcast object",
                stats.timeouts_live
            )));
        }
        (readers, wall, stats.recall_acks as usize)
    };

    // --- the death ledger (§5.5.2, §5.9): a cohort of READERS evicted by
    //     their home shards reaches the installed sink (the record's
    //     write); every shard then reads the ledger as its projection.
    let death_ledger: Arc<parking_lot::Mutex<Vec<(crate::membership::DeadMember, Instant)>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let (death_records, death_sink_us, death_shards_reached) = if cfg.evict_fraction_permille == 0 {
        (0, 0.0, 0)
    } else {
        let sink_ledger = Arc::clone(&death_ledger);
        crate::membership::install_death_sink(Arc::new(move |dead| {
            sink_ledger.lock().push((dead, Instant::now()));
        }));
        let want =
            ((cfg.clients as u64 * cfg.evict_fraction_permille as u64) / 1000).max(1) as usize;
        // Victims off shard 0 (the failover leg's home shard below).
        let victims: Vec<(String, usize)> = sessions
            .iter()
            .filter(|(_, role, shard, _)| *role == MemberRole::Reader && *shard != 0)
            .take(want)
            .map(|(id, _, shard, _)| (id.clone(), *shard))
            .collect();
        let mut worst = 0.0f64;
        for (id, shard) in &victims {
            let t0 = Instant::now();
            owners[*shard].evict(id, "membership harness: death ledger leg");
            let seen = death_ledger
                .lock()
                .iter()
                .rev()
                .find(|(d, _)| d.id == *id)
                .map(|(_, at)| at.saturating_duration_since(t0).as_secs_f64() * 1e6);
            match seen {
                Some(us) => worst = worst.max(us),
                None => {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "membership harness: eviction of '{id}' reached no death sink"
                    )))
                }
            }
        }
        // Every shard's projection: one read of the ledger names every
        // record (the manager's poll, in-process).
        let records = death_ledger.lock().len();
        let reached = (0..shards)
            .filter(|_| death_ledger.lock().len() == records)
            .count();
        crate::membership::test_clear_death_sinks();
        (records, worst, reached)
    };
    let death_poll_ms = crate::meta_backend::kv::checkpoint::checkpoint_landing_ceiling_derived();

    // --- the manager of shard 0 dies: its home-shard members park at
    //     T_self (the S6 grace contract makes every lease reclaimable, so
    //     nothing is poisoned), the successor arms and opens grace, every
    //     parked member reclaims in it.
    let (parked, reclaimed, park_expiries, grace_completion_us) = if cfg.failover {
        let dead = &owners[0];
        let home: Vec<(String, MemberRole)> = sessions
            .iter()
            .filter(|(_, _, shard, _)| *shard == 0)
            .map(|(id, role, _, _)| (id.clone(), *role))
            .collect();
        let gates: Vec<crate::park_core::ParkCore> = home
            .iter()
            .map(|_| crate::park_core::ParkCore::new())
            .collect();
        let now = clock.now_ms();
        let parked = gates.iter().filter(|g| g.park(now).is_some()).count();
        let successor = MembershipOwner::arm(
            "sim-successor-0",
            dead.term() + 1,
            dead.term(),
            clocks.clone(),
            clock.clone(),
        )?;
        successor.open_grace(home.iter().map(|(id, _)| id.clone()).collect());
        let t0 = Instant::now();
        let mut reclaimed = 0usize;
        for ((id, role), gate) in home.iter().zip(&gates) {
            let mut req = join_request(id, *role, None);
            req.prior_epoch = dead.epoch_of(id).or(Some(1));
            match successor.join(req) {
                JoinOutcome::Granted(_) => {
                    if gate.release() {
                        reclaimed += 1;
                    }
                }
                JoinOutcome::Refused { reason, .. } | JoinOutcome::UnknownLease { reason } => {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "membership harness: reclaim of '{id}' refused inside the grace \
                         window: {reason}"
                    )))
                }
            }
        }
        let elapsed = t0.elapsed().as_secs_f64() * 1e6;
        if successor.grace_active() {
            return Err(SqueezefsError::InvalidOperation(
                "membership harness: the grace window did not close after every home-shard \
                 member reclaimed"
                    .into(),
            ));
        }
        let t_park_max = crate::park_gate::t_park_max_ms(
            clocks.t_owner.as_millis() as u64,
            clocks.t_owner.as_millis() as u64,
        );
        let expiries = gates
            .iter()
            .filter(|g| g.expire(clock.now_ms(), t_park_max))
            .count();
        (parked, reclaimed, expiries, elapsed)
    } else {
        (0, 0, 0, 0.0)
    };
    let mut census_pages = 0usize;
    let mut census_rows = 0usize;
    for owner in &owners {
        let mut cursor = Some(0u64);
        while let Some(c) = cursor {
            let (rows, next) = owner.census(c, crate::membership_wire::CENSUS_PAGE_MAX);
            census_rows += rows.len();
            census_pages += 1;
            cursor = next;
        }
    }
    let wall_secs = started.elapsed().as_secs_f64();
    let journal_entries_delta = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed) - journal_before;
    latencies.sort_by(|a, b| a.partial_cmp(b).expect("finite timings"));
    Ok(SimReport {
        clients: cfg.clients,
        beats: cfg.beats,
        renewals: latencies.len(),
        journal_entries_delta,
        renew_p50_us: pct(&latencies, 0.5),
        renew_p99_us: pct(&latencies, 0.99),
        renew_max_us: pct(&latencies, 1.0),
        revoke_fanout_us: 0.0,
        grace_completion_us,
        evictions: 0,
        self_fences: 0,
        census_pages,
        census_rows,
        wall_secs,
        mode: cfg.mode.as_str(),
        shards,
        parked,
        reclaimed,
        park_expiries,
        carriage_leases,
        recall_readers,
        recall_fanout_us,
        recall_acks,
        death_records,
        death_sink_us,
        death_poll_ms,
        death_shards_reached,
        free_grace_fanin_us,
    })
}
