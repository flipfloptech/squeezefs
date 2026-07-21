//! PR VL6a — **online report-only fsck** (design-volume-lifecycle §5.6,
//! KD-9/KD-17; repair is VL6b and consumes the findings this module
//! verifies).
//!
//! Seven check classes:
//!
//! | Class | What | Source of truth |
//! |---|---|---|
//! | C1 | meta node integrity: checksum ride-along (node reads verify on
//! read) + checksum-valid semantic checks (key schema per tree, value
//! decode, in-page ordering) | v3 node checksums + record codecs |
//! | C2 | block_map ↔ allocator cross-check: **leaked** =
//! allocated-unreferenced, **lost** = referenced-unallocated /
//! out-of-range | the `df` census walk vs allocator state |
//! | C3 | refcount vs actual referencer count (clone-aware; the §5.4
//! step-2 mover pre-publish ledger consulted) | tree walk +
//! `refcount_core` reads |
//! | C4 | orphan `active_block:` / `active_block_ext:` staged custody
//! whose ino has no live meta | staged custody scan, read-only |
//! | C5 | staging-dir generation validity vs the mounted volume-set
//! generation | the generation-marker predicates, read-only |
//! | C6 | capacity accounting drift: used-blocks arithmetic vs the
//! tracked refcount population | allocator gauges |
//! | C7 | data scrub (KD-17, `--scrub` / `squeezefs scrub`): AEAD open on
//! encrypted volumes, frame decode on compressed (incl. the bit-31 raw
//! escape), readability-only on plain (`scrub_readability_only` is the
//! honesty gauge) | stored AEAD tags / frame structure / read status |
//!
//! **Verify-before-report (KD-9)** — detection never mutates, and a
//! violation becomes a finding only after it survives the class's full
//! machinery:
//!
//! * Per-object classes (C1/C4/C5): suspect → settle window → final
//!   re-check under the object's DLM lease (lattice 2/4a — brief,
//!   per-object, never a global freeze).
//! * Cross-object allocator classes (C2/C3, and C6's aggregate): the
//!   **allocation-epoch filter** — a scan-latched side map fed by
//!   `allocate_block` only while a scan is armed (never the loom-verified
//!   incarnation seqlock) — whose two-epoch survival only **escalates**
//!   to the **in-flight allocation registry** liveness check. Age alone
//!   is never a verdict (retry-forever writeback and R5 parks
//!   legitimately span epochs). The final escalation's NORMATIVE order:
//!   **registry-absence first, then re-verify the reference state** — the
//!   registry contract (owners deregister only after their publish is
//!   durable AND visible to the reads fsck performs) makes both
//!   interleavings of a racing publish/deregister safe
//!   (reference-state-first would false-positive on a healthy
//!   just-published block).
//! * C7 failures re-verify online under a validated pin
//!   (`pin_block_validated`): a mapping that moved, or an in-flight
//!   patch (unstable incarnation), clears the suspect instead of
//!   reporting a torn read.
//!
//! **Online / offline duality (§5.8)**: online reads the live daemon's
//! RAM-authoritative state (arc-swap snapshots) with the suspects
//! machinery; offline (`--offline`, read-only probe opens) needs no
//! suspects — nothing is in flight by definition. **Offline sharding**
//! (`--shards k/N`) walks only the k-th ino-residue shard with zero
//! coordination; `merge-reports` unions the JSON outputs (repeated
//! per-shard classes dedupe by identity; census counters sum). Honesty
//! notes: offline C2-*leaked* and C3 have no durable allocator ground
//! truth (data-volume allocator state is mount-session RAM, rebuilt from
//! the same walk), so offline detects the *lost*/out-of-range arm plus
//! C1/C4/C5 — stated here rather than faked.
//!
//! The C1–C6 scan is coordinator-local by design (§5.1.6 division: it
//! reads live RAM-authoritative state no remote client can see). C7 is
//! designed distributable; v1 runs it on the coordinator-local fabric
//! worker — the §5.1.6 wire dispatches whole jobs only (`Noop`), so
//! shipping scrub sub-shards over the wire's read-shard seam is a
//! follow-up, recorded honestly in `JobType::wire_executable`.

use crate::block_allocator::BlockAllocator;
use crate::error::{Result, SqueezefsError};
use crate::meta_backend::{Metadata, RoutedMetaBackend};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Settle-window override (ms) for the online suspect machinery.
pub const FSCK_SETTLE_MS_ENV: &str = "SQUEEZEFS_FSCK_SETTLE_MS";
/// Default settle window (§5.6 step 2).
pub const FSCK_SETTLE_DEFAULT_MS: u64 = 2_000;
/// Report schema version.
pub const FSCK_REPORT_SCHEMA: u32 = 1;

/// Census page size (records per tree-range fetch) — also the throttle
/// duty-cycle unit for the scan.
const SCAN_PAGE: usize = 512;
/// Scrub throttle batch (blocks per duty-cycle unit).
const SCRUB_BATCH: usize = 8;
/// Staged-custody scan bound (staging dirs are thousands of files).
const CUSTODY_SCAN_MAX: usize = 1_000_000;

// ---------------------------------------------------------------------------
// Options / report types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsckMode {
    /// Live coordinator: suspects → settle → re-check under leases;
    /// epoch filter + registry escalation armed.
    Online,
    /// Read-only probe posture: nothing is in flight by definition — no
    /// suspects machinery, violations report directly.
    Offline,
}

#[derive(Clone)]
pub struct FsckOptions {
    pub mode: FsckMode,
    /// Add the C7 data scrub to the run.
    pub scrub: bool,
    /// C7 only (`squeezefs scrub`).
    pub scrub_only: bool,
    /// KD-3 duty-cycle percentage (0/≥100 = unthrottled).
    pub throttle_pct: u32,
    /// Offline zero-coordination sharding: scan only inos with
    /// `ino % n == k` (`(k, n)`, `k < n`).
    pub shard: Option<(u32, u32)>,
    /// The §5.6 settle window (online).
    pub settle: Duration,
    /// Cooperative cancellation (fabric cancel).
    pub cancel: Arc<AtomicBool>,
}

impl FsckOptions {
    fn settle_from_env() -> Duration {
        let ms = std::env::var(FSCK_SETTLE_MS_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(FSCK_SETTLE_DEFAULT_MS);
        Duration::from_millis(ms)
    }

    pub fn online() -> Self {
        Self {
            mode: FsckMode::Online,
            scrub: false,
            scrub_only: false,
            throttle_pct: 100,
            shard: None,
            settle: Self::settle_from_env(),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn offline() -> Self {
        Self {
            mode: FsckMode::Offline,
            ..Self::online()
        }
    }
}

/// One verified violation: full identity (class, object, evidence) —
/// the input VL6b's repair planner consumes.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FsckFinding {
    /// `"C1"`..`"C7"`.
    pub class: String,
    /// The object's identity (ino / key / volume+offset / path).
    pub object: String,
    /// What was observed (human-readable, machine-greppable).
    pub evidence: String,
    /// PR VL6b (§5.6a): the structured identity the repair engine acts
    /// on. `None` on reports produced by older binaries — such findings
    /// are refused by repair (re-run detection), never guessed at from
    /// the display strings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<FindingId>,
}

/// PR VL6b (§5.6a): machine identity of a verified finding — everything
/// the per-class repair action needs, carried IN the report (findings
/// are the only repair input; repair re-verifies each is still current
/// before acting).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingId {
    /// C1: checksum-bad / torn node (walk could not advance).
    C1Torn {
        vol: usize,
        tree: u8,
        cursor_hex: String,
    },
    /// C1: checksum-valid semantic damage on one record.
    C1Semantic {
        vol: usize,
        tree: u8,
        key_hex: String,
    },
    /// C2: allocated (tracked) with zero referencers.
    C2Leaked { vol: String, offset: u64 },
    /// C2: referenced but unallocated / out-of-range / unresolvable.
    /// `unrepairable_shape` = out-of-range / unaligned / unknown-backend
    /// mappings whose allocator can never legally be repaired — the
    /// content-verify arm is skipped and the action is quarantine.
    C2Lost {
        vol: String,
        offset: u64,
        ino: u64,
        block_idx: u32,
        mapping: String,
        unrepairable_shape: bool,
    },
    /// C3: refcount ≠ counted references.
    C3Refcount { vol: String, offset: u64 },
    /// C4: orphan staged custody.
    C4Orphan { dir: PathBuf, key: String, ino: u64 },
    /// C5: stale/invalid staging generation.
    C5Staging { dir: PathBuf },
    /// C6: capacity-accounting drift.
    C6Drift { vol: String },
    /// C7: scrub-failed block.
    C7Scrub {
        ino: u64,
        block_idx: u32,
        mapping: String,
    },
}

/// The §10 `fsck_*` / `scrub_*` counter families, per run (the process
/// gauges in [`crate::fuse_client::METRICS`] accumulate the same names).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct FsckCounters {
    pub inodes_scanned: u64,
    /// Tree pages walked (one per leaf-range fetch — the C1 walk unit).
    pub nodes_walked: u64,
    pub blocks_checked: u64,
    pub refcounts_checked: u64,
    pub suspects: u64,
    pub suspects_cleared: u64,
    pub epoch_exempted: u64,
    pub inflight_exempted: u64,
    pub mover_ledger_exempted: u64,
    pub findings: u64,
    pub scan_secs: u64,
    pub scrub_blocks_scanned: u64,
    pub scrub_bytes_scanned: u64,
    pub scrub_aead_verified: u64,
    pub scrub_frame_verified: u64,
    pub scrub_readability_only: u64,
    pub scrub_failures: u64,
}

/// Per-shard partial census (cross-shard classes finalize at merge).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct PartialCensus {
    /// Canonical volume id → offset → reference count from THIS shard's
    /// ino-residue walk.
    pub refs: HashMap<String, HashMap<u64, u32>>,
}

/// The structured report (`--json` prints it verbatim; `merge-reports`
/// unions shard reports).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FsckReport {
    pub schema: u32,
    pub mode: String,
    /// `"k/N"` on sharded runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shard: Option<String>,
    pub findings: Vec<FsckFinding>,
    pub counters: FsckCounters,
    /// Present on sharded runs (merge input).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial: Option<PartialCensus>,
    /// PR VL6b (§5.6a): the repair plan/outcome when the run was invoked
    /// with `--repair` (dry run) or `--repair --apply`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<RepairReport>,
}

impl FsckReport {
    pub fn has_findings(&self) -> bool {
        !self.findings.is_empty()
    }
}

/// What the engine needs. Online: the live mount's meta + router.
/// Offline: probe opens + a probe-shaped router.
pub struct FsckCtx {
    pub meta: Arc<RoutedMetaBackend>,
    pub router: crate::routing::DataRouter,
    pub staging_dirs: Vec<PathBuf>,
    /// The mounted volume-set generation (C5 ground truth); `None`
    /// skips C5's generation arm.
    pub expected_generation: Option<String>,
}

/// The volume-set generation of an OPEN set (the
/// [`crate::meta_backend::volume_set_generation`] string computed from
/// the live superblocks, volume order preserved).
pub fn volume_generation(meta: &RoutedMetaBackend) -> String {
    use std::fmt::Write as _;
    let mut parts = Vec::with_capacity(meta.volumes.len());
    for kv in &meta.volumes {
        let mut s = String::with_capacity(3 + 32);
        s.push_str("v3:");
        for b in kv.superblock().uuid {
            let _ = write!(s, "{b:02x}");
        }
        parts.push(s);
    }
    parts.join("|")
}

// ---------------------------------------------------------------------------
// Test hook: the §5.6 registry TOCTOU barrier
// ---------------------------------------------------------------------------

/// Invoked with the suspect's clean block key immediately BEFORE the
/// final registry-absence check of an escalated C2/C3 suspect — the
/// deterministic window the publish/deregister race test needs. `None`
/// in production; a set hook may block the engine (tests park it
/// deliberately).
static PRE_REGISTRY_CHECK_HOOK: parking_lot::RwLock<Option<Arc<dyn Fn(&str) + Send + Sync>>> =
    parking_lot::RwLock::new(None);

pub fn set_pre_registry_check_hook(hook: Arc<dyn Fn(&str) + Send + Sync>) {
    *PRE_REGISTRY_CHECK_HOOK.write() = Some(hook);
}

pub fn clear_pre_registry_check_hook() {
    *PRE_REGISTRY_CHECK_HOOK.write() = None;
}

fn fire_pre_registry_hook(key: &str) {
    let hook = PRE_REGISTRY_CHECK_HOOK.read().clone();
    if let Some(h) = hook {
        h(key);
    }
}

/// C7 read-fault injector (the G-VL-5(b) "dm-error or equivalent" arm
/// at the cargo tier: the rig's root legs can interpose a real error
/// target; in-process tests inject at the read seam). Called with the
/// device read offset; `true` = fail this read.
static SCRUB_READ_FAULT_HOOK: parking_lot::RwLock<Option<Arc<dyn Fn(u64) -> bool + Send + Sync>>> =
    parking_lot::RwLock::new(None);

pub fn set_scrub_read_fault_hook(hook: Arc<dyn Fn(u64) -> bool + Send + Sync>) {
    *SCRUB_READ_FAULT_HOOK.write() = Some(hook);
}

pub fn clear_scrub_read_fault_hook() {
    *SCRUB_READ_FAULT_HOOK.write() = None;
}

// ---------------------------------------------------------------------------
// Internal census structures
// ---------------------------------------------------------------------------

/// One referenced mapping (scrub + lost-check unit).
#[derive(Clone, Debug)]
struct MappingRef {
    ino: u64,
    block_idx: u32,
    /// The mapping string VERBATIM (decoration included).
    mapping: String,
    /// The CANONICAL volume id + offset the clean base key resolves to
    /// (`"?"`/0 for unresolvable mappings). Referencer matching must key
    /// on BOTH — offsets alias across volumes (a bare default-slot
    /// mapping carries the same offset number as an unrelated `oss2://`
    /// block; the leg-13 drain-concurrent FP root cause).
    vol: String,
    offset: u64,
    /// §5.6a quarantined (`damaged:`) mapping: counted for refcount
    /// coherence (the physical block is intentionally preserved), but
    /// never scrubbed and never a lost finding — it IS the repair.
    damaged: bool,
}

struct CensusOut {
    /// Canonical volume id → offset → reference count.
    refs: HashMap<String, HashMap<u64, u32>>,
    /// Every referenced mapping (C7 / lost checks).
    mappings: Vec<MappingRef>,
    /// Mappings that do not resolve to any known backend (C2 lost).
    unresolvable: Vec<MappingRef>,
    inodes_scanned: u64,
}

/// One volume's allocator handle under its canonical id (device access
/// rides the router's key-resolved read path).
struct VolAlloc {
    id: String,
    alloc: Arc<BlockAllocator>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SuspectKind {
    /// C1: a specific record violating its tree schema.
    C1Record {
        vol: usize,
        tree: u8,
        key: Vec<u8>,
        why: String,
    },
    /// C1: a tree walk failed (checksum / undecodable node).
    C1Walk {
        vol: usize,
        tree: u8,
        cursor: Vec<u8>,
        error: String,
    },
    /// C2 leaked: allocated (tracked) with zero referencers.
    C2Leaked { vol: String, offset: u64 },
    /// C2 lost: referenced but unallocated / out-of-range / unknown
    /// backend. Carries the referencing mapping's identity for the
    /// finding (§5.6a repair input).
    C2Lost {
        vol: String,
        offset: u64,
        ino: u64,
        block_idx: u32,
        mapping: String,
        why: String,
    },
    /// C3: refcount ≠ counted references.
    C3Refcount {
        vol: String,
        offset: u64,
        expected: u32,
        actual: u32,
    },
    /// C4: staged custody without live ino meta.
    C4Orphan { dir: PathBuf, key: String, ino: u64 },
    /// C5: staging generation invalid.
    C5Generation { dir: PathBuf, why: String },
    /// C6: used-blocks accounting vs tracked population drift.
    C6Drift {
        vol: String,
        used: u64,
        tracked: u64,
    },
}

struct Suspect {
    kind: SuspectKind,
}

fn is_not_found(e: &SqueezefsError) -> bool {
    matches!(e, SqueezefsError::Io(io) if io.kind() == std::io::ErrorKind::NotFound)
}

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

/// Run the detection engine. Report-only: never mutates. Findings must
/// be zero on a healthy volume (the tripwire).
pub async fn run(ctx: &FsckCtx, opts: &FsckOptions) -> Result<FsckReport> {
    let started = std::time::Instant::now();
    let mut counters = FsckCounters::default();
    let mut findings: Vec<FsckFinding> = Vec::new();
    let vols = volume_allocators(ctx);
    let online = opts.mode == FsckMode::Online;

    // Arm the scan latch + first epoch (online: the C2/C3 consistent
    // cut; offline probes have nothing in flight by definition). The
    // latch is disarmed on every exit path by the RAII release below.
    struct LatchRelease(Vec<Arc<BlockAllocator>>);
    impl Drop for LatchRelease {
        fn drop(&mut self) {
            for a in &self.0 {
                a.fsck_end_scan();
            }
        }
    }
    let _latch_release = if online {
        for v in &vols {
            v.alloc.fsck_begin_scan();
        }
        Some(LatchRelease(vols.iter().map(|v| v.alloc.clone()).collect()))
    } else {
        None
    };

    let mut suspects: Vec<Suspect> = Vec::new();
    let mut shard_refs: Option<PartialCensus> = None;

    if !opts.scrub_only {
        // ---- Pass 1: C1 walk + census + staging + accounting ----
        let mut c1 = Vec::new();
        walk_trees_c1(ctx, opts, &mut counters, &mut c1).await;
        suspects.extend(c1);

        let census = walk_census(ctx, opts, &mut counters).await?;
        counters.inodes_scanned = census.inodes_scanned;
        if opts.shard.is_some() {
            shard_refs = Some(PartialCensus {
                refs: census.refs.clone(),
            });
        }

        // C4/C5 staging scan.
        scan_staging(ctx, opts, &mut suspects).await;

        // C2/C3/C6 evaluation against the live allocator state.
        evaluate_allocator_classes(&vols, &census, opts, &mut counters, &mut suspects);

        counters.suspects = suspects.len() as u64;

        if !suspects.is_empty() {
            if online {
                // ---- Settle (§5.6 step 2) ----
                tokio::time::sleep(opts.settle).await;
                for v in &vols {
                    v.alloc.fsck_bump_epoch();
                }
            }
            recheck_suspects(ctx, opts, &vols, suspects, &mut counters, &mut findings).await?;
        }
    }

    // ---- C7 scrub (KD-17) ----
    if opts.scrub || opts.scrub_only {
        // Fresh mapping set (post-settle when checks ran): scrub what is
        // referenced NOW.
        let census = walk_census(ctx, opts, &mut counters).await?;
        if counters.inodes_scanned == 0 {
            counters.inodes_scanned = census.inodes_scanned;
        }
        if opts.shard.is_some() && shard_refs.is_none() {
            shard_refs = Some(PartialCensus {
                refs: census.refs.clone(),
            });
        }
        scrub_c7(ctx, opts, &census, &mut counters, &mut findings).await;
    }

    counters.findings = findings.len() as u64;
    counters.scan_secs = started.elapsed().as_secs();
    findings.sort();
    findings.dedup();
    publish_metrics(&counters);

    Ok(FsckReport {
        schema: FSCK_REPORT_SCHEMA,
        mode: match opts.mode {
            FsckMode::Online => "online".to_string(),
            FsckMode::Offline => "offline".to_string(),
        },
        shard: opts.shard.map(|(k, n)| format!("{k}/{n}")),
        findings,
        counters: counters.clone(),
        partial: shard_refs,
        repair: None,
    })
}

/// Union shard reports (`--shards k/N` outputs): findings dedupe by
/// identity (per-shard-repeated classes C1/C5 collapse), census
/// counters sum, per-run gauges take the max.
pub fn merge_reports(reports: &[FsckReport]) -> FsckReport {
    let mut findings: Vec<FsckFinding> = Vec::new();
    let mut counters = FsckCounters::default();
    let mut refs: HashMap<String, HashMap<u64, u32>> = HashMap::new();
    let mut mode = "offline".to_string();
    for r in reports {
        findings.extend(r.findings.iter().cloned());
        mode.clone_from(&r.mode);
        counters.inodes_scanned += r.counters.inodes_scanned;
        counters.blocks_checked += r.counters.blocks_checked;
        counters.refcounts_checked += r.counters.refcounts_checked;
        counters.suspects += r.counters.suspects;
        counters.suspects_cleared += r.counters.suspects_cleared;
        counters.epoch_exempted += r.counters.epoch_exempted;
        counters.inflight_exempted += r.counters.inflight_exempted;
        counters.mover_ledger_exempted += r.counters.mover_ledger_exempted;
        counters.scrub_blocks_scanned += r.counters.scrub_blocks_scanned;
        counters.scrub_bytes_scanned += r.counters.scrub_bytes_scanned;
        counters.scrub_aead_verified += r.counters.scrub_aead_verified;
        counters.scrub_frame_verified += r.counters.scrub_frame_verified;
        counters.scrub_readability_only += r.counters.scrub_readability_only;
        counters.scrub_failures += r.counters.scrub_failures;
        counters.nodes_walked = counters.nodes_walked.max(r.counters.nodes_walked);
        counters.scan_secs = counters.scan_secs.max(r.counters.scan_secs);
        if let Some(p) = &r.partial {
            for (vol, m) in &p.refs {
                let e = refs.entry(vol.clone()).or_default();
                for (off, c) in m {
                    *e.entry(*off).or_insert(0) += c;
                }
            }
        }
    }
    findings.sort();
    findings.dedup();
    counters.findings = findings.len() as u64;
    FsckReport {
        schema: FSCK_REPORT_SCHEMA,
        mode,
        shard: None,
        findings,
        counters,
        partial: Some(PartialCensus { refs }),
        repair: None,
    }
}

// ---------------------------------------------------------------------------
// Volume/backend resolution
// ---------------------------------------------------------------------------

/// Every distinct allocator/device pair under its canonical id
/// (the default slot and its named registration dedupe by Arc
/// identity).
fn volume_allocators(ctx: &FsckCtx) -> Vec<VolAlloc> {
    let br = &ctx.router.backend_router;
    let mut out: Vec<VolAlloc> = Vec::new();
    let mut push = |id: &str, alloc: &Arc<BlockAllocator>| {
        if !out.iter().any(|v| Arc::ptr_eq(&v.alloc, alloc)) {
            out.push(VolAlloc {
                id: id.to_string(),
                alloc: alloc.clone(),
            });
        }
    };
    for entry in br.backends.iter() {
        push(entry.key(), &entry.value().block_allocator);
    }
    push(br.default_allocator.volume_id(), &br.default_allocator);
    out
}

/// Canonicalize a parsed backend id to the [`volume_allocators`] id
/// (`backend_0`/`squeezefs` aliases resolve to the default slot).
fn canonical_backend(ctx: &FsckCtx, be_id: &str) -> Option<(String, Arc<BlockAllocator>)> {
    let br = &ctx.router.backend_router;
    if be_id == "backend_0" || be_id == "squeezefs" {
        return Some((
            br.default_allocator.volume_id().to_string(),
            br.default_allocator.clone(),
        ));
    }
    if let Some(be) = br.backends.get(be_id) {
        let alloc = be.value().block_allocator.clone();
        // Named registration of the default slot canonicalizes to the
        // default allocator's id (one identity per allocator).
        if Arc::ptr_eq(&alloc, &br.default_allocator) {
            return Some((br.default_allocator.volume_id().to_string(), alloc));
        }
        return Some((be_id.to_string(), alloc));
    }
    None
}

// ---------------------------------------------------------------------------
// C1: tree walk (checksum ride-along + semantic checks)
// ---------------------------------------------------------------------------

/// The C1 checksum-valid schema check for one record — shared by the
/// detection walk and the repair engine's verify-before-repair re-check.
fn record_schema_violation(tree_id: u8, k: &[u8], v: &[u8]) -> Option<String> {
    use crate::meta_backend::kv::record::{
        decode_dentry_key, decode_inode_key, decode_xattr_key, DentryValue, InodeValue, XattrValue,
        TREE_DENTRIES, TREE_INODES, TREE_XATTRS,
    };
    match tree_id {
        t if t == TREE_INODES => match decode_inode_key(k) {
            Err(e) => Some(format!("key does not decode as an inode key: {e}")),
            Ok(_) => InodeValue::decode(v)
                .err()
                .map(|e| format!("inode value undecodable: {e}")),
        },
        t if t == TREE_DENTRIES => match decode_dentry_key(k) {
            Err(e) => Some(format!("key does not decode as a dentry key: {e}")),
            Ok(_) => DentryValue::decode(v)
                .err()
                .map(|e| format!("dentry value undecodable: {e}")),
        },
        t if t == TREE_XATTRS => match decode_xattr_key(k) {
            Err(e) => Some(format!("key does not decode as an xattr key: {e}")),
            Ok(_) => XattrValue::decode(v)
                .err()
                .map(|e| format!("xattr value undecodable: {e}")),
        },
        _ => None,
    }
}

async fn walk_trees_c1(
    ctx: &FsckCtx,
    opts: &FsckOptions,
    counters: &mut FsckCounters,
    suspects: &mut Vec<Suspect>,
) {
    let end = crate::meta_backend::kv::tree::KEY_SPACE_MAX;
    for (vol_idx, kv) in ctx.meta.volumes.iter().enumerate() {
        for tree in kv.trees() {
            if opts.cancel.load(Ordering::Relaxed) {
                return;
            }
            let tree_id = tree.tree_id();
            let mut cursor: Vec<u8> = vec![0u8];
            let mut prev_key: Option<Vec<u8>> = None;
            loop {
                let t0 = std::time::Instant::now();
                let page = match tree.range(&cursor, &end, SCAN_PAGE).await {
                    Ok(p) => p,
                    Err(e) => {
                        suspects.push(Suspect {
                            kind: SuspectKind::C1Walk {
                                vol: vol_idx,
                                tree: tree_id,
                                cursor: cursor.clone(),
                                error: e.to_string(),
                            },
                        });
                        break; // the walk cannot advance past an unreadable node
                    }
                };
                counters.nodes_walked += 1;
                let Some((last_key, _)) = page.last() else {
                    break;
                };
                cursor = crate::meta_backend::kv::node::key_successor(last_key);
                for (k, v) in &page {
                    // In-page + cross-page ordering (checksum-valid
                    // structural damage surfaces here).
                    if let Some(prev) = &prev_key {
                        if k.as_ref() <= prev.as_slice() {
                            suspects.push(Suspect {
                                kind: SuspectKind::C1Record {
                                    vol: vol_idx,
                                    tree: tree_id,
                                    key: k.to_vec(),
                                    why: "key ordering violated".to_string(),
                                },
                            });
                        }
                    }
                    prev_key = Some(k.to_vec());
                    let why = record_schema_violation(tree_id, k, v);
                    if let Some(why) = why {
                        suspects.push(Suspect {
                            kind: SuspectKind::C1Record {
                                vol: vol_idx,
                                tree: tree_id,
                                key: k.to_vec(),
                                why,
                            },
                        });
                    }
                }
                throttle(opts, t0.elapsed()).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Census walk (the `df` walk shape: inode tree pages + layout xattrs)
// ---------------------------------------------------------------------------

async fn walk_census(
    ctx: &FsckCtx,
    opts: &FsckOptions,
    counters: &mut FsckCounters,
) -> Result<CensusOut> {
    use crate::meta_backend::kv::record::{decode_inode_key, inode_key, InodeValue};
    let block_size = ctx
        .router
        .block_size
        .load(std::sync::atomic::Ordering::Relaxed) as usize;
    let mut out = CensusOut {
        refs: HashMap::new(),
        mappings: Vec::new(),
        unresolvable: Vec::new(),
        inodes_scanned: 0,
    };
    for (vol_idx, kv) in ctx.meta.volumes.iter().enumerate() {
        let inodes = kv.trees()[0];
        let mut cursor: Vec<u8> = inode_key(1).to_vec();
        let end = inode_key(u64::MAX - 1);
        loop {
            if opts.cancel.load(Ordering::Relaxed) {
                break;
            }
            let t0 = std::time::Instant::now();
            let page = match inodes.range(&cursor, &end, SCAN_PAGE).await {
                Ok(p) => p,
                // The C1 walk owns reporting unreadable nodes; the census
                // takes what it can reach.
                Err(_) => break,
            };
            let Some((last_key, _)) = page.last() else {
                break;
            };
            cursor = crate::meta_backend::kv::node::key_successor(last_key);
            for (k, v) in &page {
                let Ok(local_ino) = decode_inode_key(k) else {
                    continue; // C1's business
                };
                let Ok(val) = InodeValue::decode(v) else {
                    continue;
                };
                if val.nlink == 0 {
                    continue;
                }
                // Guest-only members carry raw CONTROL records with no
                // global encoding — skip them (VL9 soak-found panic).
                let Some(global_ino) = ctx.meta.try_make_global_ino(local_ino, vol_idx) else {
                    continue;
                };
                if let Some((shard_k, shard_n)) = opts.shard {
                    if global_ino % shard_n as u64 != shard_k as u64 {
                        continue;
                    }
                }
                out.inodes_scanned += 1;
                let Ok(Some(bytes)) = kv.getxattr(local_ino, "layout").await else {
                    continue;
                };
                let layout: Option<crate::routing::LayoutMetadata> = if bytes.starts_with(b"{") {
                    serde_json::from_slice(&bytes).ok()
                } else {
                    bincode::deserialize(&bytes).ok()
                };
                let Some(layout) = layout else { continue };
                census_layout(ctx, global_ino, &layout, block_size, &mut out).await;
            }
            throttle(opts, t0.elapsed()).await;
        }
    }
    counters.blocks_checked = out
        .refs
        .values()
        .map(|m| m.len() as u64)
        .sum::<u64>()
        .max(counters.blocks_checked);
    Ok(out)
}

/// One layout's contribution: block-map entries (inline or indirect,
/// the indirect blob block itself included) — mirrors
/// `recover_active_blocks_v3`'s accounting exactly.
async fn census_layout(
    ctx: &FsckCtx,
    global_ino: u64,
    layout: &crate::routing::LayoutMetadata,
    block_size: usize,
    out: &mut CensusOut,
) {
    let count_ref = |mapping: &str, ino: u64, idx: u32, out: &mut CensusOut| {
        // §5.6a quarantined mapping: the reference is COUNTED (the
        // physical block is intentionally preserved and must stay
        // refcount-coherent), but the mapping is flagged so the scrub
        // and the lost checks skip it — it is the repair, not damage.
        let damaged = crate::routing::is_damaged_mapping(mapping);
        let clean = clean_key(mapping);
        match ctx.router.backend_router.parse_block_key(&clean) {
            Ok((be_id, offset)) => match canonical_backend(ctx, &be_id) {
                Some((vol, _)) => {
                    *out.refs
                        .entry(vol.clone())
                        .or_default()
                        .entry(offset)
                        .or_insert(0) += 1;
                    out.mappings.push(MappingRef {
                        ino,
                        block_idx: idx,
                        mapping: mapping.to_string(),
                        vol,
                        offset,
                        damaged,
                    });
                }
                None if damaged => {} // quarantined AND unresolvable: already isolated
                None => out.unresolvable.push(MappingRef {
                    ino,
                    block_idx: idx,
                    mapping: mapping.to_string(),
                    vol: "?".to_string(),
                    offset: 0,
                    damaged,
                }),
            },
            Err(_) if damaged => {}
            Err(_) => out.unresolvable.push(MappingRef {
                ino,
                block_idx: idx,
                mapping: mapping.to_string(),
                vol: "?".to_string(),
                offset: 0,
                damaged,
            }),
        }
    };

    let mut entries: Vec<(u32, String)> = Vec::new();
    if let Some(ref map_id) = layout.block_map_id {
        if let Some(blob_key) = map_id.strip_prefix("indirect:") {
            // The blob block itself is a referenced block.
            count_ref(blob_key, global_ino, u32::MAX, out);
            if let Ok(raw) = ctx
                .router
                .backend_router
                .read_block(blob_key, block_size)
                .await
            {
                if let Ok(decoded) = crate::routing::decode_indirect_block_map(&raw) {
                    entries = decoded;
                }
            }
        }
    }
    if entries.is_empty() {
        if let Some(ref bm) = layout.block_map {
            entries = bm.iter().map(|(&b, key)| (b, key.clone())).collect();
        }
    }
    for (b, mapping) in entries {
        count_ref(&mapping, global_ino, b, out);
    }
}

/// Strip decoration to the clean base key (`proto://offset` / `offset`).
/// A §5.6a `damaged:` quarantine marker strips to its preserved BASE key.
fn clean_key(mapping: &str) -> String {
    let mapping = mapping
        .strip_prefix(crate::routing::DAMAGED_MAPPING_PREFIX)
        .unwrap_or(mapping);
    if let Some(pos) = mapping.find("://") {
        let proto = &mapping[..pos];
        let rest = &mapping[pos + 3..];
        let offset = rest.split(':').next().unwrap_or(rest);
        format!("{proto}://{offset}")
    } else {
        mapping.split(':').next().unwrap_or(mapping).to_string()
    }
}

// ---------------------------------------------------------------------------
// C2/C3/C6 evaluation
// ---------------------------------------------------------------------------

fn evaluate_allocator_classes(
    vols: &[VolAlloc],
    census: &CensusOut,
    opts: &FsckOptions,
    counters: &mut FsckCounters,
    suspects: &mut Vec<Suspect>,
) {
    static EMPTY: once_cell::sync::Lazy<HashMap<u64, u32>> =
        once_cell::sync::Lazy::new(HashMap::new);
    let sharded = opts.shard.is_some();
    for v in vols {
        let refs = census.refs.get(&v.id).unwrap_or(&EMPTY);
        let tracked: HashMap<u64, u32> = v.alloc.tracked_offsets().into_iter().collect();
        let capacity = v.alloc.capacity_bytes();
        let chunk = v.alloc.chunk_size();

        // Leaked / C3: allocator-side ground truth is mount-session RAM —
        // meaningless on a sharded walk (a shard sees only its residue's
        // references) and on offline probes without a recovery walk.
        if !sharded {
            for (&off, &rc) in &tracked {
                counters.refcounts_checked += 1;
                match refs.get(&off) {
                    None => suspects.push(Suspect {
                        kind: SuspectKind::C2Leaked {
                            vol: v.id.clone(),
                            offset: off,
                        },
                    }),
                    Some(&n) if n != rc => suspects.push(Suspect {
                        kind: SuspectKind::C3Refcount {
                            vol: v.id.clone(),
                            offset: off,
                            expected: n,
                            actual: rc,
                        },
                    }),
                    Some(_) => {}
                }
            }
        }

        // Lost: referenced but untracked, out-of-range, or unaligned.
        // Referencers that are §5.6a `damaged:` quarantine markers are
        // never lost findings — the mapping IS the repair (the marker
        // preserves the reference for forensics and reads EIO).
        for (&off, _) in refs.iter() {
            let referencers: Vec<&MappingRef> = census
                .mappings
                .iter()
                .filter(|m| m.vol == v.id && m.offset == off)
                .collect();
            let Some(live) = referencers.iter().find(|m| !m.damaged) else {
                // No referencer at all (cross-volume alias) or every
                // referencer already quarantined: nothing to report.
                continue;
            };
            let lost = |why: String| Suspect {
                kind: SuspectKind::C2Lost {
                    vol: v.id.clone(),
                    offset: off,
                    ino: live.ino,
                    block_idx: live.block_idx,
                    mapping: live.mapping.clone(),
                    why,
                },
            };
            if capacity != 0 && off >= capacity {
                suspects.push(lost(format!("offset past device capacity {capacity}")));
            } else if off % chunk != 0 {
                suspects.push(lost(format!(
                    "offset not aligned to the {chunk} B allocator chunk"
                )));
            } else if !sharded && !tracked.contains_key(&off) {
                suspects.push(lost(
                    "referenced offset is not allocator-tracked".to_string(),
                ));
            }
        }

        // C6: used-blocks arithmetic vs the tracked population.
        if !sharded {
            let used = v
                .alloc
                .highest_block_index()
                .saturating_sub(v.alloc.free_blocks_count());
            let tracked_count = tracked.len() as u64;
            if used != tracked_count {
                suspects.push(Suspect {
                    kind: SuspectKind::C6Drift {
                        vol: v.id.clone(),
                        used,
                        tracked: tracked_count,
                    },
                });
            }
        }
    }

    // Unresolvable mappings are lost by definition (unknown backend id /
    // unparseable key — the retired-id straggler class, R8).
    for m in &census.unresolvable {
        suspects.push(Suspect {
            kind: SuspectKind::C2Lost {
                vol: "?".to_string(),
                offset: 0,
                ino: m.ino,
                block_idx: m.block_idx,
                mapping: m.mapping.clone(),
                why: format!("mapping '{}' resolves to no known backend", m.mapping),
            },
        });
    }
}

// ---------------------------------------------------------------------------
// C4/C5 staging scan
// ---------------------------------------------------------------------------

/// Parse `active_block[_ext]:inode_{ino}:block_{b}` custody keys.
fn custody_key_ino(key: &str) -> Option<u64> {
    let rest = key
        .strip_prefix("active_block_ext:")
        .or_else(|| key.strip_prefix("active_block:"))?;
    let rest = rest.strip_prefix("inode_")?;
    let ino_str = rest.split(':').next()?;
    ino_str.parse::<u64>().ok()
}

async fn scan_staging(ctx: &FsckCtx, opts: &FsckOptions, suspects: &mut Vec<Suspect>) {
    for dir in &ctx.staging_dirs {
        // C5: generation validity.
        if let Some(expected) = &ctx.expected_generation {
            match crate::cache::nvme::read_staging_generation_marker(dir).await {
                Ok(Some(found)) if &found == expected => {}
                Ok(Some(found)) => suspects.push(Suspect {
                    kind: SuspectKind::C5Generation {
                        dir: dir.clone(),
                        why: format!(
                            "generation marker '{found}' does not match the mounted \
                             volume-set generation '{expected}'"
                        ),
                    },
                }),
                Ok(None) => {
                    if crate::cache::nvme::dir_has_segment_data(&dir.join("staging_segment")) {
                        suspects.push(Suspect {
                            kind: SuspectKind::C5Generation {
                                dir: dir.clone(),
                                why: "staging dir holds segment data with no readable \
                                      generation marker"
                                    .to_string(),
                            },
                        });
                    }
                }
                Err(e) => suspects.push(Suspect {
                    kind: SuspectKind::C5Generation {
                        dir: dir.clone(),
                        why: format!("generation marker unreadable: {e}"),
                    },
                }),
            }
        }

        // C4: orphan custody records.
        let keys = match crate::cache::nvme::scan_live_staged_custody(dir, CUSTODY_SCAN_MAX).await {
            Ok(keys) => keys,
            Err(e) => {
                log::warn!("fsck: staged custody scan of {} failed: {e}", dir.display());
                continue;
            }
        };
        for key in keys {
            let Some(ino) = custody_key_ino(&key) else {
                continue;
            };
            if let Some((k, n)) = opts.shard {
                if ino % n as u64 != k as u64 {
                    continue;
                }
            }
            let missing = match ctx.meta.getattr(ino).await {
                Ok(inode) => inode.nlink == 0,
                Err(e) if is_not_found(&e) => true,
                Err(_) => false, // infrastructure error: never a finding
            };
            if missing {
                suspects.push(Suspect {
                    kind: SuspectKind::C4Orphan {
                        dir: dir.clone(),
                        key,
                        ino,
                    },
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Re-check (settle done): the class-specific verification ladders
// ---------------------------------------------------------------------------

async fn recheck_suspects(
    ctx: &FsckCtx,
    opts: &FsckOptions,
    vols: &[VolAlloc],
    suspects: Vec<Suspect>,
    counters: &mut FsckCounters,
    findings: &mut Vec<FsckFinding>,
) -> Result<()> {
    let online = opts.mode == FsckMode::Online;
    let ledger: Vec<String> = crate::jobs::mover_prepublish_ledger()
        .into_iter()
        .map(|k| clean_key(&k))
        .collect();
    let alloc_of = |vol: &str| vols.iter().find(|v| v.id == vol).map(|v| v.alloc.clone());

    // Phase A (C2/C3): epoch filter, then — for two-epoch survivors —
    // the in-flight registry, THEN the mover ledger. Registry-absence
    // strictly precedes the Phase-B reference re-read (the §5.6
    // normative order; the hook marks the boundary).
    let mut pending: Vec<Suspect> = Vec::new();
    for s in suspects {
        match &s.kind {
            SuspectKind::C2Leaked { vol, offset }
            | SuspectKind::C2Lost { vol, offset, .. }
            | SuspectKind::C3Refcount { vol, offset, .. } => {
                let Some(alloc) = alloc_of(vol) else {
                    pending.push(s);
                    continue;
                };
                if online && alloc.allocation_epoch_of(*offset).is_some() {
                    counters.epoch_exempted += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                let key = ctx.router.backend_router.persist_block_key(vol, *offset);
                if online {
                    fire_pre_registry_hook(&key);
                }
                if online && alloc.inflight_contains(*offset) {
                    counters.inflight_exempted += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                if ledger.iter().any(|k| k == &key || k == &clean_key(&key)) {
                    counters.mover_ledger_exempted += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                pending.push(s);
            }
            _ => pending.push(s),
        }
    }

    // Phase B: ONE fresh reference walk (after every registry check —
    // an owner that deregistered before its Phase-A read has, by the
    // registry contract, already made its publish visible to this walk).
    let needs_fresh = pending.iter().any(|s| {
        matches!(
            s.kind,
            SuspectKind::C2Leaked { .. }
                | SuspectKind::C2Lost { .. }
                | SuspectKind::C3Refcount { .. }
                | SuspectKind::C6Drift { .. }
        )
    });
    let fresh = if needs_fresh {
        Some(walk_census(ctx, opts, counters).await?)
    } else {
        None
    };

    // Phase C: per-suspect final verification.
    static EMPTY: once_cell::sync::Lazy<HashMap<u64, u32>> =
        once_cell::sync::Lazy::new(HashMap::new);
    for s in pending {
        if opts.cancel.load(Ordering::Relaxed) {
            break;
        }
        let verdict: Option<FsckFinding> = match &s.kind {
            SuspectKind::C2Leaked { vol, offset } => {
                let fresh = fresh.as_ref().expect("fresh walk ran");
                let refs = fresh.refs.get(vol).unwrap_or(&EMPTY);
                let still_tracked = alloc_of(vol)
                    .map(|a| a.refcount(*offset).is_some())
                    .unwrap_or(false);
                (still_tracked && !refs.contains_key(offset)).then(|| FsckFinding {
                    class: "C2".to_string(),
                    object: format!("{vol}:{offset}"),
                    evidence: "leaked block: allocated (tracked) with zero referencers, \
                               registry-cleared across two scan epochs"
                        .to_string(),
                    identity: Some(FindingId::C2Leaked {
                        vol: vol.clone(),
                        offset: *offset,
                    }),
                })
            }
            SuspectKind::C2Lost {
                vol,
                offset,
                ino,
                block_idx,
                mapping,
                why,
            } => {
                let by = format!("ino {ino} block {block_idx}");
                if vol == "?" {
                    // Unresolvable mapping: permanent by construction.
                    Some(FsckFinding {
                        class: "C2".to_string(),
                        object: by.clone(),
                        evidence: format!("lost block: {why}"),
                        identity: Some(FindingId::C2Lost {
                            vol: vol.clone(),
                            offset: *offset,
                            ino: *ino,
                            block_idx: *block_idx,
                            mapping: mapping.clone(),
                            unrepairable_shape: true,
                        }),
                    })
                } else {
                    // §5.6 normative order for the mover's src-free
                    // adversary: observe the ALLOCATOR state first, then
                    // re-verify the REFERENCE with a fresh per-ino layout
                    // read. A drain frees a source block only after every
                    // referencing publish is durable and visible, so a
                    // mapping still present AFTER the untracked
                    // observation is a genuine loss — while the
                    // moved-and-freed race always shows the NEW mapping
                    // at this re-read and clears (the leg-13
                    // drain-concurrent FP shape).
                    let now_tracked = alloc_of(vol)
                        .map(|a| a.refcount(*offset).is_some())
                        .unwrap_or(false);
                    let out_of_range = why.contains("capacity") || why.contains("aligned");
                    let violates = out_of_range || !now_tracked;
                    let still_referenced =
                        violates && current_mapping_present(ctx, *ino, *block_idx, mapping).await;
                    (still_referenced && violates).then(|| FsckFinding {
                        class: "C2".to_string(),
                        object: format!("{vol}:{offset}"),
                        evidence: format!("lost block ({by}): {why}"),
                        identity: Some(FindingId::C2Lost {
                            vol: vol.clone(),
                            offset: *offset,
                            ino: *ino,
                            block_idx: *block_idx,
                            mapping: mapping.clone(),
                            unrepairable_shape: out_of_range,
                        }),
                    })
                }
            }
            SuspectKind::C3Refcount { vol, offset, .. } => {
                let fresh = fresh.as_ref().expect("fresh walk ran");
                let refs = fresh.refs.get(vol).unwrap_or(&EMPTY);
                let expected = refs.get(offset).copied().unwrap_or(0);
                let actual = alloc_of(vol).and_then(|a| a.refcount(*offset));
                match actual {
                    Some(actual) if expected > 0 && actual != expected => Some(FsckFinding {
                        class: "C3".to_string(),
                        object: format!("{vol}:{offset}"),
                        evidence: format!(
                            "refcount {actual} != {expected} counted references \
                             (clone-aware walk; mover ledger cleared)"
                        ),
                        identity: Some(FindingId::C3Refcount {
                            vol: vol.clone(),
                            offset: *offset,
                        }),
                    }),
                    _ => None, // untracked/unreferenced shapes are C2's business
                }
            }
            SuspectKind::C6Drift { vol, .. } => {
                let Some(alloc) = alloc_of(vol) else {
                    continue;
                };
                let used = alloc
                    .highest_block_index()
                    .saturating_sub(alloc.free_blocks_count());
                let tracked = alloc.tracked_offsets().len() as u64;
                (used != tracked).then(|| FsckFinding {
                    class: "C6".to_string(),
                    object: vol.clone(),
                    evidence: format!(
                        "capacity census drift: used-blocks accounting {used} vs \
                         {tracked} tracked refcounted blocks, stable across both \
                         scan epochs"
                    ),
                    identity: Some(FindingId::C6Drift { vol: vol.clone() }),
                })
            }
            SuspectKind::C1Walk {
                vol, tree, cursor, ..
            } => {
                // Re-attempt the read (a transient I/O error clears).
                let kv = &ctx.meta.volumes[*vol];
                let tree_ref = kv
                    .trees()
                    .into_iter()
                    .find(|t| t.tree_id() == *tree)
                    .expect("tree exists");
                // Same-shaped read as the scan (a max=1 probe can be
                // satisfied by a healthy left sibling and never touch
                // the damaged node).
                match tree_ref
                    .range(
                        cursor,
                        &crate::meta_backend::kv::tree::KEY_SPACE_MAX,
                        SCAN_PAGE,
                    )
                    .await
                {
                    Err(e) => Some(FsckFinding {
                        class: "C1".to_string(),
                        object: format!("vol{vol}/tree{tree}/cursor{}", hex(cursor)),
                        evidence: format!("tree walk failed (checksum/undecodable node): {e}"),
                        identity: Some(FindingId::C1Torn {
                            vol: *vol,
                            tree: *tree,
                            cursor_hex: hex(cursor),
                        }),
                    }),
                    Ok(_) => None,
                }
            }
            SuspectKind::C1Record {
                vol,
                tree,
                key,
                why,
            } => {
                let kv = &ctx.meta.volumes[*vol];
                // Final check under the owning ino's 4a lease where the
                // key names one (online).
                let ino = owning_ino(*tree, key);
                let _lease = match (online, ino) {
                    (true, Some(local)) => Some(kv.dlm().lock_inode_exclusive(local).await),
                    _ => None,
                };
                let tree_ref = kv
                    .trees()
                    .into_iter()
                    .find(|t| t.tree_id() == *tree)
                    .expect("tree exists");
                match tree_ref.lookup(key).await {
                    Ok(Some(_)) => Some(FsckFinding {
                        class: "C1".to_string(),
                        object: format!("vol{vol}/tree{tree}/key{}", hex(key)),
                        evidence: format!("checksum-valid semantic damage: {why}"),
                        identity: Some(FindingId::C1Semantic {
                            vol: *vol,
                            tree: *tree,
                            key_hex: hex(key),
                        }),
                    }),
                    Ok(None) => None, // record vanished (live delete)
                    Err(e) => Some(FsckFinding {
                        class: "C1".to_string(),
                        object: format!("vol{vol}/tree{tree}/key{}", hex(key)),
                        evidence: format!("record unreadable at re-check: {e}"),
                        identity: Some(FindingId::C1Semantic {
                            vol: *vol,
                            tree: *tree,
                            key_hex: hex(key),
                        }),
                    }),
                }
            }
            SuspectKind::C4Orphan { dir, key, ino } => {
                let (vol_idx, local) = ctx.meta.route_ino(*ino);
                let _lease = if online {
                    Some(
                        ctx.meta.volumes[vol_idx]
                            .dlm()
                            .lock_inode_exclusive(local)
                            .await,
                    )
                } else {
                    None
                };
                // Custody still present?
                let still_present =
                    match crate::cache::nvme::scan_live_staged_custody(dir, CUSTODY_SCAN_MAX).await
                    {
                        Ok(keys) => keys.iter().any(|k| k == key),
                        Err(_) => false,
                    };
                // Re-read through the RAW per-volume backend: the routed
                // getattr takes its own shared 4a lease and would
                // self-deadlock against the exclusive lease held above.
                let still_missing = match ctx.meta.volumes[vol_idx].getattr(local).await {
                    Ok(inode) => inode.nlink == 0,
                    Err(e) if is_not_found(&e) => true,
                    Err(_) => false,
                };
                (still_present && still_missing).then(|| FsckFinding {
                    class: "C4".to_string(),
                    object: key.clone(),
                    evidence: format!(
                        "orphan staged record in {}: ino {ino} has no live inode meta",
                        dir.display()
                    ),
                    identity: Some(FindingId::C4Orphan {
                        dir: dir.clone(),
                        key: key.clone(),
                        ino: *ino,
                    }),
                })
            }
            SuspectKind::C5Generation { dir, why } => {
                // Re-read the marker (a live restamp clears).
                let expected = ctx.expected_generation.as_deref().unwrap_or("");
                match crate::cache::nvme::read_staging_generation_marker(dir).await {
                    Ok(Some(found)) if found == expected => None,
                    _ => Some(FsckFinding {
                        class: "C5".to_string(),
                        object: dir.display().to_string(),
                        evidence: why.clone(),
                        identity: Some(FindingId::C5Staging { dir: dir.clone() }),
                    }),
                }
            }
        };
        match verdict {
            Some(f) => findings.push(f),
            None => counters.suspects_cleared += 1,
        }
    }
    Ok(())
}

/// The local ino a record key belongs to (per tree schema).
fn owning_ino(tree: u8, key: &[u8]) -> Option<u64> {
    use crate::meta_backend::kv::record::{
        decode_dentry_key, decode_inode_key, decode_xattr_key, TREE_DENTRIES, TREE_INODES,
        TREE_XATTRS,
    };
    match tree {
        t if t == TREE_INODES => decode_inode_key(key).ok(),
        t if t == TREE_DENTRIES => decode_dentry_key(key).ok().map(|(parent, _, _)| parent),
        t if t == TREE_XATTRS => decode_xattr_key(key).ok().map(|(ino, _, _)| ino),
        _ => None,
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ---------------------------------------------------------------------------
// C7: the data scrub (KD-17)
// ---------------------------------------------------------------------------

async fn scrub_c7(
    ctx: &FsckCtx,
    opts: &FsckOptions,
    census: &CensusOut,
    counters: &mut FsckCounters,
    findings: &mut Vec<FsckFinding>,
) {
    let crypto = ctx.router.get_crypto().clone();
    let block_size = ctx
        .router
        .block_size
        .load(std::sync::atomic::Ordering::Relaxed) as usize;
    // Device-bound sequential order: sort by (volume, offset).
    let mut work: Vec<&MappingRef> = census.mappings.iter().collect();
    work.sort_by_key(|m| clean_key(&m.mapping));
    let mut t0 = std::time::Instant::now();
    let mut in_batch = 0usize;
    for m in work {
        if opts.cancel.load(Ordering::Relaxed) {
            break;
        }
        if m.damaged {
            // §5.6a quarantined mapping: already isolated (reads EIO,
            // block preserved for forensics) — never re-scrubbed, never
            // re-reported.
            continue;
        }
        match verify_stored_block(ctx, &crypto, m, block_size).await {
            ScrubOutcome::Aead(bytes) => {
                counters.scrub_aead_verified += 1;
                counters.scrub_blocks_scanned += 1;
                counters.scrub_bytes_scanned += bytes;
            }
            ScrubOutcome::Frame(bytes) => {
                counters.scrub_frame_verified += 1;
                counters.scrub_blocks_scanned += 1;
                counters.scrub_bytes_scanned += bytes;
            }
            ScrubOutcome::ReadableOnly(bytes) => {
                counters.scrub_readability_only += 1;
                counters.scrub_blocks_scanned += 1;
                counters.scrub_bytes_scanned += bytes;
            }
            ScrubOutcome::Failed(evidence) => {
                counters.scrub_blocks_scanned += 1;
                // Suspect verification: online, a failure re-verifies
                // under a validated pin after re-resolving the mapping —
                // a moved mapping or an in-flight patch clears.
                let confirmed = if opts.mode == FsckMode::Online {
                    reverify_scrub_failure(ctx, &crypto, m, block_size).await
                } else {
                    true
                };
                if confirmed {
                    counters.scrub_failures += 1;
                    findings.push(FsckFinding {
                        class: "C7".to_string(),
                        object: format!("ino {} block {} ({})", m.ino, m.block_idx, m.mapping),
                        evidence,
                        identity: Some(FindingId::C7Scrub {
                            ino: m.ino,
                            block_idx: m.block_idx,
                            mapping: m.mapping.clone(),
                        }),
                    });
                } else {
                    counters.suspects_cleared += 1;
                }
            }
            ScrubOutcome::Skipped => {}
        }
        in_batch += 1;
        if in_batch >= SCRUB_BATCH {
            throttle(opts, t0.elapsed()).await;
            t0 = std::time::Instant::now();
            in_batch = 0;
        }
    }
}

enum ScrubOutcome {
    Aead(u64),
    Frame(u64),
    ReadableOnly(u64),
    Failed(String),
    Skipped,
}

/// Read the stored image for a mapping and verify what its stored form
/// makes verifiable (KD-17).
async fn verify_stored_block(
    ctx: &FsckCtx,
    crypto: &crate::crypto_compress::CryptoCompressState,
    m: &MappingRef,
    block_size: usize,
) -> ScrubOutcome {
    let br = &ctx.router.backend_router;
    let clean = clean_key(&m.mapping);
    // Decoration: `bk:rel:len` carries the EXACT stored image geometry.
    let decorated = {
        let rest = match m.mapping.find("://") {
            Some(p) => &m.mapping[p + 3..],
            None => m.mapping.as_str(),
        };
        let parts: Vec<&str> = rest.split(':').collect();
        if parts.len() == 3 {
            match (parts[1].parse::<u64>(), parts[2].parse::<usize>()) {
                (Ok(rel), Ok(len)) => Some((rel, len)),
                _ => None,
            }
        } else {
            None
        }
    };
    let (base_be, base_off) = match br.parse_block_key(&clean) {
        Ok(x) => x,
        Err(_) => return ScrubOutcome::Skipped, // C2 lost owns unparseable mappings
    };
    let Some((_vol, _alloc)) = canonical_backend(ctx, &base_be) else {
        return ScrubOutcome::Skipped;
    };
    let (read_off, read_len, exact_len) = match decorated {
        Some((rel, len)) => (base_off + rel, len.div_ceil(4096) * 4096, Some(len)),
        None => {
            // Undecorated whole-block mapping: transformed images carry
            // a self-delimiting frame and may exceed `block_size`
            // (encrypt envelope + frame — the FIND-RW4-A headroom), so
            // the window is the reader-side stored-image bound.
            let window = if crypto.is_passthrough() {
                block_size
            } else {
                crypto
                    .max_stored_image_len(block_size)
                    .div_ceil(4096)
                    .saturating_mul(4096)
            };
            (base_off, window, None)
        }
    };
    if let Some(hook) = SCRUB_READ_FAULT_HOOK.read().clone() {
        if hook(read_off) {
            return ScrubOutcome::Failed(
                "device read error: injected fault (test hook)".to_string(),
            );
        }
    }
    let (_, dev) = match br.get_backend(&base_be) {
        Ok(x) => x,
        Err(_) => {
            // Backend registered but unhealthy — reads refuse; a scrub of
            // a disabled volume reports the read status honestly.
            return ScrubOutcome::Failed("backend offline for scrub read".to_string());
        }
    };
    let image = match dev.read_block(read_off, read_len).await {
        Ok(b) => b,
        Err(e) => return ScrubOutcome::Failed(format!("device read error: {e}")),
    };
    if let Some(len) = exact_len {
        if image.len() < len {
            return ScrubOutcome::Failed(format!(
                "short device read: {} of {len} bytes",
                image.len()
            ));
        }
    }
    let image = match exact_len {
        Some(len) if image.len() > len => image.slice(0..len),
        _ => image,
    };
    let bytes = image.len() as u64;
    if crypto.is_passthrough() {
        // No stored checksum exists: read success is the honest verdict
        // (`scrub_readability_only` — the OQ-B gap, stated not faked).
        return ScrubOutcome::ReadableOnly(bytes);
    }
    match crypto.process_read(&image) {
        Ok(_) => {
            if crypto.encrypt_mode != crate::crypto_compress::EncryptMode::None {
                ScrubOutcome::Aead(bytes)
            } else {
                ScrubOutcome::Frame(bytes)
            }
        }
        Err(e) => {
            if crypto.encrypt_mode != crate::crypto_compress::EncryptMode::None {
                ScrubOutcome::Failed(format!("AEAD verification failed: {e}"))
            } else {
                ScrubOutcome::Failed(format!("transform frame undecodable: {e}"))
            }
        }
    }
}

/// Online scrub-failure re-verification: the mapping must still be
/// referenced verbatim, and the re-read must fail again under a
/// VALIDATED pin (an in-flight patch — unstable incarnation — or a
/// racing free clears the suspect instead of reporting a torn read).
async fn reverify_scrub_failure(
    ctx: &FsckCtx,
    crypto: &crate::crypto_compress::CryptoCompressState,
    m: &MappingRef,
    block_size: usize,
) -> bool {
    // Re-resolve the ino's CURRENT layout.
    let (vol_idx, local) = ctx.meta.route_ino(m.ino);
    let Some(kv) = ctx.meta.volumes.get(vol_idx) else {
        return false;
    };
    let Ok(Some(bytes)) = kv.getxattr(local, "layout").await else {
        return false; // layout gone: mapping superseded
    };
    let layout: Option<crate::routing::LayoutMetadata> = if bytes.starts_with(b"{") {
        serde_json::from_slice(&bytes).ok()
    } else {
        bincode::deserialize(&bytes).ok()
    };
    let Some(layout) = layout else { return false };
    let mut still_mapped = layout
        .block_map
        .as_ref()
        .is_some_and(|bm| bm.get(&m.block_idx).is_some_and(|v| v == &m.mapping));
    if !still_mapped {
        // Indirect maps / blob references: re-read through the census
        // decoder for this single layout.
        let mut probe = CensusOut {
            refs: HashMap::new(),
            mappings: Vec::new(),
            unresolvable: Vec::new(),
            inodes_scanned: 0,
        };
        census_layout(ctx, m.ino, &layout, block_size, &mut probe).await;
        still_mapped = probe.mappings.iter().any(|p| p.mapping == m.mapping);
    }
    if !still_mapped {
        return false;
    }
    let clean = clean_key(&m.mapping);
    match ctx.router.backend_router.pin_block_validated(&clean) {
        crate::block_allocator::PinOutcome::Pinned => {
            let outcome = verify_stored_block(ctx, crypto, m, block_size).await;
            let _ = ctx.router.backend_router.free_block(&clean).await; // unpin
            matches!(outcome, ScrubOutcome::Failed(_))
        }
        crate::block_allocator::PinOutcome::PinnedUnstable => {
            let _ = ctx.router.backend_router.free_block(&clean).await; // unpin
            false // patch mid-flight: torn read, not corruption
        }
        crate::block_allocator::PinOutcome::Refused => false, // freed under us
    }
}

// ---------------------------------------------------------------------------
// Throttle + metrics
// ---------------------------------------------------------------------------

async fn throttle(opts: &FsckOptions, elapsed: Duration) {
    if let Some(delay) = crate::jobs::job_throttle_sleep(elapsed, opts.throttle_pct) {
        tokio::time::sleep(delay).await;
    }
}

fn publish_metrics(c: &FsckCounters) {
    use crate::fuse_client::METRICS;
    let m = &*METRICS;
    m.fsck_inodes_scanned
        .fetch_add(c.inodes_scanned, Ordering::Relaxed);
    m.fsck_nodes_walked
        .fetch_add(c.nodes_walked, Ordering::Relaxed);
    m.fsck_blocks_checked
        .fetch_add(c.blocks_checked, Ordering::Relaxed);
    m.fsck_refcounts_checked
        .fetch_add(c.refcounts_checked, Ordering::Relaxed);
    m.fsck_suspects.fetch_add(c.suspects, Ordering::Relaxed);
    m.fsck_suspects_cleared
        .fetch_add(c.suspects_cleared, Ordering::Relaxed);
    m.fsck_epoch_exempted
        .fetch_add(c.epoch_exempted, Ordering::Relaxed);
    m.fsck_inflight_exempted
        .fetch_add(c.inflight_exempted, Ordering::Relaxed);
    m.fsck_mover_ledger_exempted
        .fetch_add(c.mover_ledger_exempted, Ordering::Relaxed);
    m.fsck_findings.fetch_add(c.findings, Ordering::Relaxed);
    m.fsck_scan_secs.store(c.scan_secs, Ordering::Relaxed);
    m.scrub_blocks_scanned
        .fetch_add(c.scrub_blocks_scanned, Ordering::Relaxed);
    m.scrub_bytes_scanned
        .fetch_add(c.scrub_bytes_scanned, Ordering::Relaxed);
    m.scrub_aead_verified
        .fetch_add(c.scrub_aead_verified, Ordering::Relaxed);
    m.scrub_frame_verified
        .fetch_add(c.scrub_frame_verified, Ordering::Relaxed);
    m.scrub_readability_only
        .fetch_add(c.scrub_readability_only, Ordering::Relaxed);
    m.scrub_failures
        .fetch_add(c.scrub_failures, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// PR VL6b — §5.6a repair: per-class actions, dry-run default,
// quarantine-first, verify-before-repair
// ---------------------------------------------------------------------------

/// Repair invocation options. **Dry-run is the default** (`apply =
/// false`): the planner emits per-finding actions and mutates NOTHING.
#[derive(Clone, Debug, Default)]
pub struct RepairOptions {
    /// Execute the plan (`--repair --apply`). Requires coordinator /
    /// D0-guarded authority — the CLI enforces the posture; the engine
    /// enforces per-object leases.
    pub apply: bool,
    /// Quarantine home override (`--quarantine-dir`). Default:
    /// `<first staging dir>/quarantine/`; REQUIRED on cache-less
    /// filesystems when any action needs quarantine.
    pub quarantine_dir: Option<PathBuf>,
}

/// One planned / applied / refused action (§5.6a table verbs).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RepairAction {
    pub class: String,
    pub object: String,
    /// The table verb: `rebuild-in-place`, `quarantine-report-only`,
    /// `free-leaked-block`, `repair-allocator`, `quarantine-mapping`,
    /// `recount-and-set-refcount`, `quarantine-then-discard-custody`,
    /// `quarantine-staging-dir`, `recompute-accounting`.
    pub action: String,
    pub detail: String,
}

/// §10 repair counters, per run (the process gauges in
/// [`crate::fuse_client::METRICS`] accumulate the same names).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct RepairCounters {
    pub planned: u64,
    pub applied: u64,
    /// Verify-before-repair refusals (stale/healed findings, missing
    /// identity, unreachable authority). Never an error: the state
    /// moved on and the repair honestly declined.
    pub refused: u64,
    pub quarantined_records: u64,
    pub quarantined_blocks: u64,
    pub quarantined_bytes: u64,
    /// Applied repairs per class (`"C1"`..`"C7"`).
    pub per_class: std::collections::BTreeMap<String, u64>,
}

/// The structured repair report (embedded in [`FsckReport::repair`]).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RepairReport {
    pub schema: u32,
    pub dry_run: bool,
    pub planned: Vec<RepairAction>,
    pub applied: Vec<RepairAction>,
    /// Refusals with their reasons in `detail`.
    pub refused: Vec<RepairAction>,
    /// The per-run quarantine directory, when anything was quarantined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quarantine_dir: Option<String>,
    pub counters: RepairCounters,
}

/// Kill-9-window test hook: invoked with `"<class>:<object>"` AFTER the
/// action's quarantine copies are durable and BEFORE its commit
/// mutation. Returning `true` aborts the run right there (the injected
/// crash) — quarantine exists, the mutation never happened, the finding
/// is re-detected by the next run (the §5.6a convergence law under
/// test). `None` in production.
static REPAIR_ABORT_HOOK: parking_lot::RwLock<Option<Arc<dyn Fn(&str) -> bool + Send + Sync>>> =
    parking_lot::RwLock::new(None);

pub fn set_repair_abort_hook(hook: Arc<dyn Fn(&str) -> bool + Send + Sync>) {
    *REPAIR_ABORT_HOOK.write() = Some(hook);
}

pub fn clear_repair_abort_hook() {
    *REPAIR_ABORT_HOOK.write() = None;
}

fn fire_repair_abort_hook(what: &str) -> Result<()> {
    let hook = REPAIR_ABORT_HOOK.read().clone();
    if let Some(h) = hook {
        if h(what) {
            return Err(SqueezefsError::InvalidOperation(format!(
                "fsck repair aborted by test hook at {what} (injected kill-9 window: \
                 quarantine durable, commit never ran — re-run converges)"
            )));
        }
    }
    Ok(())
}

/// The per-run quarantine directory + JSON manifest (§5.6a: "nothing is
/// destroyed without a copy"). Lazily created on the first quarantined
/// byte; every entry append rewrites + fdatasyncs `manifest.json`
/// BEFORE the action's commit mutation runs, so a crash between
/// quarantine and commit always leaves an auditable copy.
struct Quarantine {
    home: Option<PathBuf>,
    run_dir: Option<PathBuf>,
    entries: Vec<serde_json::Value>,
    seq: u32,
}

impl Quarantine {
    fn new(ctx: &FsckCtx, opts: &RepairOptions) -> Self {
        let home = opts
            .quarantine_dir
            .clone()
            .or_else(|| ctx.staging_dirs.first().map(|d| d.join("quarantine")));
        Quarantine {
            home,
            run_dir: None,
            entries: Vec::new(),
            seq: 0,
        }
    }

    async fn run_dir(&mut self) -> Result<PathBuf> {
        if let Some(d) = &self.run_dir {
            return Ok(d.clone());
        }
        let home = self.home.clone().ok_or_else(|| {
            SqueezefsError::InvalidOperation(
                "repair needs a quarantine home and this filesystem is cache-less: \
                 pass --quarantine-dir (§5.6a — nothing is destroyed without a copy)"
                    .to_string(),
            )
        })?;
        let run_id = format!(
            "{:x}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            std::process::id()
        );
        let dir = home.join(run_id);
        tokio::fs::create_dir_all(&dir).await.map_err(|e| {
            SqueezefsError::Io(std::io::Error::new(
                e.kind(),
                format!("creating quarantine dir {}: {e}", dir.display()),
            ))
        })?;
        self.run_dir = Some(dir.clone());
        Ok(dir)
    }

    /// Copy `parts` (suffix → bytes) into the run dir, fsync each, append
    /// the manifest entry, rewrite + fsync the manifest. Returns total
    /// bytes copied.
    async fn put(
        &mut self,
        class: &str,
        object: &str,
        action: &str,
        note: &str,
        parts: &[(&str, &[u8])],
    ) -> Result<u64> {
        let dir = self.run_dir().await?;
        self.seq += 1;
        let seq = self.seq;
        let mut files = Vec::new();
        let mut total = 0u64;
        for (i, (suffix, bytes)) in parts.iter().enumerate() {
            let name = format!("{seq:04}_{class}_{i}_{suffix}.bin");
            let path = dir.join(&name);
            crate::uring_fs::write_all(&path, bytes.to_vec()).await?;
            crate::uring_fs::fdatasync(&path).await?;
            total += bytes.len() as u64;
            files.push(serde_json::json!({ "name": name, "bytes": bytes.len() }));
        }
        self.entries.push(serde_json::json!({
            "class": class,
            "object": object,
            "action": action,
            "note": note,
            "files": files,
        }));
        self.write_manifest().await?;
        Ok(total)
    }

    async fn write_manifest(&mut self) -> Result<()> {
        let dir = self.run_dir().await?;
        let manifest = serde_json::json!({
            "schema": 1u32,
            "created_unix": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            "entries": self.entries,
        });
        let path = dir.join("manifest.json");
        crate::uring_fs::write_all(
            &path,
            serde_json::to_vec_pretty(&manifest).unwrap_or_default(),
        )
        .await?;
        crate::uring_fs::fdatasync(&path).await
    }
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}

/// The §5.6a table verb a finding plans to (dry run and apply share the
/// planner — the plan IS what apply executes).
fn planned_action(id: &FindingId) -> (&'static str, String) {
    match id {
        FindingId::C1Torn { .. } => (
            "quarantine-report-only",
            "torn node: no replicas exist — quarantine identity + report, never \
             fabrication (data loss made visible and bounded)"
                .to_string(),
        ),
        FindingId::C1Semantic { .. } => (
            "rebuild-in-place",
            "checksum-valid structural damage: quarantine the record bytes, then drop \
             the schema-violating record via an ordinary journaled CoW leaf re-emit"
                .to_string(),
        ),
        FindingId::C2Leaked { vol, offset } => (
            "free-leaked-block",
            format!(
                "free {vol}:{offset} via begin_free → purge → punch → finish_free \
                 (bytes quarantined first)"
            ),
        ),
        FindingId::C2Lost {
            unrepairable_shape, ..
        } => {
            if *unrepairable_shape {
                (
                    "quarantine-mapping",
                    "out-of-range/unresolvable mapping: replace with an explicit \
                     `damaged:` marker (reads EIO) — a hole is never silently fabricated"
                        .to_string(),
                )
            } else {
                (
                    "verify-content-then-repair-allocator-or-quarantine",
                    "verify the block content first; verifiable ⇒ repair the allocator \
                     (the data was fine, the accounting was wrong); unverifiable ⇒ \
                     quarantine the mapping (`damaged:` marker, reads EIO)"
                        .to_string(),
                )
            }
        }
        FindingId::C3Refcount { vol, offset } => (
            "recount-and-set-refcount",
            format!(
                "recount {vol}:{offset}'s references under every referencing ino's \
                 DLM lease (ascending) and set the refcount to the counted value"
            ),
        ),
        FindingId::C4Orphan { key, .. } => (
            "quarantine-then-discard-custody",
            format!(
                "quarantine a verbatim copy of the staged record + payload for '{key}', \
                 then discard via the recovery discard law (magic retire)"
            ),
        ),
        FindingId::C5Staging { dir } => (
            "quarantine-staging-dir",
            format!(
                "move {}'s stale-generation marker + staged segment files aside into \
                 quarantine (the discard-on-mismatch law with a copy retained)",
                dir.display()
            ),
        ),
        FindingId::C6Drift { vol } => (
            "recompute-accounting",
            format!("recompute {vol}'s derived used/free accounting from the tracked census"),
        ),
        FindingId::C7Scrub { mapping, .. } => (
            "quarantine-mapping",
            format!(
                "replace '{mapping}' with an explicit `damaged:` marker (reads EIO); \
                 the physical block stays in place for forensics"
            ),
        ),
    }
}

/// Is the ino's CURRENT layout still carrying `mapping` (verbatim,
/// non-quarantined) at `block_idx`? The identity-precise
/// verify-before-repair re-check for the mapping classes.
async fn current_mapping_present(ctx: &FsckCtx, ino: u64, block_idx: u32, mapping: &str) -> bool {
    let (vol_idx, local) = ctx.meta.route_ino(ino);
    let Some(kv) = ctx.meta.volumes.get(vol_idx) else {
        return false;
    };
    let Ok(Some(bytes)) = kv.getxattr(local, "layout").await else {
        return false;
    };
    let layout: Option<crate::routing::LayoutMetadata> = if bytes.starts_with(b"{") {
        serde_json::from_slice(&bytes).ok()
    } else {
        bincode::deserialize(&bytes).ok()
    };
    let Some(layout) = layout else { return false };
    let block_size = ctx
        .router
        .block_size
        .load(std::sync::atomic::Ordering::Relaxed) as usize;
    let mut probe = CensusOut {
        refs: HashMap::new(),
        mappings: Vec::new(),
        unresolvable: Vec::new(),
        inodes_scanned: 0,
    };
    census_layout(ctx, ino, &layout, block_size, &mut probe).await;
    probe
        .mappings
        .iter()
        .chain(probe.unresolvable.iter())
        .any(|m| m.ino == ino && m.block_idx == block_idx && m.mapping == mapping && !m.damaged)
}

/// Flip a mapping to its §5.6a `damaged:` quarantine marker — one
/// tx-atomic layout commit under the merge discipline (the 4a lease is
/// taken inside the meta transaction). **Supersession-safe**: the flip
/// rides [`crate::routing::BlockMapOp::MergeExpected`] — it lands only
/// where the captured mapping is STILL current, so a foreground rewrite
/// or a mover publish racing the repair is never overwritten (`false` =
/// superseded, the caller refuses the action). The displaced original
/// key is deliberately NOT freed: for C7 the physical block is
/// preserved for forensics; for C2-lost there is nothing allocated to
/// free.
async fn flip_mapping_damaged(
    ctx: &FsckCtx,
    ino: u64,
    block_idx: u32,
    mapping: &str,
) -> Result<bool> {
    let token = ctx.router.dlm.get_fencing_token_ino(ino);
    let damaged = format!("{}{mapping}", crate::routing::DAMAGED_MAPPING_PREFIX);
    let entries = [(block_idx, mapping.to_string(), damaged)];
    let displaced = ctx
        .router
        .merge_block_mappings(
            ino,
            crate::routing::BlockMapOp::MergeExpected(&entries),
            0,
            crate::routing::LayoutFlip::KeepLayout,
            token,
        )
        .await?;
    Ok(displaced.iter().any(|d| d == mapping))
}

/// Best-effort raw copy of a mapping's stored image for quarantine
/// (`None` when the device window cannot be read — recorded honestly in
/// the manifest instead of blocking the isolation).
async fn read_stored_image_best_effort(ctx: &FsckCtx, mapping: &str) -> Option<bytes::Bytes> {
    let block_size = ctx
        .router
        .block_size
        .load(std::sync::atomic::Ordering::Relaxed) as usize;
    let clean = clean_key(mapping);
    let (be_id, offset) = ctx.router.backend_router.parse_block_key(&clean).ok()?;
    let (_, alloc_dev) = canonical_backend(ctx, &be_id)?;
    let _ = alloc_dev; // canonicalization proves the backend exists
    let (_, dev) = ctx.router.backend_router.get_backend(&be_id).ok()?;
    let crypto = ctx.router.get_crypto();
    let window = if crypto.is_passthrough() {
        block_size
    } else {
        crypto
            .max_stored_image_len(block_size)
            .div_ceil(4096)
            .saturating_mul(4096)
    };
    dev.read_block(offset, window).await.ok()
}

/// Run the §5.6a repair over a VERIFIED report's findings. Dry-run by
/// default (`opts.apply = false`): plans + returns, mutating nothing.
/// Apply mode re-verifies every finding is STILL current before acting
/// (verify-before-repair — a healed/stale finding is a refused repair,
/// counted), quarantines before every discard, and executes each action
/// as one tx-atomic commit / copy-then-retire step so kill-9 anywhere
/// leaves a consistent filesystem and a re-run converges.
pub async fn repair(
    ctx: &FsckCtx,
    report: &FsckReport,
    opts: &RepairOptions,
) -> Result<RepairReport> {
    let mut out = RepairReport {
        schema: FSCK_REPORT_SCHEMA,
        dry_run: !opts.apply,
        planned: Vec::new(),
        applied: Vec::new(),
        refused: Vec::new(),
        quarantine_dir: None,
        counters: RepairCounters::default(),
    };

    // ---- Plan (shared by dry run and apply) ----
    let mut actionable: Vec<(&FsckFinding, &FindingId)> = Vec::new();
    for f in &report.findings {
        match &f.identity {
            Some(id) => {
                let (verb, detail) = planned_action(id);
                out.planned.push(RepairAction {
                    class: f.class.clone(),
                    object: f.object.clone(),
                    action: verb.to_string(),
                    detail,
                });
                actionable.push((f, id));
            }
            None => {
                out.refused.push(RepairAction {
                    class: f.class.clone(),
                    object: f.object.clone(),
                    action: "refused".to_string(),
                    detail: "finding carries no structured identity (older-binary report) \
                             — re-run detection with this binary"
                        .to_string(),
                });
            }
        }
    }
    out.counters.planned = out.planned.len() as u64;
    out.counters.refused = out.refused.len() as u64;

    if !opts.apply {
        publish_repair_metrics(&out.counters);
        return Ok(out);
    }

    // ---- Apply ----
    let online = report.mode == "online";
    let block_size = ctx
        .router
        .block_size
        .load(std::sync::atomic::Ordering::Relaxed) as usize;
    let crypto = ctx.router.get_crypto().clone();
    let vols = volume_allocators(ctx);
    let alloc_of = |vol: &str| vols.iter().find(|v| v.id == vol).map(|v| v.alloc.clone());
    let mut quarantine = Quarantine::new(ctx, opts);

    // One fresh census for the allocator-class verifications (C2/C3),
    // walked ONCE — the repair-time ground truth.
    let needs_census = actionable.iter().any(|(_, id)| {
        matches!(
            id,
            FindingId::C2Leaked { .. } | FindingId::C2Lost { .. } | FindingId::C3Refcount { .. }
        )
    });
    let fresh = if needs_census {
        let mut scratch = FsckCounters::default();
        Some(walk_census(ctx, &FsckOptions::offline(), &mut scratch).await?)
    } else {
        None
    };

    let refuse = |out: &mut RepairReport, f: &FsckFinding, why: String| {
        out.refused.push(RepairAction {
            class: f.class.clone(),
            object: f.object.clone(),
            action: "refused".to_string(),
            detail: why,
        });
        out.counters.refused += 1;
    };
    let class_idx = |class: &str| -> Option<usize> {
        class
            .strip_prefix('C')
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|n| (1..=7).contains(n))
            .map(|n| n - 1)
    };
    let apply_ok = |out: &mut RepairReport, f: &FsckFinding, verb: &str, detail: String| {
        out.applied.push(RepairAction {
            class: f.class.clone(),
            object: f.object.clone(),
            action: verb.to_string(),
            detail,
        });
        out.counters.applied += 1;
        *out.counters.per_class.entry(f.class.clone()).or_insert(0) += 1;
        if let Some(i) = class_idx(&f.class) {
            crate::fuse_client::METRICS.fsck_repair_class[i]
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    };

    for (f, id) in actionable {
        let what = format!("{}:{}", f.class, f.object);
        match id {
            // -------------------------------------------------- C1 torn
            FindingId::C1Torn {
                vol,
                tree,
                cursor_hex,
            } => {
                // Verify: the walk still fails from this cursor.
                let Some(kv) = ctx.meta.volumes.get(*vol) else {
                    refuse(&mut out, f, "volume index no longer exists".to_string());
                    continue;
                };
                let Some(cursor) = unhex(cursor_hex) else {
                    refuse(&mut out, f, "undecodable cursor identity".to_string());
                    continue;
                };
                let Some(tree_ref) = kv.trees().into_iter().find(|t| t.tree_id() == *tree) else {
                    refuse(&mut out, f, "tree no longer exists".to_string());
                    continue;
                };
                // Same-shaped read as the scan (a max=1 probe can be
                // satisfied by a healthy left sibling and never touch the
                // damaged node — the recheck's own lesson).
                match tree_ref
                    .range(
                        &cursor,
                        &crate::meta_backend::kv::tree::KEY_SPACE_MAX,
                        SCAN_PAGE,
                    )
                    .await
                {
                    Ok(_) => {
                        refuse(
                            &mut out,
                            f,
                            "walk now succeeds from the recorded cursor (healed / \
                             transient I/O at scan time)"
                                .to_string(),
                        );
                        continue;
                    }
                    Err(e) => {
                        // The honest action: no replicas exist — record the
                        // identity + error in the quarantine manifest and
                        // REPORT. The finding persists by design (data loss
                        // made visible, never fabricated away).
                        let note = format!(
                            "torn node on vol{vol}/tree{tree} at cursor {cursor_hex}: {e}; \
                             node bytes are unreachable through the validated read path \
                             (checksum-refused) — identity recorded, no mutation performed"
                        );
                        let bytes = quarantine
                            .put(&f.class, &f.object, "quarantine-report-only", &note, &[])
                            .await?;
                        out.counters.quarantined_records += 1;
                        out.counters.quarantined_bytes += bytes;
                        fire_repair_abort_hook(&what)?;
                        apply_ok(&mut out, f, "quarantine-report-only", note);
                    }
                }
            }
            // ---------------------------------------------- C1 semantic
            FindingId::C1Semantic { vol, tree, key_hex } => {
                let Some(kv) = ctx.meta.volumes.get(*vol) else {
                    refuse(&mut out, f, "volume index no longer exists".to_string());
                    continue;
                };
                let Some(key) = unhex(key_hex) else {
                    refuse(&mut out, f, "undecodable key identity".to_string());
                    continue;
                };
                let Some(tree_ref) = kv.trees().into_iter().find(|t| t.tree_id() == *tree) else {
                    refuse(&mut out, f, "tree no longer exists".to_string());
                    continue;
                };
                // Verify under the owning ino's lease where one exists.
                let _lease = match (online, owning_ino(*tree, &key)) {
                    (true, Some(local)) => Some(kv.dlm().lock_inode_exclusive(local).await),
                    _ => None,
                };
                let value = match tree_ref.lookup(&key).await {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        refuse(&mut out, f, "record no longer exists (healed)".to_string());
                        continue;
                    }
                    Err(e) => {
                        refuse(&mut out, f, format!("record unreadable at verify: {e}"));
                        continue;
                    }
                };
                if record_schema_violation(*tree, &key, &value).is_none() {
                    refuse(
                        &mut out,
                        f,
                        "record now satisfies its tree schema (superseded in place)".to_string(),
                    );
                    continue;
                }
                // Quarantine the record bytes, then drop it via the
                // ordinary journaled CoW mutation (the leaf re-emits
                // without the record; the parent pointer swap rides the
                // existing SMO-path machinery).
                let bytes = quarantine
                    .put(
                        &f.class,
                        &f.object,
                        "rebuild-in-place",
                        "record bytes (key, value) before the drop",
                        &[("key", &key), ("value", &value)],
                    )
                    .await?;
                out.counters.quarantined_records += 1;
                out.counters.quarantined_bytes += bytes;
                fire_repair_abort_hook(&what)?;
                tree_ref.delete(&key).await.map_err(|e| {
                    SqueezefsError::InvalidOperation(format!(
                        "rebuild-in-place drop of the schema-violating record failed: {e}"
                    ))
                })?;
                apply_ok(
                    &mut out,
                    f,
                    "rebuild-in-place",
                    format!(
                        "schema-violating record dropped from vol{vol}/tree{tree} \
                         (journaled CoW re-emit); bytes quarantined"
                    ),
                );
            }
            // ------------------------------------------------ C2 leaked
            FindingId::C2Leaked { vol, offset } => {
                let Some(alloc) = alloc_of(vol) else {
                    refuse(&mut out, f, format!("volume '{vol}' no longer registered"));
                    continue;
                };
                let fresh = fresh.as_ref().expect("census walked");
                static EMPTY: once_cell::sync::Lazy<HashMap<u64, u32>> =
                    once_cell::sync::Lazy::new(HashMap::new);
                let refs = fresh.refs.get(vol).unwrap_or(&EMPTY);
                if alloc.refcount(*offset).is_none() {
                    refuse(
                        &mut out,
                        f,
                        "offset no longer tracked (already freed)".to_string(),
                    );
                    continue;
                }
                if refs.contains_key(offset) || alloc.inflight_contains(*offset) {
                    refuse(
                        &mut out,
                        f,
                        "offset is referenced or has a live in-flight owner now \
                         (published since the scan)"
                            .to_string(),
                    );
                    continue;
                }
                // Quarantine the block bytes before the free destroys them.
                let key = ctx.router.backend_router.persist_block_key(vol, *offset);
                let (note, parts): (String, Vec<(&str, &[u8])>);
                let image = read_stored_image_best_effort(ctx, &key).await;
                match &image {
                    Some(img) => {
                        note = "leaked block bytes before the free".to_string();
                        parts = vec![("block", img.as_ref())];
                    }
                    None => {
                        note = "block bytes unreadable at quarantine time — identity \
                                recorded only"
                            .to_string();
                        parts = Vec::new();
                    }
                }
                let bytes = quarantine
                    .put(&f.class, &f.object, "free-leaked-block", &note, &parts)
                    .await?;
                out.counters.quarantined_blocks += 1;
                out.counters.quarantined_bytes += bytes;
                fire_repair_abort_hook(&what)?;
                ctx.router.backend_router.free_block(&key).await?;
                apply_ok(
                    &mut out,
                    f,
                    "free-leaked-block",
                    format!("{vol}:{offset} freed (begin → purge → punch → finish)"),
                );
            }
            // -------------------------------------------------- C2 lost
            FindingId::C2Lost {
                vol,
                offset,
                ino,
                block_idx,
                mapping,
                unrepairable_shape,
            } => {
                if !current_mapping_present(ctx, *ino, *block_idx, mapping).await {
                    refuse(
                        &mut out,
                        f,
                        "the lost mapping is no longer present (truncated / rewritten / \
                         already quarantined)"
                            .to_string(),
                    );
                    continue;
                }
                let repair_allocator = if *unrepairable_shape {
                    false
                } else {
                    let Some(alloc) = alloc_of(vol) else {
                        refuse(&mut out, f, format!("volume '{vol}' no longer registered"));
                        continue;
                    };
                    if alloc.refcount(*offset).is_some() {
                        refuse(
                            &mut out,
                            f,
                            "offset is allocator-tracked now (healed)".to_string(),
                        );
                        continue;
                    }
                    // Verify the content first (the C7 check for this
                    // block's stored form).
                    let probe = MappingRef {
                        ino: *ino,
                        block_idx: *block_idx,
                        mapping: mapping.clone(),
                        vol: vol.clone(),
                        offset: *offset,
                        damaged: false,
                    };
                    !matches!(
                        verify_stored_block(ctx, &crypto, &probe, block_size).await,
                        ScrubOutcome::Failed(_) | ScrubOutcome::Skipped
                    )
                };
                if repair_allocator {
                    let alloc = alloc_of(vol).expect("checked above");
                    let counted = fresh
                        .as_ref()
                        .and_then(|c| c.refs.get(vol).and_then(|m| m.get(offset)).copied())
                        .unwrap_or(1)
                        .max(1);
                    fire_repair_abort_hook(&what)?;
                    alloc.recover_block(*offset / alloc.chunk_size()).await?;
                    alloc.fsck_set_refcount(*offset, counted);
                    // The content is durable on the device: restore fill
                    // stability under a fresh incarnation generation.
                    alloc.publish_block(*offset);
                    apply_ok(
                        &mut out,
                        f,
                        "repair-allocator",
                        format!(
                            "content verified ⇒ {vol}:{offset} re-marked allocated at \
                             refcount {counted} (the data was fine, the accounting \
                             was wrong)"
                        ),
                    );
                } else {
                    // Quarantine the mapping: copy what is readable,
                    // then flip to the explicit damaged marker.
                    let image = read_stored_image_best_effort(ctx, mapping).await;
                    let (note, parts): (String, Vec<(&str, &[u8])>) = match &image {
                        Some(img) => (
                            "stored image window at quarantine time".to_string(),
                            vec![("block", img.as_ref())],
                        ),
                        None => (
                            "stored image unreadable (out-of-range / unresolvable) — \
                             identity recorded only"
                                .to_string(),
                            Vec::new(),
                        ),
                    };
                    let bytes = quarantine
                        .put(&f.class, &f.object, "quarantine-mapping", &note, &parts)
                        .await?;
                    out.counters.quarantined_records += 1;
                    out.counters.quarantined_bytes += bytes;
                    fire_repair_abort_hook(&what)?;
                    if !flip_mapping_damaged(ctx, *ino, *block_idx, mapping).await? {
                        refuse(
                            &mut out,
                            f,
                            "mapping superseded between verify and flip (foreground \
                             rewrite / mover publish) — nothing to quarantine"
                                .to_string(),
                        );
                        continue;
                    }
                    apply_ok(
                        &mut out,
                        f,
                        "quarantine-mapping",
                        format!(
                            "ino {ino} block {block_idx}: mapping replaced with the \
                             explicit damaged marker (reads EIO; loss made visible, \
                             never a fabricated hole)"
                        ),
                    );
                }
            }
            // ------------------------------------------------------- C3
            FindingId::C3Refcount { vol, offset } => {
                let Some(alloc) = alloc_of(vol) else {
                    refuse(&mut out, f, format!("volume '{vol}' no longer registered"));
                    continue;
                };
                let fresh = fresh.as_ref().expect("census walked");
                // The referencing inos, ascending — the §5.6a lease order.
                let mut referencers: Vec<u64> = fresh
                    .mappings
                    .iter()
                    .filter(|m| &m.vol == vol && m.offset == *offset)
                    .map(|m| m.ino)
                    .collect();
                referencers.sort_unstable();
                referencers.dedup();
                if referencers.is_empty() {
                    refuse(
                        &mut out,
                        f,
                        "no referencers remain (the unreferenced shape is C2's business)"
                            .to_string(),
                    );
                    continue;
                }
                // Take every referencing ino's exclusive lease (ascending)
                // and recount UNDER them.
                let mut guards = Vec::with_capacity(referencers.len());
                if online {
                    for &g_ino in &referencers {
                        let (vol_idx, local) = ctx.meta.route_ino(g_ino);
                        if let Some(kv) = ctx.meta.volumes.get(vol_idx) {
                            guards.push(kv.dlm().lock_inode_exclusive(local).await);
                        }
                    }
                }
                let mut counted = 0u32;
                for &r_ino in &referencers {
                    let (vol_idx, local) = ctx.meta.route_ino(r_ino);
                    let Some(kv) = ctx.meta.volumes.get(vol_idx) else {
                        continue;
                    };
                    let Ok(Some(bytes)) = kv.getxattr(local, "layout").await else {
                        continue;
                    };
                    let layout: Option<crate::routing::LayoutMetadata> = if bytes.starts_with(b"{")
                    {
                        serde_json::from_slice(&bytes).ok()
                    } else {
                        bincode::deserialize(&bytes).ok()
                    };
                    let Some(layout) = layout else { continue };
                    let mut probe = CensusOut {
                        refs: HashMap::new(),
                        mappings: Vec::new(),
                        unresolvable: Vec::new(),
                        inodes_scanned: 0,
                    };
                    census_layout(ctx, r_ino, &layout, block_size, &mut probe).await;
                    counted += probe
                        .refs
                        .get(vol)
                        .and_then(|m| m.get(offset))
                        .copied()
                        .unwrap_or(0);
                }
                let actual = alloc.refcount(*offset);
                match actual {
                    Some(actual) if counted > 0 && actual != counted => {
                        fire_repair_abort_hook(&what)?;
                        alloc.fsck_set_refcount(*offset, counted);
                        drop(guards);
                        apply_ok(
                            &mut out,
                            f,
                            "recount-and-set-refcount",
                            format!(
                                "{vol}:{offset} refcount {actual} → {counted} (recounted \
                                 under {} referencing lease(s))",
                                referencers.len()
                            ),
                        );
                    }
                    Some(actual) if counted > 0 => {
                        drop(guards);
                        refuse(
                            &mut out,
                            f,
                            format!("refcount {actual} already matches the recount (healed)"),
                        );
                    }
                    _ => {
                        drop(guards);
                        refuse(
                            &mut out,
                            f,
                            "offset untracked or unreferenced at recount (C2's business)"
                                .to_string(),
                        );
                    }
                }
            }
            // ------------------------------------------------------- C4
            FindingId::C4Orphan { dir, key, ino } => {
                // Verify: custody still live AND the ino still has no
                // meta (v3 inos are monotonic — never reused — so a
                // missing ino can never come back; the lease is for the
                // read's coherence online).
                let still_present =
                    match crate::cache::nvme::scan_live_staged_custody(dir, CUSTODY_SCAN_MAX).await
                    {
                        Ok(keys) => keys.iter().any(|k| k == key),
                        Err(e) => {
                            refuse(&mut out, f, format!("custody scan failed at verify: {e}"));
                            continue;
                        }
                    };
                if !still_present {
                    refuse(
                        &mut out,
                        f,
                        "custody record no longer present (flushed / already discarded)"
                            .to_string(),
                    );
                    continue;
                }
                let (vol_idx, local) = ctx.meta.route_ino(*ino);
                let still_missing = {
                    let _lease = if online {
                        Some(
                            ctx.meta.volumes[vol_idx]
                                .dlm()
                                .lock_inode_exclusive(local)
                                .await,
                        )
                    } else {
                        None
                    };
                    match ctx.meta.volumes[vol_idx].getattr(local).await {
                        Ok(inode) => inode.nlink == 0,
                        Err(e) if is_not_found(&e) => true,
                        Err(_) => false,
                    }
                };
                if !still_missing {
                    refuse(
                        &mut out,
                        f,
                        "ino has live meta now (not an orphan)".to_string(),
                    );
                    continue;
                }
                // Quarantine the record image(s) — header + key + payload.
                let images =
                    crate::cache::nvme::extract_and_kill_staged_custody(dir, key, false).await?;
                if images.is_empty() {
                    refuse(
                        &mut out,
                        f,
                        "custody record vanished between verify and quarantine".to_string(),
                    );
                    continue;
                }
                let parts: Vec<(&str, &[u8])> =
                    images.iter().map(|img| ("record", img.as_ref())).collect();
                let bytes = quarantine
                    .put(
                        &f.class,
                        &f.object,
                        "quarantine-then-discard-custody",
                        "verbatim staged record image(s): header + custody key + payload",
                        &parts,
                    )
                    .await?;
                out.counters.quarantined_records += images.len() as u64;
                out.counters.quarantined_bytes += bytes;
                fire_repair_abort_hook(&what)?;
                // Discard: live store first (zeroes its indexed record),
                // then the raw on-disk residue (seeded / unindexed images).
                let _ = ctx.router.cache.nvme.remove_active_block(key);
                let _ = crate::cache::nvme::extract_and_kill_staged_custody(dir, key, true).await?;
                apply_ok(
                    &mut out,
                    f,
                    "quarantine-then-discard-custody",
                    format!(
                        "orphan custody '{key}' discarded from {} ({} record image(s) \
                         quarantined first)",
                        dir.display(),
                        images.len()
                    ),
                );
            }
            // ------------------------------------------------------- C5
            FindingId::C5Staging { dir } => {
                let Some(expected) = ctx.expected_generation.as_deref() else {
                    refuse(
                        &mut out,
                        f,
                        "no expected volume-set generation in this context".to_string(),
                    );
                    continue;
                };
                let stale = match crate::cache::nvme::read_staging_generation_marker(dir).await {
                    Ok(Some(found)) => found != expected,
                    Ok(None) => {
                        crate::cache::nvme::dir_has_segment_data(&dir.join("staging_segment"))
                    }
                    Err(_) => true,
                };
                if !stale {
                    refuse(
                        &mut out,
                        f,
                        "staging generation matches the mounted set now (restamped)".to_string(),
                    );
                    continue;
                }
                // Quarantine: copy the marker + every staging segment file
                // aside (move = copy + fsync + remove; the copy is durable
                // BEFORE anything is removed).
                let mut parts_owned: Vec<(String, Vec<u8>)> = Vec::new();
                let marker_path = dir.join(crate::cache::nvme::STAGING_GENERATION_MARKER);
                if let Ok(bytes) = crate::uring_fs::read_all(&marker_path).await {
                    parts_owned.push(("generation_marker".to_string(), bytes.to_vec()));
                }
                let seg_dir = dir.join("staging_segment");
                let mut seg_files: Vec<PathBuf> = Vec::new();
                if let Ok(entries) = std::fs::read_dir(&seg_dir) {
                    for entry in entries.flatten() {
                        if entry.metadata().map(|m| m.is_file()).unwrap_or(false) {
                            seg_files.push(entry.path());
                        }
                    }
                }
                for p in &seg_files {
                    if let Ok(bytes) = crate::uring_fs::read_all(p).await {
                        let name = p
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "segment".to_string());
                        parts_owned.push((format!("segment_{name}"), bytes.to_vec()));
                    }
                }
                let parts: Vec<(&str, &[u8])> = parts_owned
                    .iter()
                    .map(|(n, b)| (n.as_str(), b.as_slice()))
                    .collect();
                let bytes = quarantine
                    .put(
                        &f.class,
                        &f.object,
                        "quarantine-staging-dir",
                        "stale-generation marker + staged segment files, moved aside \
                         (copy retained — the discard-on-mismatch law made non-destructive)",
                        &parts,
                    )
                    .await?;
                out.counters.quarantined_records += parts_owned.len() as u64;
                out.counters.quarantined_bytes += bytes;
                fire_repair_abort_hook(&what)?;
                // The move-aside completes: remove the originals (the
                // copies above are durable).
                let _ = std::fs::remove_file(&marker_path);
                for p in &seg_files {
                    let _ = std::fs::remove_file(p);
                }
                apply_ok(
                    &mut out,
                    f,
                    "quarantine-staging-dir",
                    format!(
                        "{}: stale marker + {} segment file(s) moved aside",
                        dir.display(),
                        seg_files.len()
                    ),
                );
            }
            // ------------------------------------------------------- C6
            FindingId::C6Drift { vol } => {
                let Some(alloc) = alloc_of(vol) else {
                    refuse(&mut out, f, format!("volume '{vol}' no longer registered"));
                    continue;
                };
                let used = alloc
                    .highest_block_index()
                    .saturating_sub(alloc.free_blocks_count());
                let tracked = alloc.tracked_offsets().len() as u64;
                if used == tracked {
                    refuse(
                        &mut out,
                        f,
                        "accounting converged on its own (healed)".to_string(),
                    );
                    continue;
                }
                fire_repair_abort_hook(&what)?;
                let (frees_completed, evictions) = alloc.fsck_reconcile_accounting();
                apply_ok(
                    &mut out,
                    f,
                    "recompute-accounting",
                    format!(
                        "{vol}: derived accounting recomputed from the tracked census \
                         ({frees_completed} wedged free(s) completed, {evictions} \
                         free-list eviction(s))"
                    ),
                );
            }
            // ------------------------------------------------------- C7
            FindingId::C7Scrub {
                ino,
                block_idx,
                mapping,
            } => {
                if !current_mapping_present(ctx, *ino, *block_idx, mapping).await {
                    refuse(
                        &mut out,
                        f,
                        "mapping no longer present (rewritten / truncated / already \
                         quarantined)"
                            .to_string(),
                    );
                    continue;
                }
                let probe = {
                    let clean = clean_key(mapping);
                    let (pvol, poff) = ctx
                        .router
                        .backend_router
                        .parse_block_key(&clean)
                        .ok()
                        .and_then(|(be, off)| canonical_backend(ctx, &be).map(|(v, _)| (v, off)))
                        .unwrap_or_else(|| ("?".to_string(), 0));
                    MappingRef {
                        ino: *ino,
                        block_idx: *block_idx,
                        mapping: mapping.clone(),
                        vol: pvol,
                        offset: poff,
                        damaged: false,
                    }
                };
                let still_failing = if online {
                    reverify_scrub_failure(ctx, &crypto, &probe, block_size).await
                } else {
                    matches!(
                        verify_stored_block(ctx, &crypto, &probe, block_size).await,
                        ScrubOutcome::Failed(_)
                    )
                };
                if !still_failing {
                    refuse(
                        &mut out,
                        f,
                        "block verifies now (moved / rewritten since the scan)".to_string(),
                    );
                    continue;
                }
                // Quarantine what is readable, then isolate the mapping.
                let image = read_stored_image_best_effort(ctx, mapping).await;
                let (note, parts): (String, Vec<(&str, &[u8])>) = match &image {
                    Some(img) => (
                        "corrupt stored image (verbatim device window) — the physical \
                         block also stays in place for forensics"
                            .to_string(),
                        vec![("block", img.as_ref())],
                    ),
                    None => (
                        "stored image unreadable (device read error) — identity \
                         recorded; the physical block stays in place"
                            .to_string(),
                        Vec::new(),
                    ),
                };
                let bytes = quarantine
                    .put(&f.class, &f.object, "quarantine-mapping", &note, &parts)
                    .await?;
                out.counters.quarantined_blocks += 1;
                out.counters.quarantined_bytes += bytes;
                fire_repair_abort_hook(&what)?;
                if !flip_mapping_damaged(ctx, *ino, *block_idx, mapping).await? {
                    refuse(
                        &mut out,
                        f,
                        "mapping superseded between verify and flip (foreground \
                         rewrite / mover publish) — the block moved on"
                            .to_string(),
                    );
                    continue;
                }
                apply_ok(
                    &mut out,
                    f,
                    "quarantine-mapping",
                    format!(
                        "ino {ino} block {block_idx}: mapping quarantined (reads EIO); \
                         physical block preserved for forensics"
                    ),
                );
            }
        }
    }

    out.quarantine_dir = quarantine.run_dir.as_ref().map(|d| d.display().to_string());
    publish_repair_metrics(&out.counters);
    Ok(out)
}

fn publish_repair_metrics(c: &RepairCounters) {
    use crate::fuse_client::METRICS;
    let m = &*METRICS;
    m.fsck_repairs_planned
        .fetch_add(c.planned, Ordering::Relaxed);
    m.fsck_repairs_applied
        .fetch_add(c.applied, Ordering::Relaxed);
    m.fsck_repairs_refused
        .fetch_add(c.refused, Ordering::Relaxed);
    m.fsck_quarantined_records
        .fetch_add(c.quarantined_records, Ordering::Relaxed);
    m.fsck_quarantined_blocks
        .fetch_add(c.quarantined_blocks, Ordering::Relaxed);
    m.fsck_quarantined_bytes
        .fetch_add(c.quarantined_bytes, Ordering::Relaxed);
    // per_class applied counts are published inline at apply time (the
    // per-action `apply_ok` path) — not re-added here.
}

// ---------------------------------------------------------------------------
// Offline harness (read-only probes; §5.8 duality)
// ---------------------------------------------------------------------------

/// The offline CLI harness: refuse under a live writer, open read-only
/// probes, build a probe-shaped router, rebuild the allocator census
/// (full runs only — shards skip the allocator-dependent classes by
/// design), run the engine, release the probes.
pub async fn run_offline(meta_lvs: &[String], opts: &FsckOptions) -> Result<FsckReport> {
    use std::path::Path;
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true)
            .await
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "offline fsck refused: {e} — run `squeezefs fsck <mountpoint>` against \
                     the live mount instead (offline mode requires nothing in flight, §5.6)"
                ))
            })?;
    }
    let routed = crate::meta_backend::open_probe_routed_meta_set(meta_lvs).await?;
    let result = run_offline_body(&routed, meta_lvs, opts, None).await;
    for vol in &routed.volumes {
        if let Err(e) = vol.shutdown().await {
            log::warn!("releasing probe after offline fsck: {e}");
        }
    }
    result
}

/// The offline `--repair` harness (§5.6a / §5.8): repair is a WRITER —
/// it runs under the **D0-guarded open** (the same posture every offline
/// mutating verb takes — `set-cache-paths` / `volume remove-data`),
/// never the read-only probe. Refused on `--shards` probe shards (a
/// shard sees a partial census; repair acts only on whole-scan
/// findings). Detection runs first inside the guard; the repair
/// (dry-run or apply per `ropts`) consumes its verified findings; the
/// combined report is returned with [`FsckReport::repair`] populated.
pub async fn run_offline_repair(
    meta_lvs: &[String],
    opts: &FsckOptions,
    ropts: &RepairOptions,
) -> Result<FsckReport> {
    use std::path::Path;
    if opts.shard.is_some() {
        return Err(SqueezefsError::InvalidOperation(
            "--repair is refused on --shards probe shards: repair requires the whole-scan \
             findings under the guarded open (§5.6a); run the repair unsharded"
                .to_string(),
        ));
    }
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true)
            .await
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "offline fsck --repair refused: {e} — repair requires exclusive \
                     guarded access (§5.6a)"
                ))
            })?;
    }
    // The D0-guarded open (writer claims) — NOT the read-only probe.
    let routed = crate::meta_backend::open_routed_meta_set(meta_lvs).await?;
    let result = run_offline_body(&routed, meta_lvs, opts, Some(ropts)).await;
    for vol in &routed.volumes {
        if let Err(e) = vol.shutdown().await {
            log::warn!("releasing guard after offline fsck --repair: {e}");
        }
    }
    result
}

async fn run_offline_body(
    routed: &Arc<RoutedMetaBackend>,
    meta_lvs: &[String],
    opts: &FsckOptions,
    repair_opts: Option<&RepairOptions>,
) -> Result<FsckReport> {
    let cfg = crate::config_ops::read_volume_format_config(meta_lvs).await?;
    let records = cfg.resolved_data_volumes();
    let dlm = crate::dlm::DlmClient::new("local")?;
    let live: Vec<&crate::DataVolumeRecord> = records
        .iter()
        .filter(|r| r.state != crate::VOL_STATE_RETIRED)
        .collect();
    let first = live.first().ok_or_else(|| {
        SqueezefsError::InvalidOperation("no live data volumes to fsck".to_string())
    })?;
    let first_alloc = Arc::new(
        crate::block_allocator::BlockAllocator::new(dlm.meta_client().clone(), &first.id).await?,
    );
    if let Ok(cap) = crate::nvme_dev::device_capacity_bytes(&first.backing_dev) {
        first_alloc.set_capacity_bytes(cap);
    }
    let first_dev = Arc::new(crate::nvme_dev::NvmeBlockDev::new(&first.backing_dev));
    if cfg.block_size > 0 {
        std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", cfg.block_size.to_string());
    }
    let cache = crate::cache::TieredCache::new(
        Vec::new(), // never adopt/mutate the mount's staging dirs
        Some("64MB"),
        Some("64MB"),
        None,
        None,
        dlm.meta_client().clone(),
        first_alloc.clone(),
        first_dev.clone(),
        None,
    )
    .await?;
    let router = crate::routing::DataRouter::new(dlm.clone(), cache, first_alloc, first_dev);
    router.set_block_size(cfg.block_size);
    for rec in &live {
        router
            .backend_router
            .register_backend(rec, dlm.meta_client().clone())
            .await?;
    }
    router.backend_router.set_volume_records(records.clone());
    router.set_meta_backend(routed.clone());

    // Full runs rebuild the allocator census (the C2/C3 ground truth an
    // unmounted set can offer); shards skip it — their allocator-side
    // classes finalize at merge from the partial censuses.
    if opts.shard.is_none() {
        for kv in &routed.volumes {
            for entry in router.backend_router.backends.iter() {
                entry
                    .value()
                    .block_allocator
                    .recover_active_blocks_v3(kv, &router.backend_router)
                    .await?;
            }
        }
    }

    // The config records the staging ROOTS; a mount isolates its actual
    // staging under `<root>/squeezefs/<sanitized-mountpoint>/` (the
    // per-mount isolation in `main`'s mount path — the generation marker
    // and `staging_segment/` live THERE, not at the root). Offline
    // C4/C5 must scan both shapes: the raw root (legacy/test fixtures,
    // and the quarantine home stays rooted there) plus every isolated
    // per-mount dir found under it (`cache_segment` is the shared read
    // cache — no custody, no marker — and scans inert either way).
    let staging_roots = crate::config_ops::get_cache_paths(meta_lvs)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let mut staging_dirs = Vec::new();
    for root in staging_roots {
        staging_dirs.push(root.clone());
        if let Ok(entries) = std::fs::read_dir(root.join("squeezefs")) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && entry.file_name() != "cache_segment"
                {
                    staging_dirs.push(entry.path());
                }
            }
        }
    }
    let ctx = FsckCtx {
        meta: routed.clone(),
        router,
        staging_dirs,
        expected_generation: Some(volume_generation(routed)),
    };
    let mut report = run(&ctx, opts).await?;
    if let Some(ropts) = repair_opts {
        report.repair = Some(repair(&ctx, &report, ropts).await?);
    }
    Ok(report)
}
