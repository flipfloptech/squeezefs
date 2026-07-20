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
    /// backend.
    C2Lost {
        vol: String,
        offset: u64,
        by: String,
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

async fn walk_trees_c1(
    ctx: &FsckCtx,
    opts: &FsckOptions,
    counters: &mut FsckCounters,
    suspects: &mut Vec<Suspect>,
) {
    use crate::meta_backend::kv::record::{
        decode_dentry_key, decode_inode_key, decode_xattr_key, DentryValue, InodeValue, XattrValue,
        TREE_DENTRIES, TREE_INODES, TREE_XATTRS,
    };
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
                    let why: Option<String> = match tree_id {
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
                    };
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
                let global_ino = ctx.meta.make_global_ino(local_ino, vol_idx);
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
        let clean = clean_key(mapping);
        match ctx.router.backend_router.parse_block_key(&clean) {
            Ok((be_id, offset)) => match canonical_backend(ctx, &be_id) {
                Some((vol, _)) => {
                    *out.refs.entry(vol).or_default().entry(offset).or_insert(0) += 1;
                    out.mappings.push(MappingRef {
                        ino,
                        block_idx: idx,
                        mapping: mapping.to_string(),
                    });
                }
                None => out.unresolvable.push(MappingRef {
                    ino,
                    block_idx: idx,
                    mapping: mapping.to_string(),
                }),
            },
            Err(_) => out.unresolvable.push(MappingRef {
                ino,
                block_idx: idx,
                mapping: mapping.to_string(),
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
fn clean_key(mapping: &str) -> String {
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
        for (&off, _) in refs.iter() {
            let by = census
                .mappings
                .iter()
                .find(|m| {
                    clean_key(&m.mapping)
                        .rsplit("://")
                        .next()
                        .and_then(|s| s.parse::<u64>().ok())
                        == Some(off)
                })
                .map(|m| format!("ino {} block {}", m.ino, m.block_idx))
                .unwrap_or_else(|| "unknown referencer".to_string());
            if capacity != 0 && off >= capacity {
                suspects.push(Suspect {
                    kind: SuspectKind::C2Lost {
                        vol: v.id.clone(),
                        offset: off,
                        by,
                        why: format!("offset past device capacity {capacity}"),
                    },
                });
            } else if off % chunk != 0 {
                suspects.push(Suspect {
                    kind: SuspectKind::C2Lost {
                        vol: v.id.clone(),
                        offset: off,
                        by,
                        why: format!("offset not aligned to the {chunk} B allocator chunk"),
                    },
                });
            } else if !sharded && !tracked.contains_key(&off) {
                suspects.push(Suspect {
                    kind: SuspectKind::C2Lost {
                        vol: v.id.clone(),
                        offset: off,
                        by,
                        why: "referenced offset is not allocator-tracked".to_string(),
                    },
                });
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
                by: format!("ino {} block {}", m.ino, m.block_idx),
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
                })
            }
            SuspectKind::C2Lost {
                vol,
                offset,
                by,
                why,
            } => {
                if vol == "?" {
                    // Unresolvable mapping: permanent by construction.
                    Some(FsckFinding {
                        class: "C2".to_string(),
                        object: by.clone(),
                        evidence: format!("lost block: {why}"),
                    })
                } else {
                    let fresh = fresh.as_ref().expect("fresh walk ran");
                    let refs = fresh.refs.get(vol).unwrap_or(&EMPTY);
                    let still_referenced = refs.contains_key(offset);
                    let now_tracked = alloc_of(vol)
                        .map(|a| a.refcount(*offset).is_some())
                        .unwrap_or(false);
                    let out_of_range = why.contains("capacity") || why.contains("aligned");
                    (still_referenced && (out_of_range || !now_tracked)).then(|| FsckFinding {
                        class: "C2".to_string(),
                        object: format!("{vol}:{offset}"),
                        evidence: format!("lost block ({by}): {why}"),
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
                    }),
                    Ok(None) => None, // record vanished (live delete)
                    Err(e) => Some(FsckFinding {
                        class: "C1".to_string(),
                        object: format!("vol{vol}/tree{tree}/key{}", hex(key)),
                        evidence: format!("record unreadable at re-check: {e}"),
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
    let result = run_offline_body(&routed, meta_lvs, opts).await;
    for vol in &routed.volumes {
        if let Err(e) = vol.shutdown().await {
            log::warn!("releasing probe after offline fsck: {e}");
        }
    }
    result
}

async fn run_offline_body(
    routed: &Arc<RoutedMetaBackend>,
    meta_lvs: &[String],
    opts: &FsckOptions,
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

    let staging_dirs = crate::config_ops::get_cache_paths(meta_lvs)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let ctx = FsckCtx {
        meta: routed.clone(),
        router,
        staging_dirs,
        expected_generation: Some(volume_generation(routed)),
    };
    run(&ctx, opts).await
}
