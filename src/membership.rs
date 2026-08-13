//! DLM **stage S6** — membership off the journal (lease-based liveness) —
//! and **spec §6.2 item 7**, the claim-set record
//! (`docs/pre-rc-engineering-spec.md` §6.5 item 3, §6.7 "Recovery" + "Two
//! lease clocks", §6.9 S6; `docs/pre-rc-execution-plan.md` Phase 4;
//! contracts in `tests/dlm_membership_tests.rs`).
//!
//! # The measured problem
//!
//! §6.5 item 3, verbatim: each client writes `client:{uuid}` **every 10 s
//! as a full journal transaction** under an exclusive `I{1}` guard, and
//! ino 1 routes to slot 0 → **one volume unconditionally**. At the measured
//! saturated `commit_tx_wait` of 2.198 ms that volume serializes **455
//! beats/s** against the **1,500/s** 15,000 clients require; adding volumes
//! does not help, because these are xattrs on a pinned inode. Past
//! saturation records age past the **45 s TTL** and every liveness consumer
//! starts treating live mounts as dead. **The read side is worse**:
//! `mount_registrations()` is `listxattr(1)` plus one `getxattr` per
//! client, each under a shared `I{1}` lock, per `squeezefs clients`, per
//! `status`, and in the format preflight.
//!
//! # The mechanism, and why the alternatives lose
//!
//! **Chosen (spec §6.5's option (a)): a durable registration written ONCE,
//! liveness carried by lease renewal over `cluster_wire` with the lease
//! table in RAM.** Two durable footprints, neither per-beat:
//!
//! * [`OwnerRecord`] — ONE key ([`MEMBERSHIP_OWNER_XATTR`]) written at the
//!   owner's arm and deleted at its disarm. It is the *rendezvous*: where
//!   to find the census, which era it belongs to, and what the lease TTL
//!   is. DISC-1 needs exactly this and nothing more (the shared volume IS
//!   the rendezvous — ruling D2), and one key is `O(1)` to read whatever
//!   the member count is.
//! * [`ClaimSet`] — §6.2 item 7's durable **partition membership** for
//!   WRITERS, rewritten on membership CHANGE only (never on a beat), behind
//!   incompat bit 14. Un-engaged it is a *projection* of `writer_claim`, so
//!   single-writer volumes keep byte-identical records.
//!
//! **Why (b), a dedicated non-journaled checksummed membership slab, loses.**
//! It still pays a device write plus a barrier per beat per client — 1,500
//! device writes/s at 15 k clients — so the write plane's cost merely moves
//! off the journal onto the same namespace, and the read side stays a
//! device scan. It also needs its own torn-write discipline, allocator, GC
//! and fsck class, duplicating what the CoW-checksummed journal already
//! guarantees (`docs/design-cow-kv-metadata.md` §4.10). And it buys
//! nothing, because **durability of liveness is a category error**: the
//! only consumer of a durable liveness record is a process that crashed,
//! and a crashed process's liveness answer is "dead" — which is exactly
//! what the ABSENCE of a renewal already says. Durable state should record
//! *identity and custody*, which survive a crash and must be recovered;
//! liveness is by definition not that.
//!
//! **Why gossip/multicast loses.** Ruling D2: peers are auto-discovered and
//! never manually configured, and the shared volume is already a
//! rendezvous every member can read. A gossip plane adds an O(N²) message
//! surface and a second failure model (partitioned membership views) to
//! answer a question one authority already answers exactly.
//!
//! # What survives an owner crash
//!
//! Lease state is RAM-only **by design**, and recovery is **re-assertion**
//! (NFSv4 style — §6.7 "Recovery"): the D0 ladder elects the successor, the
//! successor **bumps `term` durably before arming** (which makes every
//! old-era token and DMA authorization stale by construction — DLM S2/S7),
//! and then opens a **grace window** admitting only reclaim and refusing
//! conflicting fresh acquires. [`MembershipOwner::arm`] REFUSES a term that
//! is not strictly greater than the predecessor's: the ordering law is
//! code, not a comment. Without the window, failover triggers a
//! cluster-wide forced-flush storm at the worst possible moment.
//!
//! # Two lease clocks, and the client's is stricter
//!
//! `T_self = T_owner − 2·skew_max − D_purge` ([`LeaseClocks`]), both on
//! monotonic clocks anchored on the RPC round trip: the client anchors on
//! its **send** instant, so the round trip counts against the client too. A
//! member that cannot renew by `T_self` fail-stops the affected objects
//! **itself** ([`MemberSession::self_fence`]) before the owner can grant
//! them elsewhere; false-positive eviction then costs availability, never
//! divergence. A configuration where the inequality collapses is a
//! **refusal**, not a clamp — a plane that cannot give the client the
//! stricter clock must not arm.
//!
//! # Readers, and the §6.8 item-3 channel
//!
//! A reader performs **no metadata write** (spec §6.8 item 1 refuses them
//! by contract), which is why S5 shipped with readers invisible to
//! `squeezefs clients`. Here a reader is a wire-only member: it appears in
//! the census with zero durable footprint. That same channel is what §6.8
//! item 3 (the freed-offset grace period — "the highest-value single item
//! in the coherence analysis") will call:
//!
//! | Item-3 need | API |
//! |---|---|
//! | reader acknowledges the freed-offset epoch it has passed | [`MemberSession::ack_free_epoch`] (rides the next renewal) |
//! | writer's reallocation bound | [`MembershipOwner::min_acked_free_epoch`] |
//! | name the laggards | [`MembershipOwner::members_behind_free_epoch`] |
//! | "a reader that fails to acknowledge is fenced, not waited on" | [`MembershipOwner::evict`] |
//!
//! **Item 3 is BUILT and consumes exactly that table** — the gate itself
//! lives in [`crate::free_grace`] (the ring, the bound, the fence and the
//! reader's acknowledgement ladder) and is enforced at
//! `BlockAllocator::finish_free` / the allocation funnel. This module owns
//! only the channel: [`MembershipOwner::refresh_free_grace_bound`]
//! publishes the reallocation bound on the owner's cadence, and
//! [`MemberSession::learned_label`] hands the reader the causal label its
//! acknowledgement echoes.

use crate::cluster_wire::hex_decode;
use crate::error::{Result, SqueezefsError};
use crate::fuse_client::{CLIENT_HEARTBEAT_INTERVAL_SECS, CLIENT_STALE_TTL_SECS, METRICS};
use crate::meta_backend::kv::backend::{KvMetaBackend, WriterClaim};
use crate::meta_backend::kv::superblock::FEATURE_INCOMPAT_KV_CLAIM_SET;
use arc_swap::ArcSwapOption;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// The membership plane's **rendezvous record**: ONE xattr at local ino 1
/// beside `writer_claim`, written at the owner's arm and removed at its
/// disarm — never per beat (that is the whole point of S6).
///
/// Per-volume control state, exactly like `writer_claim` and `writer_term`:
/// it never travels with a migrating slot, because it names the process
/// that owns THIS volume's membership authority.
pub const MEMBERSHIP_OWNER_XATTR: &str = "membership_owner";

/// §6.2 **item 7**: the durable claim-SET record — the writers that are
/// members of this volume set, each with its identity, endpoint and NVMe
/// registrant key. Present only on volumes carrying incompat bit 14
/// ([`FEATURE_INCOMPAT_KV_CLAIM_SET`]); un-engaged volumes read the
/// singleton projection of `writer_claim` instead, so their bytes are
/// untouched.
pub const CLAIM_SET_XATTR: &str = "claim_set";

/// A member's role on the plane. A reader takes no lease on any object
/// (§6.8: "Readers take no leases") but IS a lease-bearing member of the
/// plane — that is what makes it visible and what gives §6.8 item 3 an
/// acknowledgement channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MemberRole {
    /// A write mount: holds custody, mints fencing tokens, submits DMA.
    Writer,
    /// A read-only coherent mount (DLM S5). Writes nothing, anywhere.
    Reader,
}

impl MemberRole {
    /// The operator-facing word (also the `squeezefs clients` row kind).
    pub fn as_str(self) -> &'static str {
        match self {
            MemberRole::Writer => "writer",
            MemberRole::Reader => "reader",
        }
    }

    /// Parse the stored/wire word. `None` for anything else — an
    /// unattributable role is never guessed.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "writer" => Some(MemberRole::Writer),
            "reader" => Some(MemberRole::Reader),
            _ => None,
        }
    }
}

/// Who a member is — the durable half of membership (identity survives a
/// crash and must be recovered; liveness does not).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberIdentity {
    /// Per-mount uuid.
    pub id: String,
    /// Writer or reader.
    pub role: MemberRole,
    /// Holder pid (same-host diagnosis; `boot` scopes it).
    pub pid: u32,
    /// Holder boot id — scopes the pid proof to this boot, exactly as
    /// `WriterClaim.boot` does.
    pub boot: String,
    /// The member's membership-plane endpoint (`ip:port`), when it hosts
    /// one. `None` for a reader and for any member that dials but does not
    /// serve — DISC-1 requires an endpoint to call a member a peer.
    pub endpoint: Option<String>,
    /// The member's NVMe **registrant key** under the data namespaces'
    /// shared WERO hold (`0` = none / detection-grade substrate). This is
    /// the PR half of §6.2 item 7: a claim SET is expressed at the device
    /// as several registrants, never as several reservation holders — the
    /// hold itself is joined through [`crate::data_custody::acquire_wero`]
    /// and never forked.
    pub pr_key: u64,
}

/// One durable member of the claim set, with the timestamp of the change
/// that recorded it (a *change* stamp, not a heartbeat — nothing refreshes
/// it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimSetMember {
    /// Identity + registrant key.
    pub identity: MemberIdentity,
    /// Unix seconds of the membership change that recorded this member.
    pub ts: u64,
}

/// §6.2 **item 7**: the claim set — "multiple writers are members of one
/// volume set, with their identities durable".
///
/// Read through [`ClaimSet::load`], which answers ONE shape whether or not
/// the volume carries bit 14: engaged volumes decode the durable record,
/// un-engaged volumes get the singleton projection of `writer_claim`
/// ([`ClaimSet::from_writer_claim`], `durable == false`). That projection is
/// what keeps single-writer byte-identical: nothing is written, nothing is
/// added to `listxattr(1)`, and the claim's bytes are untouched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimSet {
    /// Record schema.
    pub v: u32,
    /// The durable writer era the set belongs to (DLM S2's
    /// `WriterClaim.term`). A successor's set always names a greater term.
    pub term: u64,
    /// The member writers.
    pub members: Vec<ClaimSetMember>,
    /// `true` = decoded from the durable `claim_set` record; `false` = the
    /// projection of a singular `writer_claim`. Never serialized — it is a
    /// property of where the answer came from.
    #[serde(skip)]
    pub durable: bool,
}

impl ClaimSet {
    /// An empty set in `term`.
    pub fn empty(term: u64) -> Self {
        Self {
            v: 1,
            term,
            members: Vec::new(),
            durable: false,
        }
    }

    /// The **projection** every un-engaged volume answers with: the
    /// singular `writer_claim` as a one-member set. Consumers therefore
    /// never branch on engagement.
    pub fn from_writer_claim(claim: &WriterClaim) -> Self {
        Self {
            v: 1,
            term: claim.term,
            members: vec![ClaimSetMember {
                identity: MemberIdentity {
                    id: claim.id.clone(),
                    role: MemberRole::Writer,
                    pid: claim.pid,
                    boot: claim.boot.clone(),
                    endpoint: None,
                    pr_key: 0,
                },
                ts: claim.ts,
            }],
            durable: false,
        }
    }

    /// Encode the durable record (compact JSON — the `writer_claim` /
    /// `client:{id}` family's format, so an operator can read it with the
    /// same tools).
    pub fn encode(&self) -> Vec<u8> {
        serde_json::json!({
            "v": self.v,
            "term": self.term,
            "members": self.members.iter().map(|m| serde_json::json!({
                "id": m.identity.id,
                "role": m.identity.role.as_str(),
                "pid": m.identity.pid,
                "boot": m.identity.boot,
                "endpoint": m.identity.endpoint,
                "pr_key": m.identity.pr_key,
                "ts": m.ts,
            })).collect::<Vec<_>>(),
        })
        .to_string()
        .into_bytes()
    }

    /// Decode a stored record. `None` for anything unparseable — a claim
    /// set we cannot attribute proves nothing, exactly like an
    /// unparseable `writer_claim`.
    pub fn decode(raw: &[u8]) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_slice(raw).ok()?;
        let mut members = Vec::new();
        for m in v.get("members")?.as_array()? {
            members.push(ClaimSetMember {
                identity: MemberIdentity {
                    id: m.get("id")?.as_str()?.to_string(),
                    role: MemberRole::parse(m.get("role")?.as_str()?)?,
                    pid: m.get("pid")?.as_u64()? as u32,
                    boot: m.get("boot")?.as_str()?.to_string(),
                    endpoint: m
                        .get("endpoint")
                        .and_then(|e| e.as_str())
                        .map(str::to_string),
                    pr_key: m.get("pr_key").and_then(|k| k.as_u64()).unwrap_or(0),
                },
                ts: m.get("ts").and_then(|t| t.as_u64()).unwrap_or(0),
            });
        }
        Some(Self {
            v: v.get("v").and_then(|x| x.as_u64()).unwrap_or(1) as u32,
            term: v.get("term").and_then(|x| x.as_u64()).unwrap_or(0),
            members,
            durable: true,
        })
    }

    /// Every non-zero NVMe registrant key in the set — the PR half of
    /// item 7. A successor uses it to know which registrants are
    /// legitimate members and which are zombies to preempt; it never
    /// implies a second reservation (one shared [`crate::data_custody`]
    /// hold, joined).
    pub fn registrant_keys(&self) -> Vec<u64> {
        self.members
            .iter()
            .filter(|m| m.identity.pr_key != 0)
            .map(|m| m.identity.pr_key)
            .collect()
    }

    /// The set's writer members.
    pub fn writers(&self) -> impl Iterator<Item = &ClaimSetMember> {
        self.members
            .iter()
            .filter(|m| m.identity.role == MemberRole::Writer)
    }

    /// Read the set: the durable record when engaged, else the projection
    /// of `writer_claim`, else `None` (an unclaimed volume).
    pub async fn load(be: &KvMetaBackend) -> Option<Self> {
        if claim_set_engaged(be.superblock().features_incompat) {
            if let Ok(Some(raw)) = be.getxattr(1, CLAIM_SET_XATTR).await {
                if let Some(set) = Self::decode(&raw) {
                    return Some(set);
                }
                log::warn!(
                    "claim_set record on {} is undecodable — falling back to the singular \
                     writer_claim projection (an unattributable set proves nothing)",
                    be.device_path().display()
                );
            }
        }
        be.read_writer_claim()
            .await
            .map(|c| Self::from_writer_claim(&c))
    }

    /// Store the durable record. **Refuses** on a volume that does not
    /// carry bit 14: that refusal is what makes single-writer
    /// byte-identity a law rather than an intention.
    pub async fn store(be: &KvMetaBackend, set: &Self) -> Result<()> {
        if !claim_set_engaged(be.superblock().features_incompat) {
            return Err(SqueezefsError::InvalidOperation(format!(
                "refusing to write a claim_set record on {}: the format does not carry the \
                 claim-set capability (incompat bit 14). Nothing stamps it today (ruling D9: \
                 the bit is built, not stamped) — an un-engaged volume's membership IS the \
                 singular writer_claim, read through its projection, and writing this record \
                 would change what an un-stamped volume's records look like",
                be.device_path().display()
            )));
        }
        be.setxattr_internal(1, CLAIM_SET_XATTR, &set.encode())
            .await?;
        METRICS
            .membership_registration_commits
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Remove the durable record (clean departure of the last member).
    pub async fn clear(be: &KvMetaBackend) -> Result<()> {
        match be.getxattr(1, CLAIM_SET_XATTR).await {
            Ok(Some(_)) => be.removexattr_internal(1, CLAIM_SET_XATTR).await,
            _ => Ok(()),
        }
    }
}

/// Record `identity` as a member of `be`'s claim set (§6.2 **item 7**),
/// preserving every other member's entry — ONE commit per membership
/// CHANGE, never per beat.
///
/// Returns `false` (and writes NOTHING) on a volume that does not carry
/// incompat bit 14, which is every volume today (ruling D9). That is the
/// single-writer byte-identity law in the one place a mount would otherwise
/// have created a new record: the projection of `writer_claim` stays the
/// whole truth, and `listxattr(1)` gains nothing.
pub async fn upsert_writer_member(
    be: &KvMetaBackend,
    identity: &MemberIdentity,
    term: u64,
) -> Result<bool> {
    if !claim_set_engaged(be.superblock().features_incompat) {
        return Ok(false);
    }
    let mut set = match be.getxattr(1, CLAIM_SET_XATTR).await {
        Ok(Some(raw)) => ClaimSet::decode(&raw).unwrap_or_else(|| ClaimSet::empty(term)),
        _ => ClaimSet::empty(term),
    };
    set.term = set.term.max(term);
    set.members.retain(|m| m.identity.id != identity.id);
    set.members.push(ClaimSetMember {
        identity: identity.clone(),
        ts: unix_now_secs(),
    });
    set.members
        .sort_by(|a, b| a.identity.id.cmp(&b.identity.id));
    ClaimSet::store(be, &set).await?;
    log::info!(
        "claim set on {}: writer '{}' is a durable member in term {} ({} member(s),          registrant key {:#x}) — §6.2 item 7",
        be.device_path().display(),
        identity.id,
        set.term,
        set.members.len(),
        identity.pr_key
    );
    Ok(true)
}

/// Remove a writer from `be`'s claim set (clean departure), deleting the
/// record once it empties so a departed set presents as unclaimed. `false`
/// = nothing to do (un-engaged volume, or not a member).
pub async fn withdraw_writer_member(be: &KvMetaBackend, id: &str) -> Result<bool> {
    if !claim_set_engaged(be.superblock().features_incompat) {
        return Ok(false);
    }
    let Ok(Some(raw)) = be.getxattr(1, CLAIM_SET_XATTR).await else {
        return Ok(false);
    };
    let Some(mut set) = ClaimSet::decode(&raw) else {
        return Ok(false);
    };
    let before = set.members.len();
    set.members.retain(|m| m.identity.id != id);
    if set.members.len() == before {
        return Ok(false);
    }
    if set.members.is_empty() {
        ClaimSet::clear(be).await?;
    } else {
        ClaimSet::store(be, &set).await?;
    }
    Ok(true)
}

/// `true` ⇔ this volume's format expresses a claim SET (incompat bit 14).
/// Every volume today answers `false` — ruling **D9**: the bit is built,
/// nothing stamps it.
pub fn claim_set_engaged(features_incompat: u64) -> bool {
    features_incompat & FEATURE_INCOMPAT_KV_CLAIM_SET != 0
}

/// The rendezvous record ([`MEMBERSHIP_OWNER_XATTR`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerRecord {
    /// Record schema.
    pub v: u32,
    /// The owner's per-mount uuid.
    pub id: String,
    /// The durable writer era the owner armed in (DLM S2). A member that
    /// reads a record naming a term below the one it last saw knows it is
    /// looking at a stale rendezvous.
    pub term: u64,
    /// Where the census lives (`ip:port` on the ONE cluster transport).
    pub endpoint: String,
    /// The owner's lease TTL in ms — published so a member derives ITS
    /// stricter clock from the owner's actual parameter rather than from a
    /// local assumption.
    pub ttl_ms: u64,
    /// Unix seconds of the arm (a change stamp — nothing refreshes it).
    pub ts: u64,
    /// Owner pid.
    pub pid: u32,
    /// Owner boot id.
    pub boot: String,
}

impl OwnerRecord {
    /// Encode as compact JSON (the record family's format).
    pub fn encode(&self) -> Vec<u8> {
        serde_json::json!({
            "v": self.v,
            "id": self.id,
            "term": self.term,
            "endpoint": self.endpoint,
            "ttl_ms": self.ttl_ms,
            "ts": self.ts,
            "pid": self.pid,
            "boot": self.boot,
        })
        .to_string()
        .into_bytes()
    }

    /// Decode a stored record; `None` when unparseable.
    pub fn decode(raw: &[u8]) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_slice(raw).ok()?;
        Some(Self {
            v: v.get("v").and_then(|x| x.as_u64()).unwrap_or(1) as u32,
            id: v.get("id")?.as_str()?.to_string(),
            term: v.get("term").and_then(|x| x.as_u64()).unwrap_or(0),
            endpoint: v.get("endpoint")?.as_str()?.to_string(),
            ttl_ms: v
                .get("ttl_ms")
                .and_then(|x| x.as_u64())
                .unwrap_or(CLIENT_STALE_TTL_SECS * 1000),
            ts: v.get("ts").and_then(|x| x.as_u64()).unwrap_or(0),
            pid: v.get("pid").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            boot: v
                .get("boot")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
        })
    }
}

/// Publish the rendezvous record — ONE commit, at the owner's arm.
///
/// This is the ONLY durable write the liveness plane performs per mount,
/// and it is counted (`membership_registration_commits`) precisely so a
/// regression that starts writing it per beat is visible as growth.
pub async fn publish_owner_record(be: &KvMetaBackend, rec: &OwnerRecord) -> Result<()> {
    be.setxattr_internal(1, MEMBERSHIP_OWNER_XATTR, &rec.encode())
        .await?;
    METRICS
        .membership_registration_commits
        .fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Read the rendezvous record (one `getxattr` of one key — the read side's
/// whole metadata cost, whatever the member count).
pub async fn read_owner_record(be: &KvMetaBackend) -> Option<OwnerRecord> {
    match be.getxattr(1, MEMBERSHIP_OWNER_XATTR).await {
        Ok(Some(raw)) => OwnerRecord::decode(&raw),
        _ => None,
    }
}

/// Remove the rendezvous record (clean disarm). A crashed owner leaves it
/// behind; a member then finds an endpoint that refuses to answer, which is
/// the same evidence its lease renewal already gives.
pub async fn clear_owner_record(be: &KvMetaBackend) -> Result<()> {
    match be.getxattr(1, MEMBERSHIP_OWNER_XATTR).await {
        Ok(Some(_)) => be.removexattr_internal(1, MEMBERSHIP_OWNER_XATTR).await,
        _ => Ok(()),
    }
}

/// Unix seconds — the record family's timestamp base (a *change* stamp on
/// these records, never a heartbeat).
pub fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The cluster's storage-trust secret (`job:enroll`) read from ONE volume —
/// the sibling of [`crate::job_wire::read_enroll_secret`], which takes the
/// routed set. Possession of volume access IS cluster membership (ruling
/// D2), so the membership plane authenticates against the same record the
/// job wire does rather than minting a second root of trust.
pub async fn cluster_secret(be: &KvMetaBackend) -> Option<Vec<u8>> {
    let raw = be
        .getxattr(1, crate::job_wire::JOB_ENROLL_XATTR)
        .await
        .ok()??;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    hex_decode(v.get("secret")?.as_str()?)
}

// ---------------------------------------------------------------------------
// Clocks
// ---------------------------------------------------------------------------

/// The plane's monotonic clock, in milliseconds. `Monotonic` is production;
/// `Manual` is the deterministic test seam (the
/// [`crate::cluster_wire::WireClock`] precedent — lease expiry must be
/// provable without a sleep, and the tree's law is seams, never sleeps).
#[derive(Debug, Clone)]
pub enum LeaseClock {
    /// Process-monotonic milliseconds.
    Monotonic(std::time::Instant),
    /// Test-driven milliseconds.
    Manual(Arc<AtomicU64>),
}

impl LeaseClock {
    /// The production clock.
    pub fn monotonic() -> Self {
        LeaseClock::Monotonic(std::time::Instant::now())
    }

    /// A clock the caller advances by storing into the counter.
    pub fn manual(ms: Arc<AtomicU64>) -> Self {
        LeaseClock::Manual(ms)
    }

    /// Milliseconds since this clock's origin.
    pub fn now_ms(&self) -> u64 {
        match self {
            LeaseClock::Monotonic(base) => base.elapsed().as_millis() as u64,
            LeaseClock::Manual(ms) => ms.load(Ordering::SeqCst),
        }
    }
}

impl Default for LeaseClock {
    fn default() -> Self {
        LeaseClock::monotonic()
    }
}

/// Rate-error bound between two hosts' monotonic clocks, in parts per
/// million. Two independent oscillators (each specified at ±100 ppm
/// typical, ±500 ppm over temperature) can drift apart by this much per
/// unit time, so over a `T_owner` lease the skew contribution is
/// `T_owner × ppm / 1e6` — 22.5 ms at the shipped 45 s TTL. It is a
/// PHYSICAL bound on hardware, not a tuning constant, which is why it is a
/// literal with its reason on the line (AGENTS: fixed clamps need a
/// documented physical reason).
pub const MONOTONIC_RATE_DRIFT_PPM: u64 = 500;

/// The two lease clocks (§6.7 "Two lease clocks, and the client's is
/// stricter"): `T_self = T_owner − 2·skew_max − D_purge`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseClocks {
    /// The OWNER's lease TTL — the ONE staleness law's 45 s
    /// ([`CLIENT_STALE_TTL_SECS`]) unless overridden, so `live`/`stale`
    /// classification means the same thing on the plane and in the records
    /// (`docs/operations.md`).
    pub t_owner: Duration,
    /// Clock-skew bound: `max(T_owner × MONOTONIC_RATE_DRIFT_PPM,
    /// observed renewal RTT)`. The RTT term is there because the owner's
    /// answer is already that old when the client reads it.
    pub skew_max: Duration,
    /// The client's fail-stop completion bound — how long it can take to
    /// purge cached custody / stop in-flight DMA once it decides to. Two
    /// revalidation cadences (observe, then finish — the S5 purge pass is
    /// the vehicle) floored at one observed RTT, because nothing stops
    /// faster than the round trip's worth of work already in flight.
    pub d_purge: Duration,
    /// The CLIENT's deadline: strictly earlier than the owner's.
    pub t_self: Duration,
    /// Renewal cadence: `min(shipped 10 s beat, T_self / 3)` — three
    /// attempts before the client's own deadline, and never a regression
    /// below the cadence the tree already ships.
    pub renew_interval: Duration,
    /// Owner-failover grace window (§6.7 "Recovery"): reclaim admitted,
    /// conflicting fresh acquires refused. Defaults to `T_owner`, so every
    /// member that was live under the predecessor has a full lease period
    /// to re-assert.
    pub grace: Duration,
}

impl LeaseClocks {
    /// Resolve from the knob registry's derived defaults plus an observed
    /// renewal RTT (the plane measures its own; a caller with no
    /// measurement passes its dial RTT).
    pub fn derive(observed_rtt: Duration) -> Result<Self> {
        let t_owner = Duration::from_millis(crate::env_knobs::int_knob(
            "SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS",
            CLIENT_STALE_TTL_SECS * 1000,
        ));
        // drift = T_owner × ppm / 1e6. In nanoseconds that is
        // `ms × 1e6 × ppm / 1e6 = ms × ppm` — 22.5 ms at the shipped 45 s
        // TTL, with no intermediate rounding to lose.
        let drift = Duration::from_nanos(t_owner.as_millis() as u64 * MONOTONIC_RATE_DRIFT_PPM);
        let skew_max =
            match crate::env_knobs::opt_int_knob::<u64>("SQUEEZEFS_MEMBERSHIP_SKEW_MAX_MS") {
                Some(ms) => Duration::from_millis(ms),
                None => drift.max(observed_rtt),
            };
        // `ro_coherence::checkpoint_cadence()` was deleted when the S5 reader
        // was wired to the landed revalidation machinery; its successor is the
        // poller's own interval, which is the SAME derivation
        // (`max(effective writer cadence, CHECKPOINT_MAX_AGE_MS)`) read through
        // the one place that owns it. Doubling it is unchanged: observe, then
        // finish — the S5 purge pass is the vehicle.
        let cadence = crate::meta_backend::kv::revalidate::RevalidationPoller::derived().interval();
        let purge_default = (cadence * 2).max(observed_rtt);
        let d_purge = match crate::env_knobs::opt_int_knob::<u64>("SQUEEZEFS_MEMBERSHIP_PURGE_MS") {
            Some(ms) => Duration::from_millis(ms),
            None => purge_default,
        };
        let mut clocks = Self::with_params(t_owner, skew_max, d_purge)?;
        if let Some(ms) = crate::env_knobs::opt_int_knob::<u64>("SQUEEZEFS_MEMBERSHIP_GRACE_MS") {
            clocks.grace = Duration::from_millis(ms);
        }
        Ok(clocks)
    }

    /// The formula, applied to explicit parameters. **Refuses** — never
    /// clamps — when `2·skew_max + D_purge ≥ T_owner`: a member that
    /// cannot fail-stop before the owner may re-grant is the divergence
    /// this asymmetry exists to prevent, and silently shortening someone
    /// else's lease would hide it.
    pub fn with_params(t_owner: Duration, skew_max: Duration, d_purge: Duration) -> Result<Self> {
        let reserve = 2 * skew_max + d_purge;
        if reserve >= t_owner {
            return Err(SqueezefsError::InvalidOperation(format!(
                "membership lease clocks refuse to arm: T_self = T_owner − 2·skew_max − \
                 D_purge = {t_owner:?} − 2×{skew_max:?} − {d_purge:?} is not positive, so a \
                 member could still believe it holds custody the owner has already re-granted \
                 (spec §6.7 'Two lease clocks'). Raise \
                 SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS, or lower \
                 SQUEEZEFS_MEMBERSHIP_SKEW_MAX_MS / SQUEEZEFS_MEMBERSHIP_PURGE_MS"
            )));
        }
        let t_self = t_owner - reserve;
        let shipped_beat = Duration::from_secs(CLIENT_HEARTBEAT_INTERVAL_SECS);
        let renew_interval = (t_self / 3).min(shipped_beat).max(Duration::from_millis(1));
        Ok(Self {
            t_owner,
            skew_max,
            d_purge,
            t_self,
            renew_interval,
            grace: t_owner,
        })
    }
}

// ---------------------------------------------------------------------------
// The owner: the RAM lease authority
// ---------------------------------------------------------------------------

/// A join request (fresh, or a **reclaim** carrying the epoch the member
/// held under the predecessor — which is what the grace window admits).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRequest {
    /// The member's uuid.
    pub id: String,
    /// Writer or reader.
    pub role: MemberRole,
    /// The member's own endpoint, when it serves one.
    pub endpoint: Option<String>,
    /// Member pid (diagnosis).
    pub pid: u32,
    /// Member boot id.
    pub boot: String,
    /// `Some` ⇒ this is a RECLAIM of custody held under the predecessor
    /// owner. The grace window admits exactly these.
    pub prior_epoch: Option<u64>,
    /// The member's NVMe registrant key (`0` = none).
    pub pr_key: u64,
}

/// The lease grant: what the member needs to compute its stricter clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// The member's lease epoch — monotone per owner, never reused. A
    /// renewal presenting a different epoch is not custody.
    pub epoch: u64,
    /// The owner's durable writer era.
    pub term: u64,
    /// `T_owner` in ms.
    pub t_owner_ms: u64,
    /// `skew_max` in ms.
    pub skew_max_ms: u64,
    /// `D_purge` in ms.
    pub d_purge_ms: u64,
    /// The renewal cadence the owner expects, in ms.
    pub renew_ms: u64,
    /// The owner's monotonic instant of the grant (diagnostics only — the
    /// member NEVER anchors on a foreign clock's value; it anchors on its
    /// own send instant).
    pub granted_at_owner_ms: u64,
}

impl Grant {
    /// `T_self` for this grant: `T_owner − 2·skew_max − D_purge`, saturating
    /// at zero (a grant whose reserve exceeds its TTL is refused at arm, so
    /// zero here means a hostile/garbled grant — which must fail-stop
    /// immediately rather than be trusted).
    pub fn t_self_ms(&self) -> u64 {
        self.t_owner_ms
            .saturating_sub(2 * self.skew_max_ms + self.d_purge_ms)
    }
}

/// The outcome of a join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinOutcome {
    /// Admitted, with the lease.
    Granted(Grant),
    /// Refused (the grace window's fresh-acquire arm, or a garbled
    /// request), with an honest retry-after so a refused member backs off
    /// instead of spinning.
    Refused {
        /// Operator-facing reason.
        reason: String,
        /// When to try again, ms.
        retry_after_ms: u64,
    },
}

/// The outcome of a renewal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenewOutcome {
    /// Renewed — the grant carries the fresh deadline inputs.
    Renewed(Grant),
    /// This lease is not custody any more (evicted, expired-and-swept, or
    /// minted by a previous owner). The member must self-fence and re-join;
    /// it must NOT keep believing it holds anything.
    UnknownLease {
        /// Operator-facing reason.
        reason: String,
    },
}

/// One member as the census reports it — the row `squeezefs clients` shows
/// and the row DISC-1 projects peers from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberSnapshot {
    /// Member uuid.
    pub id: String,
    /// Writer or reader.
    pub role: MemberRole,
    /// The member's endpoint, when it serves one.
    pub endpoint: Option<String>,
    /// The member's lease epoch.
    pub epoch: u64,
    /// Member pid.
    pub pid: u32,
    /// Member boot id.
    pub boot: String,
    /// Milliseconds since its last renewal.
    pub age_ms: u64,
    /// Milliseconds until the OWNER's deadline (0 = expired, awaiting the
    /// sweep).
    pub expires_in_ms: u64,
    /// The freed-offset epoch this member has acknowledged passing (§6.8
    /// item 3's channel).
    pub acked_free_epoch: u64,
    /// `live` (inside the owner TTL) or `stale` (expired, not yet swept) —
    /// the SAME two words the record-based surfaces use
    /// (`MountRegistration::state`), so an operator reads ONE
    /// classification whether a row came from a record or from the plane.
    pub state: String,
}

/// A member removed from the census: its epoch is dead, and its blocks are
/// what S7's do-not-reallocate quarantine is keyed on.
#[derive(Debug, Clone)]
pub struct Eviction {
    /// The evicted member's uuid.
    pub id: String,
    /// Its role.
    pub role: MemberRole,
    /// The lease epoch that just died.
    pub epoch: u64,
    /// The S7 cohort id its offsets are quarantined under until a drain
    /// proof arrives (`docs/design-nvmeof-target-management.md` §6.8.1 for
    /// the device half).
    pub dead: crate::data_custody::DeadEpoch,
    /// Why (TTL fired, no acknowledgement, operator).
    pub reason: String,
}

struct MemberState {
    role: MemberRole,
    endpoint: Option<String>,
    pid: u32,
    boot: String,
    epoch: u64,
    join_seq: u64,
    renewed_ms: u64,
    deadline_ms: u64,
    acked_free_epoch: u64,
}

struct Grace {
    until_ms: u64,
    expected: std::collections::BTreeSet<String>,
}

/// The membership authority: a RAM lease table plus one atomic per
/// question. Renewal is one `scc` probe and two stores — §6.7's "arbitration
/// is RAM-only" applied to liveness, which is why the plane can serve 15 k
/// members at 1,500 beats/s without touching the metadata plane at all.
pub struct MembershipOwner {
    id: String,
    term: u64,
    clocks: LeaseClocks,
    clock: LeaseClock,
    members: scc::HashMap<String, MemberState>,
    next_epoch: AtomicU64,
    next_join_seq: AtomicU64,
    grace: parking_lot::Mutex<Option<Grace>>,
}

impl std::fmt::Debug for MembershipOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MembershipOwner")
            .field("id", &self.id)
            .field("term", &self.term)
            .field("members", &self.members.len())
            .field("grace_active", &self.grace_active())
            .finish_non_exhaustive()
    }
}

impl MembershipOwner {
    /// Arm the authority for `term`, superseding `prior_term`.
    ///
    /// **Refuses** unless `term > prior_term`: §6.7's recovery law is that
    /// the successor bumps `term` durably BEFORE arming, so every token and
    /// DMA authorization from the predecessor's era is stale by
    /// construction. An owner that armed on an equal era could hand out
    /// custody indistinguishable from the dead owner's.
    pub fn arm(
        id: &str,
        term: u64,
        prior_term: u64,
        clocks: LeaseClocks,
        clock: LeaseClock,
    ) -> Result<Arc<Self>> {
        if term <= prior_term && !(term == 0 && prior_term == 0) {
            return Err(SqueezefsError::InvalidOperation(format!(
                "membership owner '{id}' refuses to arm in term {term}: the predecessor's \
                 durable term is {prior_term}, and §6.7 recovery requires the successor to \
                 bump the term DURABLY before arming (an equal era makes its grants \
                 indistinguishable from the dead owner's). Arm after the D0 gate's claim \
                 barrier, which is what publishes the new term"
            )));
        }
        log::info!(
            "membership owner '{id}' armed in term {term} (was {prior_term}): lease TTL \
             {:?}, member deadline T_self {:?}, renewal cadence {:?}, grace {:?} — liveness \
             is RAM state renewed over cluster_wire and costs ZERO journal transactions \
             (DLM S6, spec §6.5 item 3)",
            clocks.t_owner,
            clocks.t_self,
            clocks.renew_interval,
            clocks.grace,
        );
        Ok(Arc::new(Self {
            id: id.to_string(),
            term,
            clocks,
            clock,
            members: scc::HashMap::new(),
            next_epoch: AtomicU64::new(1),
            next_join_seq: AtomicU64::new(1),
            grace: parking_lot::Mutex::new(None),
        }))
    }

    /// The owner's identity.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The durable era this authority grants in.
    pub fn term(&self) -> u64 {
        self.term
    }

    /// The clock parameters members are granted under.
    pub fn clocks(&self) -> &LeaseClocks {
        &self.clocks
    }

    fn grant_for(&self, epoch: u64, now: u64) -> Grant {
        Grant {
            epoch,
            term: self.term,
            t_owner_ms: self.clocks.t_owner.as_millis() as u64,
            skew_max_ms: self.clocks.skew_max.as_millis() as u64,
            d_purge_ms: self.clocks.d_purge.as_millis() as u64,
            renew_ms: self.clocks.renew_interval.as_millis() as u64,
            granted_at_owner_ms: now,
        }
    }

    /// Admit a member (fresh) or re-admit one (reclaim).
    ///
    /// During the failover grace window a FRESH acquire is refused and a
    /// reclaim is admitted — the whole point of the window, because
    /// refusing reclaims instead would trigger the cluster-wide
    /// forced-flush storm §6.7 names.
    pub fn join(&self, req: JoinRequest) -> JoinOutcome {
        if req.id.is_empty() {
            return JoinOutcome::Refused {
                reason: "a member must present a non-empty identity".into(),
                retry_after_ms: self.clocks.renew_interval.as_millis() as u64,
            };
        }
        let now = self.clock.now_ms();
        let reclaim = req.prior_epoch.is_some();
        if self.grace_active() && !reclaim {
            METRICS
                .membership_grace_refusals
                .fetch_add(1, Ordering::Relaxed);
            let retry = self.grace_remaining_ms().max(1);
            log::warn!(
                "membership: refusing FRESH acquire from '{}' — owner '{}' is inside its \
                 failover grace window ({retry} ms left), which admits reclaim only (spec \
                 §6.7 Recovery)",
                req.id,
                self.id
            );
            return JoinOutcome::Refused {
                reason: format!(
                    "owner '{}' is in its failover grace window: reclaim only for {retry} ms",
                    self.id
                ),
                retry_after_ms: retry,
            };
        }
        let epoch = self.next_epoch.fetch_add(1, Ordering::AcqRel);
        let join_seq = self.next_join_seq.fetch_add(1, Ordering::AcqRel);
        let state = MemberState {
            role: req.role,
            endpoint: req.endpoint.clone(),
            pid: req.pid,
            boot: req.boot.clone(),
            epoch,
            join_seq,
            renewed_ms: now,
            deadline_ms: now + self.clocks.t_owner.as_millis() as u64,
            acked_free_epoch: 0,
        };
        // A re-join REPLACES the prior state (same identity, new epoch):
        // the member is telling us it lost its lease view, and keeping the
        // old epoch alive would leave custody nobody presents.
        let _ = self.members.remove_sync(&req.id);
        let _ = self.members.insert_sync(req.id.clone(), state);
        METRICS.membership_joins.fetch_add(1, Ordering::Relaxed);
        // A fresh member acknowledges nothing yet, so the §6.8 item-3 bound
        // must drop to 0 BEFORE it can serve a byte — publishing at the
        // join (not at the next sweep) is what closes the window in which a
        // brand-new reader's first cached bindings could be reallocated
        // under it. Published DIRECTLY rather than through
        // `refresh_free_grace_bound`: the minimum over a set containing a
        // member that has acknowledged nothing is 0 by construction, so the
        // O(members) scan would be paid at every join of a 15 k-mount storm
        // to compute a constant.
        crate::free_grace::publish_bound(0, self.members.len());
        if reclaim {
            METRICS
                .membership_grace_reclaims
                .fetch_add(1, Ordering::Relaxed);
            self.note_reclaim(&req.id);
        }
        JoinOutcome::Granted(self.grant_for(epoch, now))
    }

    /// Renew a lease, carrying the member's acknowledged freed-offset epoch
    /// (§6.8 item 3's channel). One `scc` probe — this is the plane's hot
    /// operation and it commits nothing.
    pub fn renew(&self, id: &str, epoch: u64, acked_free_epoch: u64) -> RenewOutcome {
        let now = self.clock.now_ms();
        let ttl = self.clocks.t_owner.as_millis() as u64;
        let mut seen_epoch = None;
        let updated = self
            .members
            .update_sync(id, |_, st| {
                seen_epoch = Some(st.epoch);
                if st.epoch != epoch {
                    return false;
                }
                st.renewed_ms = now;
                st.deadline_ms = now + ttl;
                st.acked_free_epoch = st.acked_free_epoch.max(acked_free_epoch);
                true
            })
            .unwrap_or(false);
        if !updated {
            METRICS
                .membership_renew_refusals
                .fetch_add(1, Ordering::Relaxed);
            let reason = match seen_epoch {
                Some(current) => format!(
                    "member '{id}' presented lease epoch {epoch} but this owner's current \
                     epoch for it is {current}: the presented lease is not custody — \
                     self-fence and re-join"
                ),
                None => format!(
                    "member '{id}' is not in owner '{}'s census (evicted, swept past its \
                     TTL, or granted by a previous owner): self-fence and re-join",
                    self.id
                ),
            };
            log::warn!("membership: {reason}");
            return RenewOutcome::UnknownLease { reason };
        }
        METRICS.membership_renewals.fetch_add(1, Ordering::Relaxed);
        RenewOutcome::Renewed(self.grant_for(epoch, now))
    }

    /// A clean departure (unmount): the member leaves the census
    /// immediately, so nothing waits out a TTL for a mount that said
    /// goodbye. `true` ⇔ it was a member.
    pub fn leave(&self, id: &str) -> bool {
        let left = self.members.remove_sync(id).is_some();
        if left {
            log::info!(
                "membership: member '{id}' left cleanly (owner '{}')",
                self.id
            );
            // A departed reader holds nothing: its acknowledgement no
            // longer bounds the writer's reallocation (§6.8 item 3).
            self.refresh_free_grace_bound();
        }
        left
    }

    /// One census page: members with `join_seq > cursor`, in join order,
    /// at most `limit`. Returns the rows and the next cursor (`None` = the
    /// last page).
    ///
    /// Paged because a 15,000-member census must never become one
    /// oversized frame, and ordered by join sequence so paging is stable
    /// under concurrent joins (a member that joins mid-walk lands after the
    /// cursor; one that leaves simply does not appear).
    pub fn census(&self, cursor: u64, limit: usize) -> (Vec<MemberSnapshot>, Option<u64>) {
        let limit = limit.max(1);
        let now = self.clock.now_ms();
        let mut rows: Vec<(u64, MemberSnapshot)> = Vec::new();
        self.members.iter_sync(|id, st| {
            if st.join_seq > cursor {
                rows.push((
                    st.join_seq,
                    MemberSnapshot {
                        id: id.clone(),
                        role: st.role,
                        endpoint: st.endpoint.clone(),
                        epoch: st.epoch,
                        pid: st.pid,
                        boot: st.boot.clone(),
                        age_ms: now.saturating_sub(st.renewed_ms),
                        expires_in_ms: st.deadline_ms.saturating_sub(now),
                        acked_free_epoch: st.acked_free_epoch,
                        state: if now < st.deadline_ms {
                            "live".to_string()
                        } else {
                            "stale".to_string()
                        },
                    },
                ));
            }
            true
        });
        rows.sort_by_key(|(seq, _)| *seq);
        let next = if rows.len() > limit {
            Some(rows[limit - 1].0)
        } else {
            None
        };
        rows.truncate(limit);
        METRICS
            .membership_census_serves
            .fetch_add(1, Ordering::Relaxed);
        (rows.into_iter().map(|(_, row)| row).collect(), next)
    }

    /// Live member count.
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// `true` ⇔ nobody is a member.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Reader members.
    pub fn readers(&self) -> usize {
        self.count_role(MemberRole::Reader)
    }

    /// Writer members.
    pub fn writers(&self) -> usize {
        self.count_role(MemberRole::Writer)
    }

    fn count_role(&self, role: MemberRole) -> usize {
        let mut n = 0;
        self.members.iter_sync(|_, st| {
            if st.role == role {
                n += 1;
            }
            true
        });
        n
    }

    /// A member's current lease epoch (`None` = not a member).
    pub fn epoch_of(&self, id: &str) -> Option<u64> {
        self.members.read_sync(id, |_, st| st.epoch)
    }

    /// A member's OWNER-side deadline in this clock's milliseconds — the
    /// instant after which the owner may grant its objects elsewhere. The
    /// contract the client's stricter deadline is measured against.
    pub fn lease_deadline_ms(&self, id: &str) -> Option<u64> {
        self.members.read_sync(id, |_, st| st.deadline_ms)
    }

    /// **§6.8 item 3's reallocation bound**: the minimum freed-offset epoch
    /// acknowledged across every LIVE member. A writer may reallocate an
    /// offset freed in epoch `E` once this is `>= E`; with no members the
    /// answer is `u64::MAX` (nobody can be holding a stale binding), and a
    /// member that has acknowledged nothing yet holds the bound at 0.
    pub fn min_acked_free_epoch(&self) -> u64 {
        let mut min = u64::MAX;
        self.members.iter_sync(|_, st| {
            min = min.min(st.acked_free_epoch);
            true
        });
        if min == u64::MAX && !self.members.is_empty() {
            0
        } else {
            min
        }
    }

    /// Publish [`Self::min_acked_free_epoch`] and the live member count to
    /// the §6.8 item-3 gate ([`crate::free_grace`]) — the writer-side half
    /// of the acknowledgement channel.
    ///
    /// Called at every membership CHANGE (join, clean leave, eviction) and
    /// on the owner's sweep cadence, deliberately **not** per renewal: the
    /// minimum is O(members), and at 15,000 members × 1,500 beats/s that
    /// would be 22.5 M scans/s to learn something only a renewal can
    /// change. The cadence's cost is that a released offset's residence
    /// includes up to one renewal interval, which `free_grace::ack_cycle`
    /// accounts for.
    pub fn refresh_free_grace_bound(&self) {
        crate::free_grace::publish_bound(self.min_acked_free_epoch(), self.len());
    }

    /// The members that have NOT acknowledged `epoch` — item 3's laggard
    /// list, ordered so an operator sees a stable answer. "A reader that
    /// fails to acknowledge is fenced, not waited on": the fencing act is
    /// [`Self::evict`].
    pub fn members_behind_free_epoch(&self, epoch: u64) -> Vec<String> {
        let mut out = Vec::new();
        self.members.iter_sync(|id, st| {
            if st.acked_free_epoch < epoch {
                out.push(id.clone());
            }
            true
        });
        out.sort();
        out
    }

    /// Evict a named member: it leaves the census, its epoch is declared
    /// DEAD (S7's cohort id), and its blocks stay non-reallocatable until a
    /// drain proof arrives.
    pub fn evict(&self, id: &str, reason: &str) -> Option<Eviction> {
        let (_, st) = self.members.remove_sync(id)?;
        let dead = crate::data_custody::declare_dead_epoch(&format!(
            "membership: member '{id}' evicted by owner '{}' ({reason})",
            self.id
        ));
        METRICS.membership_evictions.fetch_add(1, Ordering::Relaxed);
        log::warn!(
            "membership: member '{id}' ({}) EVICTED by owner '{}' — {reason}; lease epoch \
             {} is dead and its offsets are quarantined until proven drained (DLM S6 → S7)",
            st.role.as_str(),
            self.id,
            st.epoch
        );
        // An evicted member's acknowledgement stops bounding the writer —
        // which is precisely what "fenced, not waited on" means for §6.8
        // item 3. (A READER's dead epoch names no offsets: it allocated
        // none. The cohort id is minted anyway so the two planes keep one
        // vocabulary.)
        self.refresh_free_grace_bound();
        Some(Eviction {
            id: id.to_string(),
            role: st.role,
            epoch: st.epoch,
            dead,
            reason: reason.to_string(),
        })
    }

    /// Sweep every member past the OWNER's deadline (§6.7 "Recovery",
    /// client-failure half: the TTL fires, the epoch is marked dead, the
    /// grant bucket is dropped O(1), and the blocks enter quarantine).
    ///
    /// Runs on the owner's cadence task, never on a handler lane.
    pub fn expire_due(&self) -> Vec<Eviction> {
        let now = self.clock.now_ms();
        let mut expired: Vec<String> = Vec::new();
        self.members.iter_sync(|id, st| {
            // `>=`: the lease expires AT the deadline, which is the
            // instant the member's own (strictly earlier) deadline was
            // measured against.
            if now >= st.deadline_ms {
                expired.push(id.clone());
            }
            true
        });
        expired.sort();
        expired
            .iter()
            .filter_map(|id| {
                self.evict(
                    id,
                    &format!(
                        "lease TTL {:?} expired without a renewal",
                        self.clocks.t_owner
                    ),
                )
            })
            .collect()
    }

    /// Open the failover **grace window** (§6.7): only reclaim is admitted
    /// until every id in `expected` has re-asserted, or until the window's
    /// deadline. `expected` comes from the predecessor's durable evidence —
    /// the claim set's writers, plus any census snapshot handed over.
    pub fn open_grace(&self, expected: Vec<String>) {
        let until = self.clock.now_ms() + self.clocks.grace.as_millis() as u64;
        let expected: std::collections::BTreeSet<String> = expected.into_iter().collect();
        log::warn!(
            "membership owner '{}' opened a failover grace window for {:?}: reclaim only, \
             conflicting fresh acquires refused; awaiting re-assertion from {} prior \
             member(s) (spec §6.7 — without this window failover triggers a cluster-wide \
             forced-flush storm)",
            self.id,
            self.clocks.grace,
            expected.len()
        );
        *self.grace.lock() = Some(Grace {
            until_ms: until,
            expected,
        });
    }

    /// `true` ⇔ the grace window is open (it closes on re-assertion or at
    /// its deadline, whichever comes first).
    pub fn grace_active(&self) -> bool {
        let mut guard = self.grace.lock();
        let Some(g) = guard.as_ref() else {
            return false;
        };
        if self.clock.now_ms() >= g.until_ms {
            log::info!(
                "membership owner '{}': grace window closed on its deadline with {} \
                 member(s) never re-asserting — their leases are gone and fresh acquires \
                 are admitted again",
                self.id,
                g.expected.len()
            );
            *guard = None;
            return false;
        }
        true
    }

    /// Milliseconds left in the grace window (`0` = closed).
    pub fn grace_remaining_ms(&self) -> u64 {
        if !self.grace_active() {
            return 0;
        }
        let guard = self.grace.lock();
        guard
            .as_ref()
            .map(|g| g.until_ms.saturating_sub(self.clock.now_ms()))
            .unwrap_or(0)
    }

    fn note_reclaim(&self, id: &str) {
        let mut guard = self.grace.lock();
        let Some(g) = guard.as_mut() else {
            return;
        };
        g.expected.remove(id);
        if g.expected.is_empty() {
            log::info!(
                "membership owner '{}': every prior member has re-asserted — grace window \
                 closed early, fresh acquires admitted",
                self.id
            );
            *guard = None;
        }
    }
}

// ---------------------------------------------------------------------------
// The member side: the stricter clock and the self-fence
// ---------------------------------------------------------------------------

/// What a self-fence actually did — returned so the decision is explicit
/// and testable rather than buried in side effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelfFence {
    /// The fencing member's role.
    pub role: MemberRole,
    /// A WRITER poisons process data custody (S7): no new DMA
    /// authorization is minted and every in-flight one is refused at
    /// [`crate::data_custody::authorize_dma`].
    pub poisoned_data_custody: bool,
    /// A READER must drop everything it caches (the S5 purge pass) before
    /// serving another byte.
    pub purge_requested: bool,
    /// `false` when the session had already fenced (idempotent).
    pub first: bool,
}

/// A member's view of its own lease — and the **stricter** clock.
///
/// The deadline is anchored on the member's own **send** instant, so the
/// round trip counts against the member: by construction its deadline
/// precedes the owner's by at least `2·skew_max + D_purge + RTT`.
#[derive(Debug)]
pub struct MemberSession {
    id: String,
    role: MemberRole,
    epoch: AtomicU64,
    t_self_deadline_ms: AtomicU64,
    renew_at_ms: AtomicU64,
    acked_free_epoch: AtomicU64,
    /// §6.8 item 3: the causal LABEL the last grant carried — the owner's
    /// own monotonic instant of that grant. A member never reads a foreign
    /// clock as a deadline (§6.7's law, unchanged); it echoes this value
    /// back once it has finished with everything freed at or before it, so
    /// the comparison the writer performs is between two readings of ONE
    /// clock.
    learned_label: AtomicU64,
    /// The member-clock instant at which that label was learned — the
    /// anchor the acknowledgement ladder's qualification wait measures
    /// from (its own clock, for its own durations).
    learned_at_ms: AtomicU64,
    /// The grant's `skew_max` / `D_purge`, kept so the ladder's two waits
    /// are the plane's own numbers rather than a second derivation.
    skew_max_ms: AtomicU64,
    d_purge_ms: AtomicU64,
    fenced: AtomicBool,
    clock: LeaseClock,
}

impl MemberSession {
    /// Adopt a grant. `anchor_ms` is the instant the member SENT the
    /// request (never a value from the owner's clock).
    pub fn adopt(
        id: &str,
        role: MemberRole,
        grant: &Grant,
        anchor_ms: u64,
        clock: LeaseClock,
    ) -> Self {
        let s = Self {
            id: id.to_string(),
            role,
            epoch: AtomicU64::new(grant.epoch),
            t_self_deadline_ms: AtomicU64::new(anchor_ms + grant.t_self_ms()),
            renew_at_ms: AtomicU64::new(anchor_ms + grant.renew_ms),
            acked_free_epoch: AtomicU64::new(0),
            learned_label: AtomicU64::new(grant.granted_at_owner_ms),
            learned_at_ms: AtomicU64::new(anchor_ms),
            skew_max_ms: AtomicU64::new(grant.skew_max_ms),
            d_purge_ms: AtomicU64::new(grant.d_purge_ms),
            fenced: AtomicBool::new(false),
            clock,
        };
        log::info!(
            "membership member '{id}' ({}) holds lease epoch {} in term {}: owner deadline \
             {} ms, MY deadline {} ms (T_owner − 2·skew_max − D_purge, anchored on my send \
             instant), renewing every {} ms",
            role.as_str(),
            grant.epoch,
            grant.term,
            grant.t_owner_ms,
            grant.t_self_ms(),
            grant.renew_ms,
        );
        s
    }

    /// Re-anchor on a successful renewal — including the §6.8 item-3 label
    /// this grant carried (monotone: a label is never un-learned).
    pub fn renewed(&self, grant: &Grant, anchor_ms: u64) {
        self.epoch.store(grant.epoch, Ordering::Release);
        self.t_self_deadline_ms
            .store(anchor_ms + grant.t_self_ms(), Ordering::Release);
        self.renew_at_ms
            .store(anchor_ms + grant.renew_ms, Ordering::Release);
        self.skew_max_ms.store(grant.skew_max_ms, Ordering::Release);
        self.d_purge_ms.store(grant.d_purge_ms, Ordering::Release);
        if grant.granted_at_owner_ms > self.learned_label.load(Ordering::Acquire) {
            // Order matters: the anchor is published FIRST, so a ladder
            // that observes the new label can never pair it with the old
            // (earlier) anchor and qualify too soon.
            self.learned_at_ms.store(anchor_ms, Ordering::Release);
            self.learned_label
                .store(grant.granted_at_owner_ms, Ordering::Release);
        }
    }

    /// §6.8 item 3: the label last learned from the owner and the
    /// member-clock instant it arrived — the acknowledgement ladder's two
    /// inputs ([`crate::free_grace::ReaderAckLadder`]).
    pub fn learned_label(&self) -> (u64, u64) {
        let label = self.learned_label.load(Ordering::Acquire);
        (label, self.learned_at_ms.load(Ordering::Acquire))
    }

    /// The grant's clock-skew bound, ms (the ladder's qualification term).
    pub fn skew_max_ms(&self) -> u64 {
        self.skew_max_ms.load(Ordering::Acquire)
    }

    /// The grant's `D_purge`, ms (the ladder's drain term).
    pub fn d_purge_ms(&self) -> u64 {
        self.d_purge_ms.load(Ordering::Acquire)
    }

    /// This member's own clock, in ms — the frame every wait above is
    /// measured in.
    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// The member's identity.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The member's role.
    pub fn role(&self) -> MemberRole {
        self.role
    }

    /// The lease epoch to present on the next renewal.
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// The member's own deadline, in its clock's milliseconds.
    pub fn t_self_deadline_ms(&self) -> u64 {
        self.t_self_deadline_ms.load(Ordering::Acquire)
    }

    /// When the next renewal is due.
    pub fn renew_at_ms(&self) -> u64 {
        self.renew_at_ms.load(Ordering::Acquire)
    }

    /// `true` ⇔ the member is past its own deadline and MUST fail-stop now
    /// — before the owner's TTL lets the objects be granted elsewhere.
    pub fn self_fence_due(&self) -> bool {
        !self.fenced.load(Ordering::Acquire)
            && self.clock.now_ms() >= self.t_self_deadline_ms.load(Ordering::Acquire)
    }

    /// Acknowledge having passed freed-offset `epoch` (§6.8 item 3): the
    /// value rides the next renewal, and the owner keeps the minimum across
    /// live members. Monotone — an acknowledgement is never withdrawn.
    pub fn ack_free_epoch(&self, epoch: u64) {
        self.acked_free_epoch.fetch_max(epoch, Ordering::AcqRel);
    }

    /// The freed-offset epoch this member has acknowledged.
    pub fn acked_free_epoch(&self) -> u64 {
        self.acked_free_epoch.load(Ordering::Acquire)
    }

    /// `true` ⇔ this session has fail-stopped.
    pub fn fenced(&self) -> bool {
        self.fenced.load(Ordering::Acquire)
    }

    /// **Fail-stop the affected objects ourselves** (§6.7): a writer
    /// poisons process data custody so no DMA can land after the owner may
    /// have re-granted; a reader is told to purge everything it caches.
    /// False-positive eviction then costs availability, never divergence.
    pub fn self_fence(&self, reason: &str) -> SelfFence {
        let first = !self.fenced.swap(true, Ordering::AcqRel);
        if first {
            METRICS
                .membership_self_fences
                .fetch_add(1, Ordering::Relaxed);
            log::error!(
                "membership member '{}' ({}) SELF-FENCED: {reason}. My deadline T_self is \
                 strictly earlier than the owner's TTL, so this happens BEFORE the owner \
                 can grant these objects elsewhere (spec §6.7 'the client's is stricter')",
                self.id,
                self.role.as_str()
            );
        }
        match self.role {
            MemberRole::Writer => {
                if first {
                    crate::data_custody::poison(&format!(
                        "membership lease of '{}' not renewed by T_self: {reason}",
                        self.id
                    ));
                }
                SelfFence {
                    role: self.role,
                    poisoned_data_custody: true,
                    purge_requested: false,
                    first,
                }
            }
            MemberRole::Reader => SelfFence {
                role: self.role,
                poisoned_data_custody: false,
                purge_requested: true,
                first,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// The process registry + the stats surface
// ---------------------------------------------------------------------------

/// What this process is on the membership plane.
enum Installed {
    Owner(Arc<MembershipOwner>),
    Member(Arc<MemberSession>),
}

static INSTALLED: once_cell::sync::Lazy<ArcSwapOption<Installed>> =
    once_cell::sync::Lazy::new(ArcSwapOption::empty);

/// Install this process's role (the stats inode reads it; the
/// `dlm_slot::SLOT_OWNERS` precedent — lock-free, replaceable wholesale).
pub fn install_owner(owner: Arc<MembershipOwner>) {
    INSTALLED.store(Some(Arc::new(Installed::Owner(owner))));
}

/// Install this process as a member (a reader, or a non-owning writer).
pub fn install_member(session: Arc<MemberSession>) {
    INSTALLED.store(Some(Arc::new(Installed::Member(session))));
}

/// Uninstall (disarm / unmount).
pub fn uninstall() {
    INSTALLED.store(None);
}

/// The installed lease AUTHORITY, when this process is one — the §6.8
/// item-3 gate's handle onto the plane (it asks for the bound and, past the
/// grace bound, performs the eviction). `None` on a member and on an
/// un-armed mount, which is what makes the gate structurally inert there.
pub fn installed_owner() -> Option<Arc<MembershipOwner>> {
    let guard = INSTALLED.load();
    match guard.as_deref()? {
        Installed::Owner(o) => Some(Arc::clone(o)),
        Installed::Member(_) => None,
    }
}

/// The installed member SESSION, when this process is one — the reader
/// half of §6.8 item 3 (the label it echoes, and where the acknowledgement
/// is deposited to ride the next renewal).
pub fn installed_member() -> Option<Arc<MemberSession>> {
    let guard = INSTALLED.load();
    match guard.as_deref()? {
        Installed::Member(s) => Some(Arc::clone(s)),
        Installed::Owner(_) => None,
    }
}

/// The installed MEMBER session, when this process is a member (a reader,
/// or — since DLM S9 — a co-writer). `None` for an owner and for an
/// unarmed mount.
///
/// Delegates: the §6.8 item-3 reader half and the S9 co-writer admission
/// arrived at this accessor independently, under two names. One
/// implementation, so the two halves of one posture cannot disagree about
/// what "this process is a member" means.
pub fn installed_member_session() -> Option<Arc<MemberSession>> {
    installed_member()
}

/// The member epoch this process holds (`0` when it is not a member) — the
/// S9 co-writer admission's rung-4 evidence field.
pub fn installed_member_epoch() -> u64 {
    installed_member_session().map(|s| s.epoch()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Arming at mount
// ---------------------------------------------------------------------------

/// Where this mount serves the plane (`SQUEEZEFS_MEMBERSHIP_BIND`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipBind {
    /// Not armed — the shipped default (see the knob's registry entry).
    Off,
    /// Ruling D2's posture: an ephemeral port on every interface, published
    /// through the rendezvous record.
    Auto,
    /// An explicit address.
    Addr(std::net::SocketAddr),
}

/// Resolve the bind posture. A malformed address **refuses** (the ENG-10
/// law: never a silent default, and an operator who asked to serve
/// membership somewhere specific must not silently get nothing).
pub fn resolve_bind() -> Result<MembershipBind> {
    let raw = std::env::var("SQUEEZEFS_MEMBERSHIP_BIND").unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("off") {
        return Ok(MembershipBind::Off);
    }
    if raw.eq_ignore_ascii_case("auto") {
        return Ok(MembershipBind::Auto);
    }
    raw.parse::<std::net::SocketAddr>()
        .map(MembershipBind::Addr)
        .map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "SQUEEZEFS_MEMBERSHIP_BIND='{raw}' is neither `off`, `auto`, nor an \
                 `addr:port` ({e}) — refusing rather than serving membership somewhere the \
                 operator did not ask for"
            ))
        })
}

/// What a mount armed, and the teardown that undoes it.
pub struct MembershipArm {
    plane: Option<Arc<crate::membership_wire::MembershipPlane>>,
    volumes: Vec<Arc<KvMetaBackend>>,
    /// The OWNER's identity — the claim-set entry to withdraw at disarm.
    owner_id: Option<String>,
    stop: Arc<AtomicBool>,
    mode: &'static str,
}

impl std::fmt::Debug for MembershipArm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MembershipArm")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl MembershipArm {
    /// `owner` or `member`.
    pub fn mode(&self) -> &'static str {
        self.mode
    }

    /// Stop the plane: end the cadence tasks, remove the rendezvous record
    /// (an OWNER only — a member must never touch the owner's record, and a
    /// read-only member could not write it anyway) so the volume presents
    /// as un-served, and uninstall.
    pub async fn disarm(mut self) {
        self.stop.store(true, Ordering::Release);
        let owner = self.plane.is_some();
        if owner {
            // Spec §6.8 item 3: with no plane the bound is `u64::MAX`, so
            // every offset still held in a grace ring is released by the
            // next harvest — teardown strands nothing.
            crate::free_grace::disarm_owner_plane();
        }
        if let Some(plane) = self.plane.take() {
            plane.shutdown();
        }
        for be in self.volumes.iter().filter(|_| owner) {
            if let Some(id) = self.owner_id.as_deref() {
                if let Err(e) = withdraw_writer_member(be, id).await {
                    log::warn!(
                        "membership: could not withdraw writer '{id}' from the claim set on                          {}: {e} (a stale member entry classifies by its own liveness, so                          this is honest residue rather than a lie)",
                        be.device_path().display()
                    );
                }
            }
            if let Err(e) = clear_owner_record(be).await {
                log::warn!(
                    "membership: could not remove the rendezvous record from {}: {e} (a stale \
                     record names an endpoint that no longer answers, which the next reader \
                     detects on its first renewal)",
                    be.device_path().display()
                );
            }
        }
        // Stop latch (the D0 heartbeat precedent — sqz-meta tasks are
        // never aborted mid-poll): the sweep/renewal loops check `stop`
        // after every sleep and exit on their next wake.
        uninstall();
    }
}

/// Arm the membership plane for this mount (called once, at mount, after
/// the D0 gate published the durable term and after the job wire wrote the
/// storage-trust `job:enroll` record).
///
/// * A **write mount** with a bind posture becomes the lease AUTHORITY: it
///   binds, publishes ONE rendezvous record per volume, installs itself for
///   the stats surface, and runs the TTL sweep on its own cadence task.
/// * A **read-only mount** joins as a member whenever it finds a fresh
///   rendezvous record — no knob, because the fleet already opted in when
///   its writer armed (ruling D2: zero configuration). This is what makes a
///   reader visible in `squeezefs clients` for the first time, at the cost
///   of exactly zero metadata writes.
/// * Anything else returns `Ok(None)`, loudly enough to explain itself.
///
/// `on_purge` is the reader's fail-stop action (drop every cached block —
/// `ro_coherence::purge_reader_block_keys`), invoked if the member ever
/// misses its own `T_self` deadline.
pub async fn arm_mount_membership(
    meta: &Arc<crate::meta_backend::RoutedMetaBackend>,
    read_only: bool,
    on_purge: Option<Arc<dyn Fn() + Send + Sync>>,
) -> Result<Option<MembershipArm>> {
    let bind = resolve_bind()?;
    let volumes: Vec<Arc<KvMetaBackend>> = meta.volumes.clone();
    let Some(first) = volumes.first() else {
        return Ok(None);
    };
    let Some(secret) = cluster_secret(first).await else {
        if bind != MembershipBind::Off {
            log::warn!(
                "membership: SQUEEZEFS_MEMBERSHIP_BIND is set but the volume set carries no \
                 job:enroll record, which is this plane's root of trust (possession of \
                 volume access IS cluster membership — ruling D2). Enable the cluster \
                 listener (SQUEEZEFS_JOB_WIRE_BIND) so the secret exists; membership is NOT \
                 armed"
            );
        }
        return Ok(None);
    };

    if read_only {
        return arm_member(&volumes, secret, MemberRole::Reader, 0, on_purge).await;
    }
    let bind_addr = match bind {
        MembershipBind::Off => {
            log::debug!(
                "membership: not armed (SQUEEZEFS_MEMBERSHIP_BIND=off — the shipped default; \
                 liveness stays on the `client:{{uuid}}` records this mount already writes)"
            );
            return Ok(None);
        }
        MembershipBind::Auto => "0.0.0.0:0".parse().expect("literal addr"),
        MembershipBind::Addr(a) => a,
    };
    arm_owner(meta, volumes, secret, bind_addr).await
}

async fn arm_owner(
    meta: &Arc<crate::meta_backend::RoutedMetaBackend>,
    volumes: Vec<Arc<KvMetaBackend>>,
    secret: Vec<u8>,
    bind_addr: std::net::SocketAddr,
) -> Result<Option<MembershipArm>> {
    // The RTT term of `skew_max` is derived, not measured, at arm: an owner
    // has no member to measure yet. `Duration::ZERO` therefore falls back
    // to the PHYSICAL drift bound (22.5 ms at the 45 s TTL), which
    // comfortably covers the 0.05–0.25 ms cluster-wire RTTs S3 measured
    // (`.benchmarks/2026-08-05-dlm-s3-cluster-wire.md`). A fabric slower
    // than 22.5 ms per round trip must say so through
    // SQUEEZEFS_MEMBERSHIP_SKEW_MAX_MS.
    let clocks = LeaseClocks::derive(Duration::ZERO)?;
    // The label clock: ONE instance, shared by the lease authority (which
    // stamps every grant's `granted_at_owner_ms`) and the §6.8 item-3 gate
    // (which stamps every freed offset). Two clocks would make labels and
    // acknowledgements incomparable.
    let clock = LeaseClock::monotonic();
    // Spec §6.8 item 3: arm the freed-offset grace period FIRST — an unsafe
    // grace bound must refuse before this mount writes a rendezvous record
    // or claims anything. Armed with no members it is inert (the bound is
    // `u64::MAX` and the gate's armed word is false), so the ordering costs
    // nothing and the refusal leaves no residue.
    crate::free_grace::arm_owner_plane(clock.clone(), &clocks)?;
    let term = crate::dlm::durable_term();
    // The predecessor's era, from its own durable evidence: the rendezvous
    // record it left behind (a crash) and the claim set (§6.2 item 7).
    let mut prior_term = 0u64;
    for be in &volumes {
        if let Some(rec) = read_owner_record(be).await {
            prior_term = prior_term.max(rec.term);
        }
        if let Some(set) = ClaimSet::load(be).await {
            if set.durable {
                prior_term = prior_term.max(set.term);
            }
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    let owner = MembershipOwner::arm(&id, term, prior_term, clocks.clone(), clock.clone())?;
    let plane = crate::membership_wire::MembershipPlane::start(
        crate::membership_wire::MembershipPlaneConfig::for_mount(
            bind_addr,
            clocks.t_owner + clocks.renew_interval,
        ),
        secret,
        Arc::clone(&owner),
    )?;
    let endpoint = format!(
        "{}:{}",
        crate::cluster_wire::local_advertise_ip(),
        plane.endpoint().port()
    );
    // The ONE durable write of the whole liveness plane, per volume, at
    // arm. A predecessor's grace window opens over the membership its
    // evidence names.
    let rec = OwnerRecord {
        v: 1,
        id: id.clone(),
        term,
        endpoint: endpoint.clone(),
        ttl_ms: clocks.t_owner.as_millis() as u64,
        ts: unix_now_secs(),
        pid: std::process::id(),
        boot: crate::meta_backend::kv::backend::read_boot_id(),
    };
    // §6.2 item 7's identity half: this writer joins the claim SET, with
    // its DEVICE-side registrant key read from S7's STANDING hold — joined,
    // never a second acquire (a second reservation would conflict at the
    // device and silently downgrade the guarantee class). A no-op on every
    // volume without incompat bit 14, which is all of them today (D9).
    let identity = MemberIdentity {
        id: id.clone(),
        role: MemberRole::Writer,
        pid: std::process::id(),
        boot: rec.boot.clone(),
        endpoint: Some(endpoint.clone()),
        pr_key: crate::data_custody::live_wero_key().unwrap_or(0),
    };
    let mut expected: Vec<String> = Vec::new();
    for be in &volumes {
        publish_owner_record(be, &rec).await?;
        if let Some(set) = ClaimSet::load(be).await {
            for m in set.writers() {
                if m.identity.id != id {
                    expected.push(m.identity.id.clone());
                }
            }
        }
        upsert_writer_member(be, &identity, term).await?;
    }
    expected.sort();
    expected.dedup();
    if prior_term != 0 && !expected.is_empty() {
        // §6.7 Recovery: a successor opens a grace window over the
        // predecessor's membership — reclaim admitted, conflicting fresh
        // acquires refused — so failover does not become a cluster-wide
        // forced-flush storm.
        owner.open_grace(expected);
    }
    install_owner(Arc::clone(&owner));
    // The gate's armed word agrees with the census from the first instant
    // (the plane itself was armed above, on the SAME clock the authority
    // stamps its grants with — labels and acknowledgements must be readings
    // of one clock).
    owner.refresh_free_grace_bound();
    let stop = Arc::new(AtomicBool::new(false));
    {
        let owner = Arc::clone(&owner);
        let stop = Arc::clone(&stop);
        let meta = Arc::clone(meta);
        let plane_stats = Arc::clone(&plane);
        let cadence = clocks.renew_interval;
        crate::meta_exec::spawn_meta_contained("membership_sweep", async move {
            loop {
                squeezefs_ipc::sqz_time::sleep(cadence).await;
                if stop.load(Ordering::Acquire) {
                    return;
                }
                for ev in owner.expire_due() {
                    log::warn!(
                        "membership: swept member '{}' ({}) — {} (dead {} on volume set {})",
                        ev.id,
                        ev.role.as_str(),
                        ev.reason,
                        ev.dead,
                        meta.volumes.len()
                    );
                }
                // Spec §6.8 item 3: republish the reallocation bound on the
                // owner's cadence. This is the ONLY periodic recomputation
                // of the minimum — renewals deliberately do not pay it (see
                // `refresh_free_grace_bound`), which is why a released
                // offset's residence includes one cadence.
                owner.refresh_free_grace_bound();
                // S3's must-stay-0 transport tripwires, surfaced on the
                // plane's own cadence: a frame that failed authentication
                // or a lane that refused a connection is a security or a
                // capacity event, and nothing else on this listener would
                // ever say so.
                let t = plane_stats.transport_stats();
                if t.mac_failures != 0 || t.service_refusals != 0 {
                    log::error!(
                        "membership transport tripwire: {} frame MAC failure(s), {} service                          refusal(s) on the membership listener (cluster_wire's must-stay-0                          pair — tamper/reorder/replay, or lanes at capacity)",
                        t.mac_failures,
                        t.service_refusals
                    );
                }
            }
        });
    }
    log::info!(
        "membership OWNER armed on {endpoint} (id '{id}', term {term}, superseding {prior_term}): \
         members hold RAM leases renewed over cluster_wire, so liveness costs ZERO journal \
         transactions and `squeezefs clients` reads ONE record plus a paged census instead of \
         one getxattr per client (DLM S6, spec §6.5 item 3)"
    );
    Ok(Some(MembershipArm {
        plane: Some(plane),
        volumes,
        owner_id: Some(id),
        stop,
        mode: "owner",
    }))
}

async fn arm_member(
    volumes: &[Arc<KvMetaBackend>],
    secret: Vec<u8>,
    role: MemberRole,
    pr_key: u64,
    on_purge: Option<Arc<dyn Fn() + Send + Sync>>,
) -> Result<Option<MembershipArm>> {
    let mut best: Option<OwnerRecord> = None;
    for be in volumes {
        if let Some(rec) = read_owner_record(be).await {
            if best.as_ref().is_none_or(|b| rec.term > b.term) {
                best = Some(rec);
            }
        }
    }
    let Some(rec) = best else {
        log::info!(
            "membership: no mount on this volume set serves a membership plane, so this \
             reader stays invisible to `squeezefs clients` (the S5 posture — a reader \
             performs no metadata write, by contract). Arm SQUEEZEFS_MEMBERSHIP_BIND on the \
             writer to make readers visible"
        );
        return Ok(None);
    };
    join_member_on(
        &rec,
        secret,
        &uuid::Uuid::new_v4().to_string(),
        role,
        pr_key,
        on_purge,
    )
    .await
}

/// **DLM S9** — join the membership plane as a **WRITER member** (a
/// co-writer), against a rendezvous record the caller already read.
///
/// Why it exists beside [`arm_mount_membership`]: a co-writer's admission
/// rung 4 needs the join to have SUCCEEDED before the metadata set is
/// opened at all (the join IS the liveness proof), and at that point no
/// opened set exists — the record came from a probe. The member id is the
/// co-writer's DURABLE node id rather than a fresh uuid, so the census, the
/// claim-set enrollment and the custody client all name the same node.
///
/// A writer member's self-fence poisons process data custody (S6's role
/// asymmetry, unchanged), which is exactly the fail-stop a co-writer needs:
/// nothing can land after the authority may have re-granted those bytes.
pub async fn join_as_writer_member(
    rec: &OwnerRecord,
    secret: Vec<u8>,
    node_id: &str,
    pr_key: u64,
    on_purge: Option<Arc<dyn Fn() + Send + Sync>>,
) -> Result<Option<MembershipArm>> {
    join_member_on(rec, secret, node_id, MemberRole::Writer, pr_key, on_purge).await
}

async fn join_member_on(
    rec: &OwnerRecord,
    secret: Vec<u8>,
    id: &str,
    role: MemberRole,
    pr_key: u64,
    on_purge: Option<Arc<dyn Fn() + Send + Sync>>,
) -> Result<Option<MembershipArm>> {
    let id = id.to_string();
    let req = JoinRequest {
        id: id.clone(),
        role,
        endpoint: None,
        pid: std::process::id(),
        boot: crate::meta_backend::kv::backend::read_boot_id(),
        prior_epoch: None,
        pr_key,
    };
    let clock = LeaseClock::monotonic();
    let client = match crate::membership_wire::MemberClient::join(
        &rec.endpoint,
        &secret,
        req.clone(),
        clock.clone(),
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            log::warn!(
                "membership: {} '{id}' could not join the plane owner '{}' at {} ({e}) — this \
                 mount stays invisible to `squeezefs clients` (a READER then serves reads \
                 normally; a CO-WRITER's admission rung 4 refuses, because an unseeable \
                 co-writer is an unevictable one)",
                role.as_str(),
                rec.id,
                rec.endpoint
            );
            return Ok(None);
        }
    };
    let session = Arc::clone(client.session());
    install_member(Arc::clone(&session));
    let stop = Arc::new(AtomicBool::new(false));
    spawn_member_renewal(
        client,
        rec.endpoint.clone(),
        secret,
        req,
        clock,
        Arc::clone(&stop),
        on_purge,
    );
    log::info!(
        "membership MEMBER armed: {} '{id}' holds a lease from owner '{}' at {} — visible in \
         `squeezefs clients` with ZERO metadata writes, and the same channel §6.8 item 3's \
         freed-offset acknowledgements ride (DLM S6)",
        role.as_str(),
        rec.id,
        rec.endpoint
    );
    Ok(Some(MembershipArm {
        plane: None,
        // A member owns no durable record, so it has nothing to clear at
        // disarm — and must never remove the OWNER's.
        volumes: Vec::new(),
        owner_id: None,
        stop,
        mode: "member",
    }))
}

/// The member's renewal cadence task: renew, and on failure re-join
/// (a RECLAIM — it presents the epoch it holds, which is what a
/// successor's grace window admits) until its OWN deadline, at which point
/// it fail-stops itself rather than waiting for the owner's TTL.
fn spawn_member_renewal(
    mut client: crate::membership_wire::MemberClient,
    endpoint: String,
    secret: Vec<u8>,
    req: JoinRequest,
    clock: LeaseClock,
    stop: Arc<AtomicBool>,
    on_purge: Option<Arc<dyn Fn() + Send + Sync>>,
) {
    crate::meta_exec::spawn_meta_contained("membership_renewal", async move {
        loop {
            let session = Arc::clone(client.session());
            let now = clock.now_ms();
            let due = session.renew_at_ms().saturating_sub(now);
            squeezefs_ipc::sqz_time::sleep(Duration::from_millis(due.max(1))).await;
            if stop.load(Ordering::Acquire) {
                let _ = client.leave().await;
                return;
            }
            if let Err(e) = client.renew().await {
                if session.self_fence_due() {
                    let fence = session.self_fence(&format!("renewal failed: {e}"));
                    if fence.purge_requested {
                        if let Some(purge) = on_purge.as_ref() {
                            purge();
                        }
                    }
                    return;
                }
                log::warn!(
                    "membership: renewal failed ({e}) — re-asserting as a reclaim before my \
                     own deadline (T_self)"
                );
                let mut reclaim = req.clone();
                reclaim.prior_epoch = Some(session.epoch());
                match crate::membership_wire::MemberClient::join(
                    &endpoint,
                    &secret,
                    reclaim,
                    clock.clone(),
                )
                .await
                {
                    Ok(fresh) => {
                        install_member(Arc::clone(fresh.session()));
                        client = fresh;
                    }
                    Err(e) => log::warn!("membership: reclaim refused ({e}); retrying"),
                }
            }
        }
    })
}

/// The `membership_mode` stats field: `off` (no plane armed — the shipped
/// default until `SQUEEZEFS_MEMBERSHIP_BIND` is set), `owner` (this mount
/// is the authority) or `member`.
pub fn membership_mode() -> &'static str {
    match &*INSTALLED.load() {
        None => "off",
        Some(i) => match &**i {
            Installed::Owner(_) => "owner",
            Installed::Member(_) => "member",
        },
    }
}

/// The plane's gauge block for the stats inode. Counters live in
/// [`METRICS`]; these are the live questions only the installed role can
/// answer.
pub fn stats_snapshot() -> serde_json::Value {
    match &*INSTALLED.load() {
        None => serde_json::json!({ "membership_mode": "off" }),
        Some(i) => match &**i {
            Installed::Owner(o) => serde_json::json!({
                "membership_mode": "owner",
                "membership_term": o.term(),
                "membership_members": o.len(),
                "membership_readers": o.readers(),
                "membership_writers": o.writers(),
                "membership_lease_ttl_ms": o.clocks().t_owner.as_millis() as u64,
                "membership_self_deadline_ms": o.clocks().t_self.as_millis() as u64,
                "membership_grace_remaining_ms": o.grace_remaining_ms(),
                "membership_min_acked_free_epoch": o.min_acked_free_epoch(),
            }),
            Installed::Member(s) => serde_json::json!({
                "membership_mode": "member",
                "membership_role": s.role().as_str(),
                "membership_epoch": s.epoch(),
                "membership_self_deadline_ms": s.t_self_deadline_ms(),
                "membership_self_fenced": s.fenced(),
                "membership_acked_free_epoch": s.acked_free_epoch(),
            }),
        },
    }
}
