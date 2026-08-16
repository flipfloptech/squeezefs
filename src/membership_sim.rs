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
use std::time::Instant;

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
             census: {rows} row(s) in {pages} page(s)",
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
    })
}
