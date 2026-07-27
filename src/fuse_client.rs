use crate::dlm::DlmClient;
use crate::error::SqueezefsError;
use crate::meta_backend::Metadata;

use crate::routing::DataRouter;
use fuse3::raw::{
    prelude::*,
    reply::{DirectoryEntry, FileAttr, ReplyCopyFileRange, ReplyIoctl},
    Request,
};
use fuse3::{Errno, Inode, MountOptions, Result as FuseResult, Timestamp};
use log::{debug, error, info, warn};
use once_cell::sync::Lazy;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use tokio::runtime::Builder;

pub const CONFIG_INODE: u64 = 0xffff_ffff_ffff_fffe;
pub const STATS_INODE: u64 = 0xffff_ffff_ffff_fffd;

/// §4.5 dir-entry-cache policy (PR K7): only directories with at most
/// this many entries are snapshotted into `dir_entry_cache_v3` — an
/// `Arc<[…]>` of a 1 M-entry listing is ~60 MB, and moka's capacity
/// accounting here counts *directories*, not entries. Larger directories
/// stream through `readdir(dir, offset, max)` instead.
pub const DIR_ENTRY_CACHE_MAX_ENTRIES: usize = 10_000;

/// Readdir cookie of the root's virtual `.config` entry on v3 volumes
/// (design §5.1, PR K7): real-entry cookies occupy
/// `[3, 3 + ((2^54−1)·2^8 + 255)] = [3, 2^62 + 2]`, so the virtual entries
/// ride strictly above the whole real space — still sign-bit-clear for
/// the `i64` FUSE surface. v2 volumes keep positional offsets (virtuals
/// first), untouched.
pub const READDIR_VIRTUAL_CONFIG_COOKIE: u64 = (1 << 62) + 3;

/// Readdir cookie of the root's virtual `.stats` entry on v3 volumes
/// (see [`READDIR_VIRTUAL_CONFIG_COOKIE`]).
pub const READDIR_VIRTUAL_STATS_COOKIE: u64 = (1 << 62) + 4;

/// PR K7: real entries fetched per FUSE `readdir` call on a v3 volume.
/// A kernel dirent buffer holds ~1–2 K entries, so one page bounds the
/// over-fetch waste while big directories stream page by page.
const V3_READDIR_PAGE: usize = 4096;

/// `readdirplus` pages smaller: every entry carries a full attribute
/// fetch (attr-cache-backed inode-tree point lookups).
const V3_READDIRPLUS_PAGE: usize = 1024;

/// How often a mounted client refreshes its `client:{id}` registration on the
/// metadata volume (heartbeat), so peers can distinguish a live mount from a
/// crashed one.
pub const CLIENT_HEARTBEAT_INTERVAL_SECS: u64 = 10;
/// A `client:{id}` registration whose heartbeat is older than this is stale: the
/// client died (kill -9 / crash / power loss) without unregistering. Stale
/// registrations do NOT block `format` and are reaped on next format attempt.
pub const CLIENT_STALE_TTL_SECS: u64 = 45;

fn get_fuse_timeout() -> Duration {
    // Launch-time knob, memoized (item A): `std::env::var` takes the
    // process-global env lock and allocates — measured at ~0.7% of daemon
    // cycles on the warm rand-4k transport row, called per FUSE op. Pinned
    // by `fuse_timeout_is_memoized_not_per_op_env_read`.
    static FUSE_TIMEOUT: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *FUSE_TIMEOUT.get_or_init(|| {
        if let Ok(val) = std::env::var("SQUEEZEFS_TIMEOUT") {
            if let Ok(secs) = val.parse::<u64>() {
                return Duration::from_secs(secs);
            }
        }
        Duration::from_secs(30)
    })
}

// `StripeLocks` moved to `crate::stripe_locks` so `meta_backend` can use it
// without a module cycle. Re-exported here for source compatibility (existing
// `crate::fuse_client::StripeLocks` / `squeezefs::fuse_client::StripeLocks` paths
// and the lock-order documentation continue to work).
pub use crate::stripe_locks::StripeLocks;

/// Per-class kernel cache TTLs (design-metadata-throughput §5.2 D2.b/D2.c
/// + the reference-client survey P1-C rider: DAOS ships the per-class
/// split as container attributes, JuiceFS as mount flags — SqueezeFS
/// previously hardcoded 1 s everywhere and silently DROPPED user-passed
/// `entry_timeout`/`attr_timeout`/`negative_timeout` mount options).
///
/// Four classes, all defaulting to the historical 1 s:
/// - `attr`: GETATTR/SETATTR reply TTL + the daemon attr-cache freshness
///   window (`get_attr_internal`).
/// - `entry`: dentry TTL for non-directory lookup/create/link results.
/// - `dir_entry`: dentry TTL for directory results (dir dentries
///   invalidate whole subtrees — the DAOS `dfuse-dentry-dir-time` split).
/// - `negative`: TTL on cacheable negative lookup replies (D2.b);
///   `0` disables negative caching (miss replies stay bare ENOENT).
///
/// Sources, later wins: defaults → `SQUEEZEFS_FUSE_{ATTR,ENTRY,DIR_ENTRY,
/// NEGATIVE}_TTL_MS` env (launch-time, read once at construction — the
/// `get_fuse_timeout` convention) → `-o attr_timeout=/entry_timeout=/
/// dir_entry_timeout=/negative_timeout=` mount options (libfuse-style
/// float seconds). The mount options are daemon-level: they are still
/// stripped from the kernel `mount(2)` option string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelCacheTtls {
    pub attr: Duration,
    pub entry: Duration,
    pub dir_entry: Duration,
    pub negative: Duration,
}

impl Default for KernelCacheTtls {
    fn default() -> Self {
        Self {
            attr: Duration::from_secs(1),
            entry: Duration::from_secs(1),
            dir_entry: Duration::from_secs(1),
            negative: Duration::from_secs(1),
        }
    }
}

impl KernelCacheTtls {
    /// Launch-time env knobs (milliseconds). Read once at filesystem
    /// construction — never on the per-op path.
    pub fn from_env() -> Self {
        fn env_ms(key: &str, default: Duration) -> Duration {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_millis)
                .unwrap_or(default)
        }
        let d = Self::default();
        Self {
            attr: env_ms("SQUEEZEFS_FUSE_ATTR_TTL_MS", d.attr),
            entry: env_ms("SQUEEZEFS_FUSE_ENTRY_TTL_MS", d.entry),
            dir_entry: env_ms("SQUEEZEFS_FUSE_DIR_ENTRY_TTL_MS", d.dir_entry),
            negative: env_ms("SQUEEZEFS_FUSE_NEGATIVE_TTL_MS", d.negative),
        }
    }

    /// Apply `-o` mount-option overrides (libfuse-style float seconds:
    /// `attr_timeout=2.5,entry_timeout=1,dir_entry_timeout=10,
    /// negative_timeout=0`). Unknown keys and unparseable values are
    /// ignored (the kernel-option filter owns rejection of stray keys).
    pub fn with_mount_options(mut self, opts: &str) -> Self {
        for opt in opts.split(',') {
            let opt = opt.trim();
            let Some((key, val)) = opt.split_once('=') else {
                continue;
            };
            let Ok(secs) = val.trim().parse::<f64>() else {
                continue;
            };
            if !secs.is_finite() || secs < 0.0 {
                continue;
            }
            let ttl = Duration::from_secs_f64(secs);
            match key.trim() {
                "attr_timeout" => self.attr = ttl,
                "entry_timeout" => self.entry = ttl,
                "dir_entry_timeout" => self.dir_entry = ttl,
                "negative_timeout" => self.negative = ttl,
                _ => {}
            }
        }
        self
    }
}

// ===========================================================================
// D1.b AWAIT-DISPOSITION AUDIT (design-metadata-throughput §5.1, PR M4)
//
// PR M4 retires the per-op `tokio::time::timeout(get_fuse_timeout(), …)`
// wrappers. Their replacement — the deadline watchdog below — only LOGS;
// it never cancels. So every handler-reachable await that can block
// indefinitely on device/ring/network needs an explicit disposition, and
// the design mandates that the exception list be DERIVED FROM AN AUDIT,
// not named ad hoc. This block is that audit, verified against dev @
// 3c9eda4 (line anchors current at that tip).
//
// Why removal is a correctness fix, not just a perf cut: `timeout()`
// expiry DROPS the handler's future at whatever await it is parked on.
// Reachable through every mutation handler sits `commit_tx`
// (`src/meta_backend/kv/backend.rs:1921+`), whose journal accounting is
// deliberately unforgiving: an `Admission` dropped between ring admission
// and `reserve_registered` leaks ring budget forever
// (`journal_core.rs:197-204` — `#[must_use]`, no Drop recovery), and a
// registered `Reservation` whose future dies before `complete()`
// permanently stalls the `completed_upto` watermark
// (`journal.rs:398-420`), wedging every later committer's
// `wait_completed_upto`. The 30 s per-op timeout was therefore a live
// volume-wedge vector on any commit that crossed the threshold. After M4
// NO per-op `timeout()` future-drop can hit `commit_tx` — the residual
// drop vectors (unmount/session teardown, panics) are owned by PR M7's
// detached-pass shield (§5.5 lifecycle; the M4 → M7 load-bearing edge).
//
// Semantics change, stated loudly: ops no longer synthesize `ETIMEDOUT`
// at `SQUEEZEFS_TIMEOUT`. The watchdog task scans the op registry every
// WATCHDOG_TICK and logs (ERROR, with op detail + age) every op older
// than the threshold, counting `fuse_op_watchdog_overdue`. The wedge
// classes the old timeout papered over were fixed structurally in the
// unmount/sideband work (2026-07-08-unmount-stuck-request-rootcause);
// hang *diagnosis* is preserved — louder and structured.
//
// The audit table. "Watchdog-only" = the op stays parked (correct for a
// genuinely wedged device — an op error cannot fix it) and the watchdog
// reports it; "bounded" = a synthesized error stays load-bearing.
//
// | # | Await class (anchors @ 3c9eda4)              | Disposition |
// |---|-----------------------------------------------|-------------|
// | 1 | FLUSH/FSYNC-class barrier waits: `fsync` (fuse_client `flush_inode_to_backend` → `sync_device_for_ino`, meta_backend/mod.rs:1177) and `fsyncdir` (`sync_all_devices`, mod.rs:1167) funnel into `KvMetaBackend::sync_device` (backend.rs:984-1000), the per-volume `SyncCoalescer::barrier` (sync_coalescer.rs:62-112) and its leader's `uring_fs::fdatasync`. The strict-mode `commit_tx` step-7 barrier and the checkpoint tick barriers (checkpoint.rs:402) share the same funnel. At this tip NOTHING bounds the wait (the fsync/fsyncdir handlers were never timeout-wrapped; the coalescer waits are unbounded). | **bounded wait + error** — M4 adds the bound INSIDE the coalescer (`SyncCoalescer::barrier_bounded`): the one drop-safe layer. Bounding from outside (a `timeout()` around `sync_device`) would drop an inline LEADER mid-`sync_fn`, stranding `flushing = true` and every queued follower — the exact wedge shape this PR exists to remove. A synthesized `ETIMEDOUT`-class error stays load-bearing for userspace fsync liveness on a sick device; escalation truth (barrier_failures rungs) stays with REAL barrier outcomes, never the bound. |
// | 2 | Ring-admission parking: `commit_tx` step 2 (backend.rs:1952-1975) — the register-recheck-await loop on `space_notified()` (journal.rs:472-474). Shutdown/failure flags are re-checked per park, but a wedged-not-failed drain (checkpoint task alive, `reusable_upto` frozen — e.g. admitted-but-never-reserved budget, a stuck device write in the flush path) never notifies: a parked committer waits forever and, pre-M4, the op timeout dropped its future while the volume kept wedging. | **watchdog + escalation** — each park is time-bounded so the liveness re-check can never be starved by a silent drain; cumulative parked time ≥ `SQUEEZEFS_TIMEOUT` logs loud and trips `note_journal_failure()` (backend.rs:1883) once per threshold crossing. Repeated crossings latch `failed` (JOURNAL_FAILURE_LATCH = 3 → ~3× threshold for a solo committer, one threshold under real op concurrency), every mutation then returns EIO and the routed layer mirrors the volume into `disabled_volumes` (mod.rs:210) — strictly more actionable than the old ETIMEDOUT-while-the-volume-keeps-wedging. Self-arbitrating: any OTHER committer's entry-write success resets `journal_failures` (backend.rs:2098), so a merely starved-but-alive volume logs loud without fail-stopping. |
// | 3 | `wait_completed_upto` (journal.rs:443-453): commit_tx step 7's predecessor-completion wait. A stall here means an uncompleted registered reservation — a commit-pipeline bug, not an op-recoverable condition. | **watchdog-only** — the overdue log names the op; post-M7 this wait is conveyor-leader-internal and additionally surfaced by `meta_commit_group_*`. |
// | 4 | Demand-paged node reads: every backend lookup/commit may fault KV nodes through the node cache's demand paging into `uring_fs` device reads (process-worker owned, no timeout). | **watchdog-only** — device ERRORS already propagate as op errors; only true device hangs remain, and synthesizing an op error cannot fix those (the volume needs operator action; the watchdog log is the signal). |
// | 5 | DLM stripe locks + in-process serialization: `DlmLockManager` I/D stripes (meta_backend/dlm.rs — RAM tokio RwLocks, canonical acquisition order), `active_inode_locks`/`lease_locks`/`INODE_META_LOCKS`/`BLOCK_FLUSH_LOCKS` (P1-9 order). Cluster lease acquisition (`get_or_acquire_lease`, fuse_client.rs:2065) is already bounded (`acquire_lock(…, 5 s)`) and measured ZERO on metadata storms (baseline lever 9 demotion). No unbounded network awaits exist on metadata paths. | **watchdog-only** — deadlock-freedom is the lock-order contract's job; a violation is a bug the watchdog now makes visible in production. |
// | 6 | Data-path device I/O: `NvmeBlockDev` read/write (bounded io_uring worker queues + backpressure), staging mmap ops, `read_file_range_zero_copy`, active-block flushes. The never-lossy writeback ladder stays retry-forever BY DESIGN (AGENTS.md). | **watchdog-only** — the old `max(SQUEEZEFS_TIMEOUT, 30 s)` read/write timeouts synthesized ETIMEDOUT while leaving the device wedged; reads/writes now park visibly. |
//
// Bounded waits that already exist and are KEPT (not per-op timeouts):
// `destroy()`'s staged-drain wait (bounded by `dismount_wait`,
// fuse_client.rs:4243-4261), the 2 s device health probe (routing.rs:898), the
// DLM lease acquire's 5 s bound, and the reclaim gather window
// (`drain_reclaim_batch`'s `timeout_at`, fuse_client.rs:8018+ — a batch
// WINDOW, not an op wait). POSIX byte-range locks are KERNEL-LOCAL
// (FUSE_POSIX_LOCKS is never advertised — fstests 131/478/504), so no
// lock wait ever reaches this daemon.
// ===========================================================================

// ===========================================================================
// D1.a per-op attribution rig (design-metadata-throughput §5.1, PR M2)
//
// Off by default; `SQUEEZEFS_OP_PROFILE=1` (launch-time, memoized like
// `SQUEEZEFS_TIMEOUT`) turns on per-op-type phase histograms — monotonic
// stamps at handler entry → backend entry → backend (commit_tx) return →
// reply enqueued (= handler return; fuse3 enqueues the reply immediately
// after) — plus the two first-class artifacts the program's cost model
// rests on:
//
//  1. the **under-`i_rwsem` span estimator**: each create's preceding
//     LOOKUP is paired by `(parent, name)` through a bounded latch-free
//     table, and LOOKUP-arrival → CREATE-reply lands in
//     `fuse_create_under_lock_ns` — the direct measurement §4's cost model
//     and the R1 arithmetic are re-derived from;
//  2. the **watchdog-ready op registry**: a fixed lock-free slab holding
//     `(op, ino, start)` per in-flight profiled op. M2 lands the slot
//     claim/release plumbing (exercised by this rig and surfaced as
//     `fuse_op_profile_inflight`); D1.b (PR M4) reuses it as the deadline
//     watchdog's scan surface when per-op `timeout()` wrapping dies.
//
// Cost contract (hard M2 acceptance): with the variable unset the rig is
// ZERO work on every path — `OpProf::begin` is one memoized atomic load +
// branch returning `None`; no `Instant` reads, no allocation, no slots.
// When enabled, each op pays its stamps (four `Instant` reads) and one
// slot claim/release — profile mode is explicitly allowed to cost.
// ===========================================================================

/// Launch-time gate for the D1.a rig, memoized (the `get_fuse_timeout`
/// pattern): `std::env::var` takes the process-global env lock and
/// allocates — never on the per-op path. Pinned by
/// `op_profile_gate_is_memoized_and_default_off`.
pub fn op_profile_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SQUEEZEFS_OP_PROFILE").is_ok_and(|v| v == "1"))
}

/// `SQUEEZEFS_PATCH_MAX_BYTES` cell (design-random-small-writes §6): max
/// length of a W1 sole-owner in-place patch. Default **512 KiB**; `0`
/// disables the patch path — the A/B lever for acceptance runs (and the
/// pin lever for tests that document the patch-INELIGIBLE accumulation
/// pipeline), not an operational escape hatch. Env is read once
/// (memoized — never on the per-op path); the atomic cell keeps the
/// A/B flip runtime-settable via [`set_patch_max_bytes`].
fn patch_max_bytes_cell() -> &'static AtomicU64 {
    static CELL: std::sync::OnceLock<AtomicU64> = std::sync::OnceLock::new();
    CELL.get_or_init(|| {
        let v = std::env::var("SQUEEZEFS_PATCH_MAX_BYTES")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(512 * 1024);
        AtomicU64::new(v)
    })
}

/// Current W1 patch length cap in bytes (`0` = patch path disabled).
pub fn patch_max_bytes() -> u64 {
    patch_max_bytes_cell().load(Ordering::Relaxed)
}

/// Set the W1 patch length cap (the §6 A/B lever; tests/acceptance).
pub fn set_patch_max_bytes(v: u64) {
    patch_max_bytes_cell().store(v, Ordering::Relaxed);
}

/// W2 fold trigger — extent COUNT threshold (`SQUEEZEFS_FOLD_MAX_EXTENTS`,
/// design §6): an extent overlay reaching this many parked runs enqueues a
/// background fold. Env read once; runtime-settable for tests/acceptance.
fn fold_max_extents_cell() -> &'static AtomicU64 {
    static CELL: std::sync::OnceLock<AtomicU64> = std::sync::OnceLock::new();
    CELL.get_or_init(|| {
        let v = std::env::var("SQUEEZEFS_FOLD_MAX_EXTENTS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(64);
        AtomicU64::new(v)
    })
}

/// Current W2 fold extent-count trigger (0 = threshold folds disabled).
pub fn fold_max_extents() -> u64 {
    fold_max_extents_cell().load(Ordering::Relaxed)
}

/// Set the W2 fold extent-count trigger (tests/acceptance).
pub fn set_fold_max_extents(v: u64) {
    fold_max_extents_cell().store(v, Ordering::Relaxed);
}

/// W2 fold trigger — parked payload BYTE threshold per block
/// (`SQUEEZEFS_FOLD_MAX_BYTES`, design §6). Default 1 MiB.
fn fold_max_bytes_cell() -> &'static AtomicU64 {
    static CELL: std::sync::OnceLock<AtomicU64> = std::sync::OnceLock::new();
    CELL.get_or_init(|| {
        let v = std::env::var("SQUEEZEFS_FOLD_MAX_BYTES")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(1024 * 1024);
        AtomicU64::new(v)
    })
}

/// Current W2 fold byte trigger (0 = threshold folds disabled).
pub fn fold_max_bytes() -> u64 {
    fold_max_bytes_cell().load(Ordering::Relaxed)
}

/// Set the W2 fold byte trigger (tests/acceptance).
pub fn set_fold_max_bytes(v: u64) {
    fold_max_bytes_cell().store(v, Ordering::Relaxed);
}

/// W2 parked-write budget, in BUFFERS' WORTH of bytes (× block size): the
/// retired 256-COUNT cap's byte form (§5.2 — an extent overlay charges its
/// payload bytes, not a whole buffer). Runtime-settable test/acceptance
/// seam; production default = the historical 256.
fn parked_cap_buffers_cell() -> &'static AtomicU64 {
    static CELL: std::sync::OnceLock<AtomicU64> = std::sync::OnceLock::new();
    CELL.get_or_init(|| AtomicU64::new(MAX_ACTIVE_BLOCK_BUFFERS as u64))
}

/// Current parked-write budget in buffers' worth (see
/// [`set_parked_cap_buffers`]).
pub fn parked_cap_buffers() -> u64 {
    parked_cap_buffers_cell().load(Ordering::Relaxed)
}

/// Set the parked-write budget in buffers' worth (tests/acceptance).
pub fn set_parked_cap_buffers(v: u64) {
    parked_cap_buffers_cell().store(v, Ordering::Relaxed);
}

/// Parse an `active_block:`/`active_block_ext:` ino-form key into its
/// `(ino, block)` identity (PR VL7 — the D3 fold-target enumeration).
/// Path-form keys (`active_block:{file_path}:block_{b}` with a non-`inode_`
/// path) return `None`: they never name striped fold custody.
fn parse_block_family_key(key: &str) -> Option<(u64, u32)> {
    let rest = key
        .strip_prefix("active_block_ext:")
        .or_else(|| key.strip_prefix("active_block:"))?;
    let rest = rest.strip_prefix("inode_")?;
    let (ino, block) = rest.split_once(":block_")?;
    Some((ino.parse().ok()?, block.parse().ok()?))
}

/// Op types the registry attributes — the mdstorm-visible request mix
/// plus every op class that lost its per-op `timeout()` wrapper to the
/// D1.b watchdog (read/write/readdir/link-family/fsync can all park on
/// device/ring waits and must be scan-visible). `repr(usize)` indexes the
/// phase-histogram table directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum FuseOpKind {
    Lookup = 0,
    Getattr = 1,
    Setattr = 2,
    Mknod = 3,
    Mkdir = 4,
    Create = 5,
    Unlink = 6,
    Rmdir = 7,
    Rename = 8,
    Flush = 9,
    Release = 10,
    Forget = 11,
    Read = 12,
    Write = 13,
    Readdir = 14,
    Readdirplus = 15,
    Symlink = 16,
    Link = 17,
    Fsync = 18,
    // VL8 item 2 (the 013/464 wedge live capture, 2026-07-21): these op
    // classes sat permanently in flight while the watchdog was BLIND to
    // them — every diagnosis started from the visible victims instead of
    // the holders. They register like every other op.
    CopyFileRange = 19,
    Fallocate = 20,
    Open = 21,
}

const FUSE_OP_KINDS: usize = 22;

impl FuseOpKind {
    const ALL: [FuseOpKind; FUSE_OP_KINDS] = [
        FuseOpKind::Lookup,
        FuseOpKind::Getattr,
        FuseOpKind::Setattr,
        FuseOpKind::Mknod,
        FuseOpKind::Mkdir,
        FuseOpKind::Create,
        FuseOpKind::Unlink,
        FuseOpKind::Rmdir,
        FuseOpKind::Rename,
        FuseOpKind::Flush,
        FuseOpKind::Release,
        FuseOpKind::Forget,
        FuseOpKind::Read,
        FuseOpKind::Write,
        FuseOpKind::Readdir,
        FuseOpKind::Readdirplus,
        FuseOpKind::Symlink,
        FuseOpKind::Link,
        FuseOpKind::Fsync,
        FuseOpKind::CopyFileRange,
        FuseOpKind::Fallocate,
        FuseOpKind::Open,
    ];

    fn name(self) -> &'static str {
        match self {
            FuseOpKind::Lookup => "lookup",
            FuseOpKind::Getattr => "getattr",
            FuseOpKind::Setattr => "setattr",
            FuseOpKind::Mknod => "mknod",
            FuseOpKind::Mkdir => "mkdir",
            FuseOpKind::Create => "create",
            FuseOpKind::Unlink => "unlink",
            FuseOpKind::Rmdir => "rmdir",
            FuseOpKind::Rename => "rename",
            FuseOpKind::Flush => "flush",
            FuseOpKind::Release => "release",
            FuseOpKind::Forget => "forget",
            FuseOpKind::Read => "read",
            FuseOpKind::Write => "write",
            FuseOpKind::Readdir => "readdir",
            FuseOpKind::Readdirplus => "readdirplus",
            FuseOpKind::Symlink => "symlink",
            FuseOpKind::Link => "link",
            FuseOpKind::Fsync => "fsync",
            FuseOpKind::CopyFileRange => "copy_file_range",
            FuseOpKind::Fallocate => "fallocate",
            FuseOpKind::Open => "open",
        }
    }
}

/// Phase split of one op's wall (design §5.1 stamp points): handler entry
/// → backend entry → backend return (`commit_tx` return for mutations) →
/// reply enqueued. Reads use the same marks around their backend fetch.
const OP_PHASES: usize = 4;
const OP_PHASE_NAMES: [&str; OP_PHASES] = [
    "handler_to_backend", // pre-backend handler glue (arg parse, caches)
    "backend",            // backend entry → return (engine + commit)
    "backend_to_reply",   // post-backend glue → reply enqueued
    "total",              // handler entry → reply enqueued
];

/// Watchdog-ready op registry slot count. Sized for the daemon's realistic
/// in-flight op ceiling (over-uring queues × depth is far below this);
/// claim degrades to unregistered-but-profiled when full — never blocks.
const OP_REGISTRY_SLOTS: usize = 256;

struct OpSlot {
    /// 0 = free, 1 = claimed. CAS-claimed, store-released.
    state: std::sync::atomic::AtomicU32,
    /// `FuseOpKind as u32` — the watchdog's op detail.
    kind: std::sync::atomic::AtomicU32,
    /// Primary ino argument (parent for name-ops) — the watchdog's target.
    ino: AtomicU64,
    /// Op start, ns since [`prof_epoch`] — the watchdog's overdue test.
    start_ns: AtomicU64,
}

struct OpRegistry {
    slots: [OpSlot; OP_REGISTRY_SLOTS],
    /// Round-robin claim cursor: keeps claim O(1) amortized instead of
    /// rescanning slot 0 under storm.
    cursor: std::sync::atomic::AtomicUsize,
}

impl OpRegistry {
    fn claim(&self, kind: FuseOpKind, ino: u64, start_ns: u64) -> Option<usize> {
        let base = self.cursor.fetch_add(1, Ordering::Relaxed);
        for probe in 0..OP_REGISTRY_SLOTS {
            let idx = (base + probe) % OP_REGISTRY_SLOTS;
            let slot = &self.slots[idx];
            if slot
                .state
                .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                slot.kind.store(kind as u32, Ordering::Relaxed);
                slot.ino.store(ino, Ordering::Relaxed);
                slot.start_ns.store(start_ns, Ordering::Relaxed);
                return Some(idx);
            }
        }
        None // slab full: op still profiles, just unregistered
    }

    fn release(&self, idx: usize) {
        self.slots[idx].state.store(0, Ordering::Release);
    }

    fn active(&self) -> u64 {
        self.slots
            .iter()
            .filter(|s| s.state.load(Ordering::Relaxed) == 1)
            .count() as u64
    }
}

/// Bounded latch-free recent-LOOKUPs table for the under-`i_rwsem`
/// estimator: open-addressed single-probe slots keyed by
/// `xxh3(parent ‖ name) | 1` (0 = empty). A colliding arrival overwrites
/// the older one (fixed capacity, evict-on-collision — the pairing
/// distance in a create storm is one op, so 1024 slots is generous), and
/// `take` consumes the slot so one lookup arms at most one pairing.
/// Estimator-grade by design: a torn racing overwrite loses or skews one
/// SAMPLE, never memory safety or a wrong op's reply.
const LOOKUP_PAIR_SLOTS: usize = 1024;

struct LookupPairTable {
    keys: [AtomicU64; LOOKUP_PAIR_SLOTS],
    stamps: [AtomicU64; LOOKUP_PAIR_SLOTS],
}

impl LookupPairTable {
    fn hash(parent: u64, name: &str) -> u64 {
        let mut buf = Vec::with_capacity(8 + name.len());
        buf.extend_from_slice(&parent.to_le_bytes());
        buf.extend_from_slice(name.as_bytes());
        xxhash_rust::xxh3::xxh3_64(&buf) | 1
    }

    fn note(&self, parent: u64, name: &str, arrival_ns: u64) {
        let h = Self::hash(parent, name);
        let idx = (h as usize) % LOOKUP_PAIR_SLOTS;
        self.stamps[idx].store(arrival_ns, Ordering::Relaxed);
        self.keys[idx].store(h, Ordering::Release);
    }

    fn take(&self, parent: u64, name: &str) -> Option<u64> {
        let h = Self::hash(parent, name);
        let idx = (h as usize) % LOOKUP_PAIR_SLOTS;
        if self.keys[idx]
            .compare_exchange(h, 0, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(self.stamps[idx].load(Ordering::Relaxed))
        } else {
            None
        }
    }
}

struct OpProfileState {
    /// `[op][phase]` latency histograms — the §9 `fuse_op_phase_ns` family
    /// (recorded from ns `Duration`s into the repo's standard µs-bucket
    /// [`LatencyHistogram`]).
    phases: [[LatencyHistogram; OP_PHASES]; FUSE_OP_KINDS],
    /// LOOKUP-arrival → CREATE-reply spans — `fuse_create_under_lock_ns`,
    /// the §4 cost model's direct measurement.
    create_under_lock: LatencyHistogram,
    registry: OpRegistry,
    recent_lookups: LookupPairTable,
}

static OP_PROFILE: Lazy<OpProfileState> = Lazy::new(|| OpProfileState {
    phases: std::array::from_fn(|_| std::array::from_fn(|_| LatencyHistogram::default())),
    create_under_lock: LatencyHistogram::default(),
    registry: OpRegistry {
        slots: std::array::from_fn(|_| OpSlot {
            state: std::sync::atomic::AtomicU32::new(0),
            kind: std::sync::atomic::AtomicU32::new(0),
            ino: AtomicU64::new(0),
            start_ns: AtomicU64::new(0),
        }),
        cursor: std::sync::atomic::AtomicUsize::new(0),
    },
    recent_lookups: LookupPairTable {
        keys: std::array::from_fn(|_| AtomicU64::new(0)),
        stamps: std::array::from_fn(|_| AtomicU64::new(0)),
    },
});

/// Monotonic ns since the rig's first use (one process-wide `Instant`
/// epoch — stamps are u64s so they live in atomics).
fn prof_now_ns() -> u64 {
    static PROF_EPOCH: Lazy<std::time::Instant> = Lazy::new(std::time::Instant::now);
    PROF_EPOCH.elapsed().as_nanos() as u64
}

/// The watchdog's coarse clock: epoch-ns refreshed by the watchdog task
/// once per tick (D1.b "start: coarse Instant"). Op registration reads it
/// with ONE relaxed load — zero clock syscalls on the per-op path, which
/// is the whole point of retiring `timeout()`'s timer-wheel + vdso tax
/// (2.75 % of daemon cycles measured). Staleness ≤ one watchdog tick
/// (5 s), so overdue ages are overstated by at most one tick against a
/// 30 s-class threshold — diagnostic-grade by design.
static COARSE_NOW_NS: AtomicU64 = AtomicU64::new(0);

/// The profile-mode stamps (four precise `Instant` reads per op) — only
/// allocated inside [`OpProf`] when `SQUEEZEFS_OP_PROFILE=1`; the
/// disabled path pays no clock reads at all (the M2 cost contract,
/// carried forward under the always-on watchdog registration).
struct OpProfStamps {
    t0_ns: u64,
    backend_start_ns: AtomicU64,
    backend_done_ns: AtomicU64,
}

/// One in-flight FUSE op: created at handler entry, `Drop` at handler
/// return.
///
/// Two duties since PR M4 (D1.b):
/// - **Watchdog registration (always on)**: claims a registry slot with
///   `(kind, ino, coarse start)` — the scan surface that replaced the
///   per-op `timeout()` wrappers. Cost: one CAS + three relaxed stores at
///   entry, one release store at exit, no clock reads.
/// - **Phase profiling (gated)**: when `SQUEEZEFS_OP_PROFILE=1`, precise
///   stamps at the backend boundary feed the D1.a `fuse_op_phase_ns`
///   histograms; drop records all four phases (drop-based so error exits
///   record truthfully).
pub struct OpProf {
    kind: FuseOpKind,
    slot: Option<usize>,
    stamps: Option<OpProfStamps>,
}

impl OpProf {
    /// The handler entry point: always registers the op with the
    /// watchdog registry; adds profile stamps only under
    /// `SQUEEZEFS_OP_PROFILE=1`.
    #[inline]
    pub fn begin(kind: FuseOpKind, ino: u64) -> OpProf {
        if op_profile_enabled() {
            Self::begin_forced(kind, ino)
        } else {
            OpProf {
                kind,
                slot: OP_PROFILE
                    .registry
                    .claim(kind, ino, COARSE_NOW_NS.load(Ordering::Relaxed)),
                stamps: None,
            }
        }
    }

    /// Profile-armed constructor: the tests' seam (the memoized gate is
    /// process-wide and default-off under `cargo test`, so rig behavior
    /// is exercised explicitly) and the `begin` fast path's slow arm.
    pub fn begin_forced(kind: FuseOpKind, ino: u64) -> OpProf {
        let t0_ns = prof_now_ns();
        OpProf {
            kind,
            slot: OP_PROFILE.registry.claim(kind, ino, t0_ns),
            stamps: Some(OpProfStamps {
                t0_ns,
                backend_start_ns: AtomicU64::new(0),
                backend_done_ns: AtomicU64::new(0),
            }),
        }
    }

    /// Stamp the backend entry (first backend/router touch). No-op when
    /// the rig is disabled (no clock read).
    #[inline]
    pub fn mark_backend_start(&self) {
        if let Some(s) = &self.stamps {
            s.backend_start_ns.store(prof_now_ns(), Ordering::Relaxed);
        }
    }

    /// Stamp the backend return (`commit_tx` return for mutations; fetch
    /// return for reads). No-op when the rig is disabled.
    #[inline]
    pub fn mark_backend_done(&self) {
        if let Some(s) = &self.stamps {
            s.backend_done_ns.store(prof_now_ns(), Ordering::Relaxed);
        }
    }

    /// Record this LOOKUP's arrival for the under-`i_rwsem` estimator
    /// (called at lookup handler exit with the op's entry stamp — the
    /// kernel holds the parent's `i_rwsem` from before LOOKUP dispatch
    /// through CREATE completion, so arrival is the honest span start).
    /// Profile-mode only.
    pub fn note_lookup_arrival(&self, parent: u64, name: &str) {
        if let Some(s) = &self.stamps {
            OP_PROFILE.recent_lookups.note(parent, name, s.t0_ns);
        }
    }

    /// Pair this CREATE's reply with its preceding LOOKUP and record the
    /// LOOKUP-arrival → CREATE-reply span (`fuse_create_under_lock_ns`).
    /// Profile-mode only.
    pub fn pair_create_reply(&self, parent: u64, name: &str) {
        if self.stamps.is_none() {
            return;
        }
        if let Some(arrival_ns) = OP_PROFILE.recent_lookups.take(parent, name) {
            let span = prof_now_ns().saturating_sub(arrival_ns);
            OP_PROFILE
                .create_under_lock
                .record(Duration::from_nanos(span));
        }
    }
}

impl Drop for OpProf {
    fn drop(&mut self) {
        if let Some(s) = &self.stamps {
            let now = prof_now_ns();
            let hists = &OP_PROFILE.phases[self.kind as usize];
            let bs = s.backend_start_ns.load(Ordering::Relaxed);
            let bd = s.backend_done_ns.load(Ordering::Relaxed);
            if bs > 0 {
                hists[0].record(Duration::from_nanos(bs.saturating_sub(s.t0_ns)));
                if bd >= bs {
                    hists[1].record(Duration::from_nanos(bd - bs));
                }
            }
            let reply_from = if bd > 0 {
                bd
            } else if bs > 0 {
                bs
            } else {
                s.t0_ns
            };
            hists[2].record(Duration::from_nanos(now.saturating_sub(reply_from)));
            hists[3].record(Duration::from_nanos(now.saturating_sub(s.t0_ns)));
        }
        if let Some(idx) = self.slot {
            OP_PROFILE.registry.release(idx);
        }
    }
}

/// `fuse_op_phase_ns` stats payload: `{op: {phase: histogram}}`.
pub fn op_profile_phase_json() -> serde_json::Value {
    let mut ops = serde_json::Map::new();
    for kind in FuseOpKind::ALL {
        let mut phases = serde_json::Map::new();
        for (pi, pname) in OP_PHASE_NAMES.iter().enumerate() {
            phases.insert(
                (*pname).to_string(),
                OP_PROFILE.phases[kind as usize][pi].to_json(),
            );
        }
        ops.insert(kind.name().to_string(), serde_json::Value::Object(phases));
    }
    serde_json::Value::Object(ops)
}

/// `fuse_create_under_lock_ns` stats payload.
pub fn op_profile_under_lock_json() -> serde_json::Value {
    OP_PROFILE.create_under_lock.to_json()
}

/// Live profiled-op count from the registry (`fuse_op_profile_inflight`)
/// — the D1.b watchdog's future scan surface, kept honest now by the
/// claim/release lifecycle tests.
pub fn op_profile_inflight() -> u64 {
    OP_PROFILE.registry.active()
}

/// One overdue in-flight op as reported by [`op_watchdog_tick`] (D1.b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverdueOp {
    /// Op-kind name (`FuseOpKind::name`).
    pub op: &'static str,
    /// Primary ino argument (parent for name-ops).
    pub ino: u64,
    /// Age at scan time, ms.
    pub age_ms: u64,
}

/// D1.b watchdog scan primitive (design-metadata-throughput §5.1): walk
/// the op registry and report every in-flight op older than `threshold`
/// — logging each LOUDLY and counting `fuse_op_watchdog_overdue` (§9:
/// counter + structured log; the counter counts overdue *observations*,
/// one per scan per stuck op, so a still-stuck op keeps the signal
/// alive). The per-daemon watchdog task calls this on its tick; tests
/// call it directly with their own thresholds (no env coupling).
///
/// Scan-vs-release race note: a slot can be released (or reused by a new
/// op) between the state load and the field loads — worst case one scan
/// reports one op with a mixed kind/ino/age for one tick. Diagnostic
/// grade by design (same contract as the estimator tables above); never
/// memory-unsafe, never affects any op's reply.
pub fn op_watchdog_tick(threshold: Duration) -> Vec<OverdueOp> {
    let now = prof_now_ns();
    let threshold_ns = threshold.as_nanos() as u64;
    let mut overdue = Vec::new();
    for slot in &OP_PROFILE.registry.slots {
        if slot.state.load(Ordering::Acquire) != 1 {
            continue;
        }
        let start = slot.start_ns.load(Ordering::Relaxed);
        let age = now.saturating_sub(start);
        if age < threshold_ns {
            continue;
        }
        let kind = slot.kind.load(Ordering::Relaxed) as usize;
        let op = FuseOpKind::ALL
            .get(kind)
            .map(|k| k.name())
            .unwrap_or("unknown");
        let ino = slot.ino.load(Ordering::Relaxed);
        let age_ms = age / 1_000_000;
        error!(
            "FUSE op watchdog: {op} (ino {ino}) in flight for {age_ms} ms (> {} ms) — \
             op is NOT cancelled (D1.b semantics: no ETIMEDOUT synthesis); investigate \
             the volume/device if this repeats",
            threshold.as_millis()
        );
        METRICS
            .fuse_op_watchdog_overdue
            .fetch_add(1, Ordering::Relaxed);
        overdue.push(OverdueOp { op, ino, age_ms });
    }
    overdue
}

/// Watchdog tick cadence (design §5.1 D1.b: "one per-daemon task ticks
/// every 5 s"). Also the coarse clock's refresh period, so overdue ages
/// are accurate to ± one tick.
const OP_WATCHDOG_TICK: Duration = Duration::from_secs(5);

/// Spawn the per-daemon D1.b watchdog task (idempotent: one per process
/// — the op registry is process-global, so one scanner serves every
/// mounted volume). Called from FUSE `init`.
///
/// Detached-task rationale (the AGENTS.md task-tracking rule): a
/// process-lifetime diagnostic daemon with no state anyone joins on — it
/// owns nothing, holds no budget, and dies with the runtime at process
/// exit. A `JoinHandle` would have no reader.
pub fn spawn_op_watchdog() {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    STARTED.get_or_init(|| {
        tokio::spawn(async move {
            let threshold = get_fuse_timeout();
            let mut ticker = tokio::time::interval(OP_WATCHDOG_TICK);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                // Refresh the coarse clock FIRST so ops registering after
                // this tick stamp a fresh epoch, then scan.
                COARSE_NOW_NS.store(prof_now_ns(), Ordering::Relaxed);
                let overdue = op_watchdog_tick(threshold);
                if !overdue.is_empty() {
                    // Named-holder companion (VL10): the overdue-op lines
                    // name parked OPS; this names the LOCKS they park on
                    // and each blocked stripe's last acquirer, so a wedge
                    // capture reads as a cycle without a gdb session.
                    log_lock_wait_census(threshold);
                }
            }
        });
    });
}

#[inline]
fn osstr_to_cow(name: &std::ffi::OsStr) -> std::borrow::Cow<'_, str> {
    name.to_str()
        .map(std::borrow::Cow::Borrowed)
        .unwrap_or_else(|| name.to_string_lossy())
}

/// POSIX `NAME_MAX`: max bytes in a single path component (excludes the trailing NUL).
/// Reported via `statfs.f_namelen` and enforced on every name-taking FUSE op so the
/// kernel gets `ENAMETOOLONG` instead of silently accepting oversize components.
pub(crate) const FUSE_NAME_MAX: usize = 255;

/// Reject a directory entry name longer than [`FUSE_NAME_MAX`].
#[inline]
pub(crate) fn check_component_name_len(name: &std::ffi::OsStr) -> Result<(), Errno> {
    if name.len() > FUSE_NAME_MAX {
        Err(Errno::from(libc::ENAMETOOLONG))
    } else {
        Ok(())
    }
}

/// `BLOCK_FLUSH_LOCKS` stripe count — shared with the RW1 stripe-collision
/// audit table, which must be sized to the ACTUAL lock population.
pub const BLOCK_LOCK_STRIPES: usize = 4096;

pub static BLOCK_FLUSH_LOCKS: Lazy<StripeLocks<tokio::sync::Mutex<()>, BLOCK_LOCK_STRIPES>> =
    Lazy::new(|| StripeLocks::new());

/// Per-(ino, block) CUSTODY-TRANSFER epoch — the moving-custody read
/// protocol's seqlock word (fstests generic/795). A block's acked bytes
/// migrate RAM overlay ⇄ staged sibling / extent record ⇄ durable binding;
/// every transfer publishes its destination strictly BEFORE retiring its
/// source (under the block's flush lock), so the RETIRE is the only event
/// that can invert a lock-free reader's probe order (destination probed
/// pre-publish, source probed post-retire — acked bytes invisible to both).
/// Every retire bumps this word; the read handler fingerprints it across
/// its whole probe window and re-runs the read on movement. Stripe
/// collisions only ever cause a spurious retry, never a missed one.
pub static BLOCK_CUSTODY_EPOCHS: Lazy<
    StripeLocks<std::sync::atomic::AtomicU64, BLOCK_LOCK_STRIPES>,
> = Lazy::new(StripeLocks::new);

/// Bump `ino`/`b`'s custody epoch (Release) — call at every overlay /
/// staged-sibling / extent-record retire, after the removal completed.
pub fn bump_block_custody_epoch(ino: u64, b: u32) {
    BLOCK_CUSTODY_EPOCHS
        .get_lock(ino, b)
        .fetch_add(1, Ordering::Release);
}

/// Current custody epoch for `ino`/`b` (Acquire).
pub fn block_custody_epoch(ino: u64, b: u32) -> u64 {
    BLOCK_CUSTODY_EPOCHS
        .get_lock(ino, b)
        .load(Ordering::Acquire)
}

/// The read window's custody fingerprint — the moving-custody read
/// protocol's defense #3 (generic/795), compact form (P2 per-op economy).
///
/// Semantics are the old tuple-vector's exactly: per covered block, the
/// CURRENT binding key and the (ino, block) custody-epoch word; two
/// fingerprints match iff every covered block agrees on both. The
/// representation changed, not the verdict: the map shape holds the
/// shared `block_map` **Arc** (clone = refcount bump — writers publish
/// copy-on-write via `Arc::make_mut`, so a held snapshot's content is
/// immutable) instead of cloning every covered key String per read per
/// side, and `matches` compares pointer-equal maps by epochs alone.
/// Epoch monotonicity kills binding-string ABA (a key can be retired and
/// republished byte-identical only across a retire, which bumped the
/// word), so the semantic compare is exactly as strong as the owned one.
pub enum ReadCustodyFp {
    /// Inline block map: the shared Arc + the covered window's epochs.
    Map {
        map: std::sync::Arc<std::collections::HashMap<u32, String>>,
        start: u32,
        epochs: Vec<u64>,
    },
    /// `block_prefix` files: keys are pure functions of the prefix.
    Prefix {
        prefix: String,
        start: u32,
        epochs: Vec<u64>,
    },
    /// Authoritative async fallback (the anomalous map-id-without-map
    /// shape): owned resolved keys, the historical representation.
    Owned(Vec<(u32, Option<String>, u64)>),
}

/// A covered block's binding key, borrowed from its fingerprint — the
/// `Derived` arm avoids materializing `"{prefix}/part_{b}"` per compare.
enum FpKeyRef<'a> {
    Hole,
    Str(&'a str),
    Derived(&'a str, u32),
}

impl FpKeyRef<'_> {
    fn eq(&self, other: &FpKeyRef<'_>) -> bool {
        use FpKeyRef::*;
        match (self, other) {
            (Hole, Hole) => true,
            (Str(a), Str(b)) => a == b,
            (Derived(p, b), Derived(q, c)) => p == q && b == c,
            (Derived(p, b), Str(s)) | (Str(s), Derived(p, b)) => {
                // s == format!("{p}/part_{b}") without the alloc.
                s.strip_prefix(p)
                    .and_then(|r| r.strip_prefix("/part_"))
                    .and_then(|r| r.parse::<u32>().ok())
                    .is_some_and(|n| n == *b)
            }
            _ => false,
        }
    }
}

impl ReadCustodyFp {
    /// Build from an in-hand RAM metadata snapshot — zero cache probes,
    /// zero key clones (one Arc bump / one prefix String). Returns `None`
    /// for shapes with no custody chain (non-striped, empty window) AND
    /// for the anomalous map-id-without-inline-map entry, which must ride
    /// the authoritative async resolve instead (never fingerprint a shape
    /// whose keys this builder cannot see).
    pub fn build_sync(
        meta: &crate::routing::CachedMetadata,
        ino: u64,
        block_size: u64,
        offset: u64,
        len: usize,
    ) -> Option<ReadCustodyFp> {
        if len == 0 || meta.file_type != "striped" || block_size == 0 {
            return None;
        }
        let start = (offset / block_size) as u32;
        let end = ((offset + len as u64 - 1) / block_size) as u32;
        let epochs: Vec<u64> = (start..=end).map(|b| block_custody_epoch(ino, b)).collect();
        if let Some(map) = &meta.block_map {
            Some(ReadCustodyFp::Map {
                map: map.clone(),
                start,
                epochs,
            })
        } else if meta.block_map_id.is_some() {
            None
        } else {
            meta.block_prefix.as_ref().map(|p| ReadCustodyFp::Prefix {
                prefix: p.clone(),
                start,
                epochs,
            })
        }
    }

    fn window(&self) -> (u32, usize) {
        match self {
            ReadCustodyFp::Map { start, epochs, .. }
            | ReadCustodyFp::Prefix { start, epochs, .. } => (*start, epochs.len()),
            ReadCustodyFp::Owned(v) => (v.first().map(|(b, _, _)| *b).unwrap_or(0), v.len()),
        }
    }

    fn epoch(&self, i: usize) -> u64 {
        match self {
            ReadCustodyFp::Map { epochs, .. } | ReadCustodyFp::Prefix { epochs, .. } => epochs[i],
            ReadCustodyFp::Owned(v) => v[i].2,
        }
    }

    fn key(&self, i: usize) -> FpKeyRef<'_> {
        match self {
            ReadCustodyFp::Map { map, start, .. } => match map.get(&(start + i as u32)) {
                Some(k) => FpKeyRef::Str(k),
                None => FpKeyRef::Hole,
            },
            ReadCustodyFp::Prefix { prefix, start, .. } => {
                FpKeyRef::Derived(prefix, start + i as u32)
            }
            ReadCustodyFp::Owned(v) => match &v[i].1 {
                Some(k) => FpKeyRef::Str(k),
                None => FpKeyRef::Hole,
            },
        }
    }

    /// Semantic equality over the covered window: per-block binding key
    /// AND custody epoch. Pointer-equal map Arcs short-circuit the key
    /// walk (the common no-movement case) — CoW publication makes pointer
    /// equality a proof of content equality.
    pub fn matches(&self, other: &ReadCustodyFp) -> bool {
        let (sa, la) = self.window();
        let (sb, lb) = other.window();
        if sa != sb || la != lb {
            return false;
        }
        let keys_known_equal = match (self, other) {
            (ReadCustodyFp::Map { map: a, .. }, ReadCustodyFp::Map { map: b, .. }) => {
                std::sync::Arc::ptr_eq(a, b)
            }
            (ReadCustodyFp::Prefix { prefix: a, .. }, ReadCustodyFp::Prefix { prefix: b, .. }) => {
                a == b
            }
            _ => false,
        };
        for i in 0..la {
            if self.epoch(i) != other.epoch(i) {
                return false;
            }
            if !keys_known_equal && !self.key(i).eq(&other.key(i)) {
                return false;
            }
        }
        true
    }
}

/// `Option` face of [`ReadCustodyFp::matches`] — `None` (no custody chain)
/// only ever matches `None`, the old `Option<Vec<…>> ==` verdict.
pub(crate) fn read_custody_fp_matches(
    a: &Option<ReadCustodyFp>,
    b: &Option<ReadCustodyFp>,
) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x.matches(y),
        _ => false,
    }
}

// ===========================================================================
// RW1 write-path attribution rig (docs/design-random-small-writes.md PR RW1;
// §5.3 W3 rig extension; §5.4 observability)
//
// Extends the M2 rig with the striped-write cost anatomy the rand-write
// program's forensics run on:
//
//  1. **Write sub-phase histograms** (`fuse_write_phase_ns`): route-classify
//     → checkout (buffer acquire) → staged-sibling remove (the per-write
//     spawn_blocking hop, H1) → merge copy → seed fetch (item-B RMW
//     materialization, all drivers) → upload DMA / upload map-merge (H2
//     hold-time split) → park/spill → staging put.
//  2. **Per-site `BLOCK_FLUSH_LOCKS` wait attribution** (`block_lock_wait_by_
//     site`): the FIND-L1-A `block_lock_wait` tail, split by acquiring call
//     site (checkout vs spill-victim vs writeback-flush vs flush-exit vs
//     punch vs overlay-prune).
//  3. **The H2b stripe-collision audit** (`block_lock_stripe_audit`):
//     contended acquisitions classified CROSS-KEY (a different (ino, block)
//     key holds the shared stripe — the splitmix-spread/stripe-count defect
//     signature) vs SAME-KEY (true per-block serialization), plus the
//     waiters-at-arrival depth. Diagnostic-grade by design: the holder word
//     is the stripe's LAST acquirer (release does not clear it), so one
//     racing sample can misclassify — same contract as the M2 estimator
//     tables; never memory-unsafe, never affects any op's reply.
//  4. **The in-flight WRITE histogram** (`fuse_write_inflight`): concurrent
//     WRITE handler depth at each arrival — the §12 OQ2 answer (does the
//     kernel actually dispatch ≥ iodepth×threads concurrent WRITEs).
//
// Cost contract (the M2 memoized-gate pattern, pinned by
// `tests/rand_write_rig_off_tests.rs`): with `SQUEEZEFS_OP_PROFILE` unset
// every helper here is one memoized atomic load + branch — no `Instant`
// reads, no atomics touched, no histogram writes, and the stats JSON surface
// is byte-identical to pre-RW1. The always-on §1.2 device-byte LEDGER
// counters live in [`Metrics`] instead (one relaxed `fetch_add` on
// 4 MiB-class paths — the red gate consumes them without profile mode).
// ===========================================================================

/// Striped-write sub-phases (`fuse_write_phase_ns` — design §5.3 W3 list).
/// `repr(usize)` indexes the histogram table directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum WritePhase {
    /// Write handler backend entry → `write_file_staged` dispatch (lease +
    /// layout classification — the striped route decision).
    RouteClassify = 0,
    /// Block lock held → RMW base owned (parked buffer / staged copy /
    /// fresh / deferred).
    Checkout = 1,
    /// The per-write staged-sibling `spawn_blocking` remove hop (H1).
    SiblingRemove = 2,
    /// `record_write` + `make_mut` + payload slice copy.
    MergeCopy = 3,
    /// `fetch_seed_image` — the deferred RMW seed materialization, every
    /// driver (gap / trigger / spill victim / flush exits / parked gate).
    SeedFetch = 4,
    /// `upload_full_block` crypto → allocate → DMA (H2 hold-time, device
    /// leg).
    UploadDma = 5,
    /// `upload_full_block` block-map merge (H2 hold-time, conveyor leg).
    UploadMapMerge = 6,
    /// `insert_active_block_buffer` — park incl. the inline victim-spill
    /// loop and the Red parked-gate.
    ParkSpill = 7,
    /// `put_active_block` staging writes (spill / flush / fallback).
    StagingPut = 8,
}

const WRITE_PHASES: usize = 9;
const WRITE_PHASE_NAMES: [&str; WRITE_PHASES] = [
    "route_classify",
    "checkout",
    "sibling_remove",
    "merge_copy",
    "seed_fetch",
    "upload_dma",
    "upload_map_merge",
    "park_spill",
    "staging_put",
];

/// `BLOCK_FLUSH_LOCKS` acquiring call-site classes (`block_lock_wait_by_site`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum BlockLockSite {
    /// `write_file_staged` per-block checkout (the write hot path).
    WriteCheckout = 0,
    /// `insert_active_block_buffer` victim `try_lock` (contended = skip,
    /// counted separately — a blocking acquire can self-deadlock, §5.3).
    SpillVictim = 1,
    /// `flush_one_active_block` (writeback worker / fsync flush units).
    WritebackFlush = 2,
    /// `flush_memory_buffers_for_inode` / teardown stage-exit sweeps.
    FlushExit = 3,
    /// `punch_hole_range` whole-block drop arm.
    Punch = 4,
    /// `drop_active_block_overlays_beyond` (truncate prune).
    OverlayPrune = 5,
    /// The READ path's deferred-seed materialize (single-block read hitting
    /// an item-B deferred buffer) — a reader convoying with writers is
    /// attribution the H2b audit must see.
    ReadSeed = 6,
    /// `DataRouter::write_file`'s staged/inline-promotion block-0 guard
    /// (the FIND-VS-B staged-layout sibling shape).
    StagedWrite = 7,
    /// W2 per-block fold (`fold_extent_block`) — seed once, apply k
    /// extents, one durable upload (design-random-small-writes §5.2).
    Fold = 8,
    /// The VL8-item-7 read-path contention escalation
    /// (`DataRouter::get_block_for_index`, `escalate_contended`) — one
    /// fetch serialized under the block's stripe.
    ReadEscalate = 9,
}

const BLOCK_LOCK_SITES: usize = 10;
const BLOCK_LOCK_SITE_NAMES: [&str; BLOCK_LOCK_SITES] = [
    "write_checkout",
    "spill_victim",
    "writeback_flush",
    "flush_exit",
    "punch",
    "overlay_prune",
    "read_seed",
    "staged_write",
    "fold",
    "read_escalate",
];

struct WriteProfState {
    /// `fuse_write_phase_ns` histograms, [`WritePhase`]-indexed.
    phases: [LatencyHistogram; WRITE_PHASES],
    /// `block_lock_wait_by_site` histograms, [`BlockLockSite`]-indexed
    /// (every profiled acquisition, contended or not).
    site_waits: [LatencyHistogram; BLOCK_LOCK_SITES],
    /// Contended waits whose stripe's last acquirer was a DIFFERENT
    /// (ino, block) key — the H2b cross-key collision class.
    cross_key_waits: LatencyHistogram,
    /// Contended waits on the waiter's own key — true block serialization.
    same_key_waits: LatencyHistogram,
    /// Waiters already parked on the stripe at arrival (contended
    /// acquisitions only) — the convoy-depth distribution.
    stripe_waiters: QueueDepthHistogram,
    /// Concurrent WRITE handler depth sampled at each WRITE arrival.
    inflight_writes: QueueDepthHistogram,
    /// Spill-victim `try_lock` refusals (the contended-skip arm).
    spill_victim_lock_skips: AtomicU64,
    /// Last-acquirer key word per stripe (0 = never acquired). Written on
    /// every profiled acquisition; NEVER cleared on release — "last
    /// acquirer" semantics, diagnostic-grade (see the module comment).
    stripe_holders: Vec<AtomicU64>,
    /// Live parked waiters per stripe (profiled contended acquisitions).
    stripe_wait_depth: Vec<AtomicU64>,
    /// Live WRITE handler gauge feeding `inflight_writes`.
    write_gauge: AtomicU64,
}

static WRITE_PROF: Lazy<WriteProfState> = Lazy::new(|| WriteProfState {
    phases: std::array::from_fn(|_| LatencyHistogram::default()),
    site_waits: std::array::from_fn(|_| LatencyHistogram::default()),
    cross_key_waits: LatencyHistogram::default(),
    same_key_waits: LatencyHistogram::default(),
    stripe_waiters: QueueDepthHistogram::default(),
    inflight_writes: QueueDepthHistogram::default(),
    spill_victim_lock_skips: AtomicU64::new(0),
    stripe_holders: (0..BLOCK_LOCK_STRIPES).map(|_| AtomicU64::new(0)).collect(),
    stripe_wait_depth: (0..BLOCK_LOCK_STRIPES).map(|_| AtomicU64::new(0)).collect(),
    write_gauge: AtomicU64::new(0),
});

/// Nonzero identity word for a `(ino, block)` key in the stripe audit —
/// the same splitmix64 mix the lock table spreads on, `| 1` so 0 stays the
/// "never acquired" sentinel. Two distinct keys colliding on one WORD is a
/// ~2⁻⁶³ diagnostic misclassification, not a correctness event.
#[inline]
fn stripe_key64(ino: u64, b: u32) -> u64 {
    let mut x = ino ^ ((b as u64) << 32);
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58476d1ce4e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d049bb133111eb);
    x ^= x >> 31;
    x | 1
}

/// Phase-stamp start: ONE memoized load + branch when the rig is off (no
/// clock read — the M2 cost contract).
#[inline]
pub fn write_phase_start() -> Option<std::time::Instant> {
    if op_profile_enabled() {
        Some(std::time::Instant::now())
    } else {
        None
    }
}

/// Record a write sub-phase span started by [`write_phase_start`]. No-op
/// (no clock read, no histogram touch) when the start was rig-off `None`.
#[inline]
pub fn write_phase_record(phase: WritePhase, started: Option<std::time::Instant>) {
    if let Some(t0) = started {
        WRITE_PROF.phases[phase as usize].record(t0.elapsed());
    }
}

/// Profile-armed `BLOCK_FLUSH_LOCKS` acquisition: per-site wait histogram +
/// the H2b collision classification. `try_lock` first — its success arm is
/// the same one-CAS fast path `lock()` takes, so semantics and uncontended
/// cost are unchanged; the contended arm classifies against the stripe's
/// last-acquirer word BEFORE parking, then falls into the ordinary FIFO
/// `lock().await`.
async fn block_lock_acquire_prof(
    lock: &tokio::sync::Mutex<()>,
    site: BlockLockSite,
    ino: u64,
    b: u32,
) -> (tokio::sync::MutexGuard<'_, ()>, Duration) {
    let key = stripe_key64(ino, b);
    let stripe = BLOCK_FLUSH_LOCKS.block_shard_index(ino, b);
    let t0 = std::time::Instant::now();
    let guard = match lock.try_lock() {
        Ok(g) => g,
        Err(_) => {
            let holder = WRITE_PROF.stripe_holders[stripe].load(Ordering::Relaxed);
            let depth = WRITE_PROF.stripe_wait_depth[stripe].fetch_add(1, Ordering::Relaxed) + 1;
            WRITE_PROF.stripe_waiters.record(depth as usize);
            let _t = LockWaitToken::begin(LockClass::Block, site as u64, ino, b as u64);
            let g = lock.lock().await;
            WRITE_PROF.stripe_wait_depth[stripe].fetch_sub(1, Ordering::Relaxed);
            let waited = t0.elapsed();
            if holder != 0 && holder != key {
                WRITE_PROF.cross_key_waits.record(waited);
            } else {
                WRITE_PROF.same_key_waits.record(waited);
            }
            g
        }
    };
    let waited = t0.elapsed();
    WRITE_PROF.stripe_holders[stripe].store(key, Ordering::Relaxed);
    STRIPE_LAST_HOLDER[stripe].store(pack_holder(site, ino, b), Ordering::Relaxed);
    WRITE_PROF.site_waits[site as usize].record(waited);
    (guard, waited)
}

/// Acquire block `(ino, b)`'s `BLOCK_FLUSH_LOCKS` stripe with RW1 per-site
/// attribution, returning the guard and the measured wait — for the two
/// HISTORICAL sites (write checkout, writeback flush) that always timed
/// their wait into the global `block_lock_wait` histogram: the rig-off path
/// is exactly today's `Instant` + `lock().await` sequence, and the caller
/// keeps recording the returned wait into the global histogram so the
/// FIND-L1-A baseline series stays comparable.
pub async fn block_lock_acquire_timed(
    ino: u64,
    b: u32,
    site: BlockLockSite,
) -> (tokio::sync::MutexGuard<'static, ()>, Duration) {
    let lock = BLOCK_FLUSH_LOCKS.get_lock(ino, b);
    if !op_profile_enabled() {
        let t0 = std::time::Instant::now();
        let g = block_lock_census_acquire(lock, site, ino, b).await;
        return (g, t0.elapsed());
    }
    block_lock_acquire_prof(lock, site, ino, b).await
}

/// Always-on census acquisition: `try_lock` fast path (one CAS, the same
/// state transition `lock()` performs uncontended); the contended arm
/// registers a [`LockWaitToken`] so the watchdog can name this wait, and
/// every acquisition stamps the stripe's last-holder word.
async fn block_lock_census_acquire(
    lock: &tokio::sync::Mutex<()>,
    site: BlockLockSite,
    ino: u64,
    b: u32,
) -> tokio::sync::MutexGuard<'_, ()> {
    let g = match lock.try_lock() {
        Ok(g) => g,
        Err(_) => {
            let _t = LockWaitToken::begin(LockClass::Block, site as u64, ino, b as u64);
            lock.lock().await
        }
    };
    let stripe = BLOCK_FLUSH_LOCKS.block_shard_index(ino, b);
    STRIPE_LAST_HOLDER[stripe].store(pack_holder(site, ino, b), Ordering::Relaxed);
    g
}

/// [`block_lock_acquire_timed`] for the sites that never timed their wait:
/// the rig-off path is a bare `lock().await` — ZERO clock reads (the M2
/// contract; these sites paid none before RW1 and pay none after).
pub async fn block_lock_acquire(
    ino: u64,
    b: u32,
    site: BlockLockSite,
) -> tokio::sync::MutexGuard<'static, ()> {
    let lock = BLOCK_FLUSH_LOCKS.get_lock(ino, b);
    if !op_profile_enabled() {
        return block_lock_census_acquire(lock, site, ino, b).await;
    }
    block_lock_acquire_prof(lock, site, ino, b).await.0
}

/// Note a spill-victim `try_lock` outcome (the one site whose contended arm
/// SKIPS instead of waiting — §5.3 mandatory try_lock): acquisitions update
/// the stripe holder word + site histogram (zero wait by construction),
/// refusals count `spill_victim_lock_skips`. No-op when the rig is off.
pub fn block_lock_try_note(site: BlockLockSite, ino: u64, b: u32, acquired: bool) {
    if !op_profile_enabled() {
        return;
    }
    if acquired {
        let stripe = BLOCK_FLUSH_LOCKS.block_shard_index(ino, b);
        WRITE_PROF.stripe_holders[stripe].store(stripe_key64(ino, b), Ordering::Relaxed);
        WRITE_PROF.site_waits[site as usize].record(Duration::ZERO);
    } else {
        WRITE_PROF
            .spill_victim_lock_skips
            .fetch_add(1, Ordering::Relaxed);
    }
}

// ===========================================================================
// Lock-wait census — the D1.b watchdog's NAMED-HOLDER surface (VL10).
//
// The VL8 wedge captures proved the overdue-op list alone cannot draw a
// deadlock cycle: it names parked OPS, not the LOCKS they park on or the
// keys that hold those locks. This census tracks, always-on:
//
//  - **Live waiters** on the two lock populations every write-path cycle
//    has run through (`BLOCK_FLUSH_LOCKS`, `INODE_META_LOCKS`): a fixed
//    lock-free slab, claimed only on the CONTENDED path (the uncontended
//    fast path pays one extra `try_lock` CAS, nothing else).
//  - **Last holder per BLOCK stripe**: one relaxed store per acquisition
//    packing (site, ino, block) — "last acquirer" semantics like the RW1
//    audit word (release does not clear it), diagnostic-grade by design.
//
// The watchdog tick appends the census to its overdue report, so a wedge
// capture reads as a cycle: op X (ino A) waits block stripe S whose last
// holder was site=fold (ino B, block C), which waits meta ino B, …
// ===========================================================================

/// Census lock classes (the populations wired so far).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LockClass {
    /// `BLOCK_FLUSH_LOCKS` (P1-9 level 3) — `key` = block index.
    Block = 0,
    /// `INODE_META_LOCKS` (P1-9 level 3.5) — `key` unused.
    Meta = 1,
}

const LOCK_CLASS_NAMES: [&str; 2] = ["block", "meta"];
const LOCK_WAIT_SLOTS: usize = 512;

struct LockWaitSlot {
    /// 0 = free, 1 = claimed/waiting.
    state: AtomicU64,
    class: AtomicU64,
    site: AtomicU64,
    ino: AtomicU64,
    key: AtomicU64,
    since_ns: AtomicU64,
}

static LOCK_WAIT_CENSUS: Lazy<Vec<LockWaitSlot>> = Lazy::new(|| {
    (0..LOCK_WAIT_SLOTS)
        .map(|_| LockWaitSlot {
            state: AtomicU64::new(0),
            class: AtomicU64::new(0),
            site: AtomicU64::new(0),
            ino: AtomicU64::new(0),
            key: AtomicU64::new(0),
            since_ns: AtomicU64::new(0),
        })
        .collect()
});

/// Always-on last-holder word per `BLOCK_FLUSH_LOCKS` stripe:
/// `[63:60] site+1 (0 = never held)`, `[59:24] ino low 36`,
/// `[23:0] block low 24`. Truncation/collision = one misattributed
/// diagnostic line, never a correctness event.
static STRIPE_LAST_HOLDER: Lazy<Vec<AtomicU64>> =
    Lazy::new(|| (0..BLOCK_LOCK_STRIPES).map(|_| AtomicU64::new(0)).collect());

#[inline]
fn pack_holder(site: BlockLockSite, ino: u64, b: u32) -> u64 {
    ((site as u64 + 1) << 60) | ((ino & ((1 << 36) - 1)) << 24) | (b as u64 & 0xFF_FFFF)
}

fn unpack_holder(word: u64) -> Option<(&'static str, u64, u64)> {
    let site = (word >> 60) as usize;
    if site == 0 {
        return None;
    }
    let name = BLOCK_LOCK_SITE_NAMES
        .get(site - 1)
        .copied()
        .unwrap_or("unknown");
    Some((name, (word >> 24) & ((1 << 36) - 1), word & 0xFF_FFFF))
}

/// RAII census entry for one CONTENDED lock wait. Slab exhaustion (more
/// than `LOCK_WAIT_SLOTS` concurrent contended waits) degrades to an
/// uncounted wait — never blocks, never allocates.
pub struct LockWaitToken(usize);

impl LockWaitToken {
    fn begin(class: LockClass, site: u64, ino: u64, key: u64) -> LockWaitToken {
        for (i, slot) in LOCK_WAIT_CENSUS.iter().enumerate() {
            if slot
                .state
                .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                slot.class.store(class as u64, Ordering::Relaxed);
                slot.site.store(site, Ordering::Relaxed);
                slot.ino.store(ino, Ordering::Relaxed);
                slot.key.store(key, Ordering::Relaxed);
                slot.since_ns.store(prof_now_ns(), Ordering::Relaxed);
                return LockWaitToken(i);
            }
        }
        LockWaitToken(usize::MAX)
    }
}

impl Drop for LockWaitToken {
    fn drop(&mut self) {
        if self.0 != usize::MAX {
            LOCK_WAIT_CENSUS[self.0].state.store(0, Ordering::Release);
        }
    }
}

/// One live contended lock wait as reported by [`lock_wait_census`].
#[derive(Debug, Clone)]
pub struct LockWaitEntry {
    /// Lock class name (`block` / `meta`).
    pub class: &'static str,
    /// Acquiring call-site name (block class) or `-`.
    pub site: &'static str,
    /// Waiter's target inode.
    pub ino: u64,
    /// Waiter's target block (block class; 0 for meta).
    pub key: u64,
    /// Wait age at scan, ms.
    pub waited_ms: u64,
    /// The blocked stripe's LAST holder, when known (block class):
    /// `(site, ino, block)` — last-acquirer semantics.
    pub holder: Option<(&'static str, u64, u64)>,
}

/// Scan the census for waits older than `threshold` (the watchdog's
/// named-holder report; tests call it directly). Same scan-vs-release
/// race contract as [`op_watchdog_tick`]: diagnostic-grade, never wrong
/// about a wait that is genuinely stuck.
pub fn lock_wait_census(threshold: Duration) -> Vec<LockWaitEntry> {
    let now = prof_now_ns();
    let threshold_ns = threshold.as_nanos() as u64;
    let mut out = Vec::new();
    for slot in LOCK_WAIT_CENSUS.iter() {
        if slot.state.load(Ordering::Acquire) != 1 {
            continue;
        }
        let age = now.saturating_sub(slot.since_ns.load(Ordering::Relaxed));
        if age < threshold_ns {
            continue;
        }
        let class_idx = slot.class.load(Ordering::Relaxed) as usize;
        let class = LOCK_CLASS_NAMES.get(class_idx).copied().unwrap_or("?");
        let site_idx = slot.site.load(Ordering::Relaxed) as usize;
        let ino = slot.ino.load(Ordering::Relaxed);
        let key = slot.key.load(Ordering::Relaxed);
        let (site, holder) = if class_idx == LockClass::Block as usize {
            let stripe = BLOCK_FLUSH_LOCKS.block_shard_index(ino, key as u32);
            (
                BLOCK_LOCK_SITE_NAMES.get(site_idx).copied().unwrap_or("?"),
                unpack_holder(STRIPE_LAST_HOLDER[stripe].load(Ordering::Relaxed)),
            )
        } else {
            ("-", None)
        };
        out.push(LockWaitEntry {
            class,
            site,
            ino,
            key,
            waited_ms: age / 1_000_000,
            holder,
        });
    }
    out
}

/// Log the named-holder census (the watchdog's cycle-drawing companion to
/// the overdue-op lines). Bounded output; one line per stuck wait.
pub fn log_lock_wait_census(threshold: Duration) {
    for e in lock_wait_census(threshold).into_iter().take(64) {
        match e.holder {
            Some((hsite, hino, hb)) => error!(
                "lock-wait census: {}/{} ino {} key {} waited {} ms — stripe last-holder \
                 site={} ino {} block {}",
                e.class, e.site, e.ino, e.key, e.waited_ms, hsite, hino, hb
            ),
            None => error!(
                "lock-wait census: {}/{} ino {} key {} waited {} ms",
                e.class, e.site, e.ino, e.key, e.waited_ms
            ),
        }
    }
}

/// Census-wrapped acquisition of an `INODE_META_LOCKS`-class mutex (used
/// by `routing::meta_lock_acquire` — the lock lives in `routing`, the
/// census here). Fast path: one `try_lock`.
pub async fn census_meta_lock_acquire(
    lock: &tokio::sync::Mutex<()>,
    ino: u64,
) -> tokio::sync::MutexGuard<'_, ()> {
    if let Ok(g) = lock.try_lock() {
        return g;
    }
    let _t = LockWaitToken::begin(LockClass::Meta, 0, ino, 0);
    lock.lock().await
}

/// RAII in-flight WRITE sample (`fuse_write_inflight`): entering samples the
/// live WRITE-handler depth into the histogram; drop releases the gauge.
/// Rig off: no atomics, no samples (the gauge itself stays untouched).
pub struct WriteInflight {
    armed: bool,
}

impl WriteInflight {
    pub fn enter() -> WriteInflight {
        if op_profile_enabled() {
            let depth = WRITE_PROF.write_gauge.fetch_add(1, Ordering::Relaxed) + 1;
            WRITE_PROF.inflight_writes.record(depth as usize);
            WriteInflight { armed: true }
        } else {
            WriteInflight { armed: false }
        }
    }
}

impl Drop for WriteInflight {
    fn drop(&mut self) {
        if self.armed {
            WRITE_PROF.write_gauge.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// `fuse_write_phase_ns` stats payload: `{phase: histogram}`.
pub fn write_profile_phase_json() -> serde_json::Value {
    let mut phases = serde_json::Map::new();
    for (pi, pname) in WRITE_PHASE_NAMES.iter().enumerate() {
        phases.insert((*pname).to_string(), WRITE_PROF.phases[pi].to_json());
    }
    serde_json::Value::Object(phases)
}

/// `block_lock_wait_by_site` stats payload: `{site: histogram}`.
pub fn block_lock_site_json() -> serde_json::Value {
    let mut sites = serde_json::Map::new();
    for (si, sname) in BLOCK_LOCK_SITE_NAMES.iter().enumerate() {
        sites.insert((*sname).to_string(), WRITE_PROF.site_waits[si].to_json());
    }
    serde_json::Value::Object(sites)
}

/// `block_lock_stripe_audit` stats payload (the H2b deliverable):
/// `{cross_key_waits, same_key_waits, waiters_at_arrival,
/// spill_victim_lock_skips}`.
pub fn block_lock_stripe_audit_json() -> serde_json::Value {
    serde_json::json!({
        "cross_key_waits": WRITE_PROF.cross_key_waits.to_json(),
        "same_key_waits": WRITE_PROF.same_key_waits.to_json(),
        "waiters_at_arrival": WRITE_PROF.stripe_waiters.to_json(),
        "spill_victim_lock_skips": WRITE_PROF
            .spill_victim_lock_skips
            .load(Ordering::Relaxed),
    })
}

/// `fuse_write_inflight` stats payload.
pub fn write_inflight_json() -> serde_json::Value {
    WRITE_PROF.inflight_writes.to_json()
}

/// §1.2 driver attribution for the flush-exit staging/upload legs
/// (design-random-small-writes review Issue 4): the R5-pressure parked
/// drain and the fsync/close family move bytes through the SAME primitive
/// (`flush_memory_buffers_for_inode`) — the ledger must know which driver
/// paid, because on the loss shape the durable-upload stream is
/// drain-driven, never fsync-driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlushDriver {
    /// fsync / FLUSH / close-background family.
    FsyncClose,
    /// The R5-pressure parked drain (`drain_parked_toward`).
    ParkedDrain,
}

impl FlushDriver {
    fn staging_put_bytes_counter(self) -> &'static AtomicU64 {
        match self {
            FlushDriver::FsyncClose => &METRICS.staging_put_bytes_flush,
            FlushDriver::ParkedDrain => &METRICS.staging_put_bytes_drain,
        }
    }

    fn writeback_enqueued_counter(self) -> &'static AtomicU64 {
        match self {
            FlushDriver::FsyncClose => &METRICS.writeback_enqueued_flush,
            FlushDriver::ParkedDrain => &METRICS.writeback_enqueued_drain,
        }
    }
}

/// P1-8: how long the FUSE write path holds the per-inode write lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InodeWriteLockScope {
    /// Hold exclusive lock for the entire write (inline RMW or layout transition).
    EntireOp,
    /// Hold exclusive lock only for meta-prep (lease, size/type, quota, attr/meta
    /// size update). Data I/O runs without the inode write lock; per-block
    /// [`BLOCK_FLUSH_LOCKS`] serialize active-block mutation.
    MetaPrepOnly,
}

/// Decide inode write-lock scope for a FUSE write.
///
/// Already-striped files use block-level locks on the data path, so the full-inode
/// write lock need only cover short meta-prep. Inline and non-striped (layout
/// transition / whole-buffer RMW) paths keep the lock for the entire operation.
pub const MAX_INLINE_SIZE: u64 = 4096;

#[inline]
pub fn inode_write_lock_scope(fits_inline: bool, is_striped: bool) -> InodeWriteLockScope {
    if fits_inline || !is_striped {
        InodeWriteLockScope::EntireOp
    } else {
        InodeWriteLockScope::MetaPrepOnly
    }
}

/// §5.4 lease-severance boundary (zero-copy write-path design, PR 5).
///
/// FUSE_WRITE payloads arrive as zero-copy transport leases over the
/// registered FUSE-over-io_uring payload buffer; the ring ent is not
/// re-armed (COMMIT_AND_FETCH) until the lease drops. A lease that escapes
/// the write handler into a long-lived sink (`data_key`, the LRUs) parks
/// that ent forever — at `Q_DEPTH = 4`, a deterministic mount hang. The one
/// route that hands the payload to `DataRouter::write_file` — the
/// `use_router_write` branch top; PR 6 deleted the transitional
/// `is_aligned` second sever point together with its branch — therefore
/// materializes it lease-free first with one unconditional copy (the same
/// bytes the transport used to copy *twice* before PR 5, now paid only by
/// the small/staged routes — the hot striped route consumes the lease via
/// the accumulation merge and never severs). Unconditional rather than
/// lease-detecting: `bytes::Bytes` cannot cheaply introspect its owner, and
/// a copy on the cold routes beats a reachability argument every reviewer
/// must re-verify. Invariant, checkable in one place: a transport lease
/// never escapes the write handler's call graph — consumed by the
/// accumulation merge, the one-shot severing copy, or `sever_payload`,
/// all before the handler returns.
#[inline]
fn sever_payload(data: &bytes::Bytes) -> bytes::Bytes {
    bytes::Bytes::copy_from_slice(data)
}

struct ThreadLocalState {
    count: u64,
    target: *const AtomicU64,
}

impl Drop for ThreadLocalState {
    fn drop(&mut self) {
        if self.count > 0 && !self.target.is_null() {
            unsafe {
                (*self.target).fetch_add(self.count, Ordering::Relaxed);
            }
        }
    }
}

#[derive(Default)]
pub struct ProbabilisticAtomic {
    inner: AtomicU64,
}

impl ProbabilisticAtomic {
    pub fn fetch_add(&self, val: u64, order: Ordering) -> u64 {
        thread_local! {
            static STATE: std::cell::RefCell<ThreadLocalState> = std::cell::RefCell::new(ThreadLocalState {
                count: 0,
                target: std::ptr::null(),
            });
        }
        STATE.with(|s| {
            let mut state = s.borrow_mut();
            if state.target.is_null() {
                state.target = &self.inner as *const AtomicU64;
            }
            state.count += val;
            if state.count >= 128 {
                let to_add = state.count;
                state.count = 0;
                self.inner.fetch_add(to_add, order)
            } else {
                self.inner.load(order)
            }
        })
    }

    pub fn load(&self, order: Ordering) -> u64 {
        self.inner.load(order)
    }
}

#[repr(align(64))]
#[derive(Default)]
pub struct Align64<T>(pub T);

impl<T> std::ops::Deref for Align64<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> std::ops::DerefMut for Align64<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

pub struct LatencyHistogram {
    pub buckets: [AtomicU64; 26],
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self {
            buckets: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
        }
    }
}

impl LatencyHistogram {
    pub fn record(&self, duration: Duration) {
        let micros = duration.as_micros() as u64;
        let bucket_idx = if micros <= 1 {
            0
        } else {
            let idx = (micros - 1).ilog2() as usize + 1;
            std::cmp::min(idx, 25)
        };
        self.buckets[bucket_idx].fetch_add(1, Ordering::Relaxed);
    }

    pub fn to_json(&self) -> serde_json::Value {
        const LABELS: &[&str] = &[
            "<=1us", "<=2us", "<=4us", "<=8us", "<=16us", "<=32us", "<=64us", "<=128us", "<=256us",
            "<=512us", "<=1024us", "<=2ms", "<=4ms", "<=8ms", "<=16ms", "<=32ms", "<=64ms",
            "<=128ms", "<=256ms", "<=512ms", "<=1024ms", "<=2s", "<=4s", "<=8s", "<=16s", ">16s",
        ];
        let mut map = serde_json::Map::new();
        for (i, label) in LABELS.iter().enumerate() {
            let val = self.buckets[i].load(Ordering::Relaxed);
            map.insert(
                label.to_string(),
                serde_json::Value::Number(serde_json::Number::from(val)),
            );
        }
        serde_json::Value::Object(map)
    }
}

pub struct QueueDepthHistogram {
    pub buckets: [AtomicU64; 15],
}

impl Default for QueueDepthHistogram {
    fn default() -> Self {
        Self {
            buckets: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
        }
    }
}

impl QueueDepthHistogram {
    pub fn record(&self, depth: usize) {
        let bucket_idx = if depth == 0 {
            0
        } else if depth == 1 {
            1
        } else if depth == 2 {
            2
        } else {
            let idx = (depth - 1).ilog2() as usize + 2;
            std::cmp::min(idx, 14)
        };
        self.buckets[bucket_idx].fetch_add(1, Ordering::Relaxed);
    }

    pub fn to_json(&self) -> serde_json::Value {
        const LABELS: &[&str] = &[
            "0", "1", "2", "<=4", "<=8", "<=16", "<=32", "<=64", "<=128", "<=256", "<=512",
            "<=1024", "<=2048", "<=4096", ">4096",
        ];
        let mut map = serde_json::Map::new();
        for (i, label) in LABELS.iter().enumerate() {
            let val = self.buckets[i].load(Ordering::Relaxed);
            map.insert(
                label.to_string(),
                serde_json::Value::Number(serde_json::Number::from(val)),
            );
        }
        serde_json::Value::Object(map)
    }
}

/// Process-wide counters (P3-1). All updates are `Relaxed` atomics — no locks on the hot path.
#[derive(Default)]
pub struct Metrics {
    pub fuse_ops: Align64<ProbabilisticAtomic>,
    /// D1.b watchdog (design-metadata-throughput §9): in-flight ops
    /// observed past `SQUEEZEFS_TIMEOUT` by the watchdog scan — one count
    /// per scan per overdue op (was: silent per-op ETIMEDOUT synthesis).
    /// > 0 ⇒ investigate.
    pub fuse_op_watchdog_overdue: Align64<AtomicU64>,
    /// D1.d (design-metadata-throughput §5.1, PR M5): FLUSH requests on a
    /// never-dirtied handle served by the fast path — no lease acquire, no
    /// buffer scan. On kernels that honor `FOPEN_NOFLUSH` (D2.a) the FLUSH
    /// round trip itself disappears and this stays ~0; growth here means
    /// the kernel still sends FLUSH and D1.d is absorbing its cost.
    pub fuse_flush_clean_fastpath: Align64<AtomicU64>,
    /// D1.d: RELEASE requests on a never-dirtied handle that skipped the
    /// lease acquire + background flush spawn (bookkeeping still runs:
    /// lease/lock teardown, open-count, reclaim queue).
    pub fuse_release_clean_fastpath: Align64<AtomicU64>,
    /// D2.b: lookup misses answered with a cacheable negative entry
    /// (nodeid 0 + entry TTL) instead of a bare ENOENT — each is a
    /// kernel-side negative dentry that absorbs repeated-miss round trips.
    pub fuse_lookup_negative_replies: Align64<AtomicU64>,
    /// D2.c: post-op attr-cache refreshes (unlink/rename/link family) —
    /// parent/child attrs re-seeded from the RAM-authoritative backend
    /// instead of invalidated, so the kernel's forced revalidation GETATTR
    /// is a ~µs cache hit rather than a contended backend fetch.
    pub fuse_attr_cache_refreshes: Align64<AtomicU64>,
    pub meta_updates: Align64<AtomicU64>,
    pub put_obj: Align64<AtomicU64>,
    pub get_obj: Align64<AtomicU64>,
    pub del_obj: Align64<AtomicU64>,
    pub cache_hits: Align64<AtomicU64>,
    pub cache_misses: Align64<AtomicU64>,
    /// Striped serves whose block-index→key binding (or device-fill
    /// incarnation) moved between resolution and fetch and were re-resolved
    /// instead of served (the reused-key stale-fill family, `8e3995e`
    /// follow-up). Each increment is an averted wrong-block serve — the live
    /// detector for the free→reallocate ABA window.
    pub stale_binding_rebinds: Align64<AtomicU64>,
    /// Single-flight waiters served directly from their cohort's carried
    /// `FillResult` (R1a, docs/design-read-path.md §5.2) — the adoption
    /// signal that waiter correctness is publish-independent. Replaces the
    /// old probe-after-publish tier-recheck hits for cohort members.
    pub singleflight_waiter_result_serves: Align64<AtomicU64>,
    /// R4 hot-block RAM tier (docs/design-read-path.md §5.4): hits are the
    /// warm-read adoption signal for the > 256 KiB block population;
    /// misses count device-validated fills entering probation; evictions
    /// count victims leaving the clock ring; probation_drops counts
    /// never-read probation victims DROPPED by the dehydration gate —
    /// stays 0 until PR 4 flips the protected-only policy.
    pub hot_block_hits: Align64<AtomicU64>,
    pub hot_block_misses: Align64<AtomicU64>,
    pub hot_block_evictions: Align64<AtomicU64>,
    pub hot_block_probation_drops: Align64<AtomicU64>,
    /// Protected hot-tier victims dropped at the dehydration channel mouth
    /// because their bytes were already NVMe-tier-resident (duplicate-write
    /// dedupe, §5.3/§5.5).
    pub hot_block_dehydrate_skips: Align64<AtomicU64>,
    /// R3 ranged reads (docs/design-read-path.md §5.6): adoption counter —
    /// sub-block device reads served by `get_block_range_for_index`.
    pub ranged_reads: Align64<AtomicU64>,
    /// R3 amplification numerator: DEVICE bytes read by ranged ops (window
    /// bytes, ≥ the requested bytes only by 4 KiB rounding). Compare
    /// against user bytes for the random-row amplification bound.
    pub ranged_read_bytes: Align64<AtomicU64>,
    /// Ranged requests whose 4 KiB window rounding widened the request
    /// (unaligned edges) — ≫ 0 on O_DIRECT means the LBA assumption is
    /// wrong for the workload (§5.6 open-question follow-up trigger).
    pub ranged_read_unaligned_bounces: Align64<AtomicU64>,
    /// Ranged serves that hit binding/incarnation movement and re-resolved
    /// (the 074-family discipline on the ranged path).
    pub ranged_read_rebinds: Align64<AtomicU64>,
    /// Hybrid I/O (user directive 2026-07-15): ranged second touches whose
    /// ghost evidence escalated to one whole-block fetch + admission —
    /// rand-4k re-read heat converging to RAM. ≈ 0 on a re-read-heavy
    /// random workload means the evidence-based admission regressed;
    /// growing on a pure one-pass scan means the ghost window is
    /// misclassifying (collisions — check `read_tier_admission_ghost_hits`).
    pub ranged_read_ghost_escalations: Align64<AtomicU64>,
    /// Scan-resistant admission governor (2026-07-26; the beyond-budget
    /// random-read collapse): `evicted_unhit` — ghost-admitted hot-tier
    /// victims evicted without EVER serving a reader (the
    /// `prefetch_evicted_unconsumed` sibling for admissions; sustained
    /// growth = the cache is not earning and the clamp should be engaged);
    /// `wasted_bytes` — cumulative payback shortfall of admitted victims
    /// (waste ÷ device read bytes is the bounded-waste verdict, target ≤
    /// a few percent under churn); `governor_denials` — escalations
    /// refused by the clamp (each one stayed a device-true ranged window
    /// read; ≈ 0 on fitting working sets by construction).
    pub read_admission_evicted_unhit: Align64<AtomicU64>,
    pub read_admission_wasted_bytes: Align64<AtomicU64>,
    pub read_admission_governor_denials: Align64<AtomicU64>,
    /// R5 (§5.7 Yellow row): dehydration-worker victims dropped because the
    /// memory authority paused dehydration entirely (protected included —
    /// disk-tier warmth is the cheapest sacrifice under memory pressure).
    pub mem_budget_dehydrate_paused: Align64<AtomicU64>,
    /// R5 finding-#2 escalation: disk-tier publishes skipped at the
    /// `cache_read_block` funnel while the authority's unreclaimable arm
    /// rides the Red band (never-lossy — read-cache absence).
    pub read_tier_publishes_paused: Align64<AtomicU64>,
    /// R5 finding-#2 escalation: writers that entered the Red parked-buffer
    /// admission gate (awaited the never-lossy drain instead of parking
    /// past the halved cap). The pre-fix advisory-soft cap let the parked
    /// set balloon to 1,937 buffers / 7.6 GiB anon under 16-writer rand-4k.
    pub parked_gate_waits: Align64<AtomicU64>,
    /// Gated writers that escalated to a durable SELF-FLUSH of their own
    /// block (the fsync staging-refusal escalation) after the drain-assist
    /// window — the block never parks, RAM frees immediately, and no
    /// writer ever stalls behind another inode's drain convoy (leg B
    /// measured 36 ten-second deadline stalls before this escalation).
    pub parked_gate_self_flushes: Align64<AtomicU64>,
    /// Self-flush FAILURES that parked past the cap anyway (loud — the
    /// never-lossy last resort when the backend refuses the upload).
    pub parked_gate_timeouts: Align64<AtomicU64>,
    /// Staged-file RMW seeds served through the bounded `BUFFER_POOL`
    /// (follow-up C): the whole-image read_staged seed recycles instead of
    /// mallocing ~4 MiB per sub-block write — the allocation flood behind
    /// the aged-daemon cage kills (dhat: ~21 GB churn / 30 k-op storm).
    pub staged_rmw_pooled_seeds: Align64<AtomicU64>,
    /// Item B (overwrite lazy-RMW seed): existing-data blocks checked out
    /// with the seed read DEFERRED.
    pub overwrite_seed_deferred: Align64<AtomicU64>,
    /// Deferred seeds that were SKIPPED forever — accumulation fully
    /// covered the block before any exit (the row-4 win: one elided device
    /// block read each).
    pub overwrite_seed_skipped: Align64<AtomicU64>,
    /// Deferred seeds materialized at an escape point (stage/upload exits,
    /// sparse read; RW3b deleted the write-path gap/partial-trigger sites)
    /// — same cost as the old eager seed, paid only when actually needed.
    pub overwrite_seed_materialized: Align64<AtomicU64>,
    /// Truncate-shrinks of a staged blob applied as an IN-PLACE ring header
    /// patch (never a re-stage that ring pressure can refuse) — the fix for
    /// the aged-fsx stale-resurrection corruption
    /// (`tests/staged_truncate_stale_tests.rs`).
    pub staged_truncate_inplace_shrinks: Align64<AtomicU64>,
    /// Truncate-shrinks that clip-rewrote a promoted/spilled durable staged
    /// image (`block_map[0]`) longer than the new size — the ring-miss leg
    /// of the same corruption class.
    pub staged_truncate_durable_clips: Align64<AtomicU64>,
    /// R1b admission (docs/design-read-path.md §5.3): skipped ≈ streamed
    /// cold blocks (the tax kill's adoption signal — ≈ 0 on a streaming
    /// workload means the classifier/admission is broken); admissions ≈
    /// re-read blocks reaching the disk tier; ghost_hits = second-touch
    /// detections; streams_classified / odirect_requests are the
    /// classifier inputs made observable.
    pub read_fill_publishes_skipped: Align64<AtomicU64>,
    pub read_tier_admissions: Align64<AtomicU64>,
    pub read_tier_admission_ghost_hits: Align64<AtomicU64>,
    pub read_streams_classified: Align64<AtomicU64>,
    pub read_odirect_requests: Align64<AtomicU64>,
    /// Hybrid I/O (user directive 2026-07-15): O_DIRECT requests served
    /// from the single-block RAM/NVMe tier fast paths (the 416–492k IOPS
    /// class signature — ≈ 0 on a warm O_DIRECT workload means the hybrid
    /// serve side regressed); O_DIRECT-initiated ranged second touches
    /// whose ghost evidence escalated to a whole-block admission; and
    /// device-true diagnostic reads (`-o direct_device_true` /
    /// `SQUEEZEFS_DIRECT_DEVICE_TRUE` — adoption signal for the escape:
    /// > 0 on a mount that should be hybrid means the escape is armed).
    pub read_odirect_tier_serves: Align64<AtomicU64>,
    pub read_odirect_ghost_admits: Align64<AtomicU64>,
    pub read_device_true_reads: Align64<AtomicU64>,
    /// R2 pipeline (docs/design-read-path.md §5.5): issued/completed/
    /// wasted account every prefetch task (wasted >> 0 = abandonment or
    /// mis-detection); inflight_bytes is the live gauge; window_hwm the
    /// growth witness; foreground_waits drive window growth and should
    /// decay in steady state; evicted_unconsumed is THE refetch-spiral
    /// detector (AIMD collapse trigger); active_streams exposes the
    /// contention-scaling denominator (two-epoch activity gauge).
    pub prefetch_issued: Align64<AtomicU64>,
    pub prefetch_completed: Align64<AtomicU64>,
    pub prefetch_wasted: Align64<AtomicU64>,
    pub prefetch_inflight_bytes: Align64<AtomicU64>,
    pub prefetch_window_hwm: Align64<AtomicU64>,
    pub prefetch_foreground_waits: Align64<AtomicU64>,
    pub prefetch_evicted_unconsumed: Align64<AtomicU64>,
    pub prefetch_active_streams: Align64<AtomicU64>,
    /// Layout mix (write path outcomes).
    pub layout_inline_writes: Align64<AtomicU64>,
    pub layout_staged_writes: Align64<AtomicU64>,
    pub layout_striped_writes: Align64<AtomicU64>,
    /// FIND-RW5-A: never-lossy StorageFull escalations — a staged
    /// whole-image/fold/clone arm found the staging ring unable to admit
    /// its image and degraded to the durable direct-block spill instead of
    /// surfacing StorageFull to the caller. One count per escalation, all
    /// arms (staged replace, rider fold, clone). Growth under sustained
    /// pressure is the DESIGNED degraded mode; a user-visible EIO of the
    /// StorageFull class is a regression.
    pub staged_spill_escalations: Align64<AtomicU64>,
    /// FIND-RW5-A forensics: double-release of an already-free device
    /// offset (the release_superseded double-free family). MUST STAY 0 —
    /// growth means an offset can be minted to two live owners.
    pub block_double_frees: Align64<AtomicU64>,
    /// FIND-RW5-A face 6 containment: a steady-state free of a refcount-
    /// UNTRACKED offset was REFUSED instead of released. At steady state
    /// every legitimately freeable offset is tracked (allocation seeds the
    /// count; the mount recovery walk seeds every live reference), so an
    /// untracked free is the second half of a double-release — refusing it
    /// is what makes the two-live-owner mint impossible. Growth is loud
    /// evidence of a double-release lineage upstream (leak-safe: the block
    /// leaks until fsck C6, it is never handed to two owners).
    pub block_untracked_free_refusals: Align64<AtomicU64>,
    /// FIND-RW4-A store-raw escape: block images whose compressed form
    /// would not shrink (incompressible payloads) stored RAW behind the
    /// frame's raw marker — compression is best-effort per block, never a
    /// frame that cannot fit its allocator chunk. Growth ≈ the
    /// incompressible share of the write mix; a compressible workload
    /// must keep this ~0 (the escape must not disable compression).
    pub compress_stored_raw: Align64<AtomicU64>,
    /// Best-effort background admission (see `bg_admit`).
    pub bg_spawn_admitted: Align64<AtomicU64>,
    pub bg_spawn_rejected: Align64<AtomicU64>,
    /// io_uring request queue backpressure (submit rejected as full).
    pub uring_queue_full: Align64<AtomicU64>,
    /// Block writes that missed `write_block`'s zero-copy `WriteData::Aligned`
    /// DMA branch and paid the bounce-buffer copy (zero-copy write-path design
    /// §5.6, PR 2 — the pooled-buffer alignment-contract violation detector).
    /// Pooled sources are 4 KiB-aligned by construction, so on aligned
    /// workloads this must stay 0; non-4 KiB-multiple payloads
    /// (compressed/encrypted output, tail blocks) are the only legitimate
    /// contributors.
    pub nvme_unaligned_write_fallbacks: Align64<AtomicU64>,
    /// DLM lease acquire outcomes (coarse lock-wait signal).
    pub lease_acquire_ok: Align64<AtomicU64>,
    pub lease_acquire_fail: Align64<AtomicU64>,
    /// Writeback path: durable flush hard failures (sticky).
    pub writeback_retry_exhaustions: Align64<AtomicU64>,
    /// Writeback units resolved as SUPERSEDED no-ops (staged stamp no
    /// longer matches the unit's token: a newer write re-staged the block
    /// and owns its custody chain). The healthy churn outcome — the
    /// FIND-M11-A contract that stale units resolve instead of livelock.
    /// §5.1.2 reserved-xattr screen refusals (get/set/remove on a
    /// reserved internal name through FUSE) — tamper-attempt tripwire.
    pub fuse_reserved_xattr_refusals: Align64<AtomicU64>,
    /// Job-fabric counters (design-volume-lifecycle §10, PR VL2).
    pub job_submitted: Align64<AtomicU64>,
    pub job_completed: Align64<AtomicU64>,
    pub job_cancelled: Align64<AtomicU64>,
    pub job_failed: Align64<AtomicU64>,
    pub job_tasks_done: Align64<AtomicU64>,
    pub job_checkpoint_writes: Align64<AtomicU64>,
    /// Worker copy-buffer bytes (the R5 `job_copy_buffers` gauge,
    /// charged by the VL4 movers' copy window).
    pub job_copy_buffer_bytes: Align64<AtomicU64>,
    /// Jobs paused by the R5 shed hook.
    pub job_paused_mem_pressure: Align64<AtomicU64>,
    /// PR VL9 mover-serialization pin: mover-class jobs held Queued
    /// behind a running mover with an intersecting volume scope (one
    /// mover-class job per volume at a time; counted once per deferral
    /// episode — KD-6 idempotence makes queueing safe).
    pub job_serialized_waits: Align64<AtomicU64>,
    /// Drains self-paused by the §5.2 checkpoint-time capacity
    /// re-verification (state `paused-capacity`) instead of running the
    /// survivors to StorageFull.
    pub job_paused_capacity: Align64<AtomicU64>,
    /// VL4 evacuation family (design-volume-lifecycle §10). Distinct
    /// victim blocks moved (a clone-shared block counts ONCE — the
    /// move-once law).
    pub evacuate_blocks_moved: Align64<AtomicU64>,
    /// Stored bytes copied by the mover — THE drain-engagement
    /// instrument (§3: a drain row is INVALID unless this accounts for
    /// the victim's used bytes).
    pub evacuate_bytes_moved: Align64<AtomicU64>,
    /// Moved blocks whose refcount was > 1 (clone-shared; refcount
    /// transferred to the destination pre-publish).
    pub evacuate_shared_blocks_moved: Align64<AtomicU64>,
    /// Copy-window bytes in flight (gauge; bounded by 64 blocks/worker —
    /// the §5.2 `transient` term's live counterpart).
    pub evacuate_inflight_bytes: Align64<AtomicU64>,
    /// Mover publishes superseded by a concurrent foreground write
    /// (FIND-M11-A applied to movers: contractual no-ops, re-plan
    /// revisits).
    pub evacuate_stale_token_noops: Align64<AtomicU64>,
    /// Blocks deferred by the quiescent-first rule (§5.4 step 3: live
    /// active buffers / parked extents / spilled records).
    pub evacuate_deferred_staged_blocks: Align64<AtomicU64>,
    /// Census re-plan passes (KD-6: convergence is by idempotent plan
    /// regeneration).
    pub evacuate_replans: Align64<AtomicU64>,
    /// The §5.2 preflight terms of the ACTIVE drain (gauges; 0 when no
    /// drain runs) — `volume list` mirrors them so the operator sees the
    /// same math preflight ran.
    pub evacuate_needed_bytes: Align64<AtomicU64>,
    pub evacuate_avail_bytes: Align64<AtomicU64>,
    pub evacuate_transient_bytes: Align64<AtomicU64>,
    /// Volume-lifecycle preflight refusals (design-volume-lifecycle §10,
    /// PR VL3): online `volume add-data` / state-change requests refused
    /// by validation (bad device, duplicate member, meta volume, probe
    /// failure). Offline verbs refuse in the CLI process and do not
    /// count here.
    pub volume_preflight_refusals: Align64<AtomicU64>,
    /// §5.9 placement-table rebuilds (design-volume-lifecycle §10, PR
    /// VL4b): the health-worker cadence + registration/state-change/
    /// override/retire hooks + the empty-band degenerate rebuild. Never
    /// a per-write cost — steady-state picks leave this flat (pinned in
    /// tests/placement_tests.rs).
    pub placement_table_refreshes: Align64<AtomicU64>,
    /// PR VL6a `fsck_*` family (design-volume-lifecycle §10, §5.6):
    /// cumulative across runs on this daemon. **`fsck_findings` must be
    /// 0 on a healthy volume — the tripwire.**
    pub fsck_inodes_scanned: Align64<AtomicU64>,
    /// Tree pages walked by the C1 integrity walk (one per leaf-range
    /// fetch).
    pub fsck_nodes_walked: Align64<AtomicU64>,
    pub fsck_blocks_checked: Align64<AtomicU64>,
    pub fsck_refcounts_checked: Align64<AtomicU64>,
    /// Scan-phase violations queued for verification (a high count with
    /// a matching `fsck_suspects_cleared` is expected and healthy
    /// online).
    pub fsck_suspects: Align64<AtomicU64>,
    pub fsck_suspects_cleared: Align64<AtomicU64>,
    /// C2/C3 suspects exempted by the allocation-epoch side map
    /// (allocated younger than the scan epoch).
    pub fsck_epoch_exempted: Align64<AtomicU64>,
    /// C2/C3 suspects exempted by the in-flight allocation registry
    /// (live writeback/flush/parked/mover owners).
    pub fsck_inflight_exempted: Align64<AtomicU64>,
    /// C3 suspects exempted by the §5.4-step-2 mover pre-publish ledger.
    pub fsck_mover_ledger_exempted: Align64<AtomicU64>,
    /// Verified findings (class-labeled C1–C7 in the report). MUST stay
    /// 0 on healthy volumes.
    pub fsck_findings: Align64<AtomicU64>,
    /// Wall seconds of the most recent run (gauge).
    pub fsck_scan_secs: Align64<AtomicU64>,
    /// C7 scrub family (KD-17). `scrub_readability_only` is the honesty
    /// gauge: plain blocks that could only be READ, not verified (OQ-B).
    pub scrub_blocks_scanned: Align64<AtomicU64>,
    pub scrub_bytes_scanned: Align64<AtomicU64>,
    pub scrub_aead_verified: Align64<AtomicU64>,
    pub scrub_frame_verified: Align64<AtomicU64>,
    pub scrub_readability_only: Align64<AtomicU64>,
    pub scrub_failures: Align64<AtomicU64>,
    /// PR VL6b repair family (design-volume-lifecycle §10, §5.6a):
    /// planned = per-finding actions the planner emitted (dry runs
    /// included); applied = actions executed under `--apply`; refused =
    /// verify-before-repair refusals (a stale/healed finding is never
    /// acted on — a growing count under repeated repairs of the same
    /// report is the designed outcome, never an error).
    pub fsck_repairs_planned: Align64<AtomicU64>,
    pub fsck_repairs_applied: Align64<AtomicU64>,
    pub fsck_repairs_refused: Align64<AtomicU64>,
    /// Quarantine-first accounting: records (meta/custody images +
    /// move-aside files), blocks (device-block byte copies), and total
    /// bytes copied into the per-run quarantine before anything was
    /// discarded.
    pub fsck_quarantined_records: Align64<AtomicU64>,
    pub fsck_quarantined_blocks: Align64<AtomicU64>,
    pub fsck_quarantined_bytes: Align64<AtomicU64>,
    /// Applied repairs per class — index 0..6 ⇔ C1..C7
    /// (`fsck_repair_classC{1..7}` on the stats inode).
    pub fsck_repair_class: Align64<[AtomicU64; 7]>,
    /// PR VL7 defrag family (design-volume-lifecycle §5.7/§10, KD-11).
    /// Distinct blocks moved by the D1/D2 defrag objective (the same
    /// `move_one` engine as `evacuate_*` — a defrag move counts BOTH
    /// families; this one is the defrag-engagement instrument).
    pub defrag_blocks_moved: Align64<AtomicU64>,
    pub defrag_bytes_moved: Align64<AtomicU64>,
    /// D3 fold kicks that actually ran a fold pass (drives the EXISTING
    /// `fold_*` machinery — `fold_passes` moves in lockstep).
    pub defrag_folds_kicked: Align64<AtomicU64>,
    /// D4 leaf compactions nudged through the SMO serialization
    /// (`meta_kv_node_compactions` moves in lockstep).
    pub defrag_meta_compactions_kicked: Align64<AtomicU64>,
    /// The four KD-11 axis gauges. Ratios are permille+1 encoded
    /// (`crate::defrag::{encode_ratio,decode_ratio}`; raw 0 = never
    /// measured — the stats JSON emits `null` then). Worst-volume
    /// semantics for the per-volume axes (the actionable tripwire);
    /// per-volume rows ride the `--report-only` JSON.
    pub frag_d1_contiguity: Align64<AtomicU64>,
    pub frag_d1_reclaimable_tail: Align64<AtomicU64>,
    pub frag_d2_locality: Align64<AtomicU64>,
    /// Plain bytes (parked overlay bytes + spilled `active_block_ext:`
    /// record bytes) — not ratio-encoded.
    pub frag_d3_pressure_bytes: Align64<AtomicU64>,
    pub frag_d4_dead_bset_ratio: Align64<AtomicU64>,
    /// §5.1.6 remote-wire family (design-volume-lifecycle §10, PR VL2b).
    /// Currently-enrolled remote workers (gauge).
    pub job_remote_workers: Align64<AtomicU64>,
    /// Successful worker enrollments (HMAC-verified).
    pub job_remote_enrollments: Align64<AtomicU64>,
    /// Refused enrollments (bad HMAC / wire_schema / undecodable hello)
    /// — a security tripwire.
    pub job_remote_enroll_refused: Align64<AtomicU64>,
    /// Shards assigned to remote workers — the remote-engagement
    /// instrument (§10: a "distributed" run's row is INVALID unless
    /// this accounts for the remote share).
    pub job_remote_shards: Align64<AtomicU64>,
    /// Verified-and-published remote result submissions.
    pub job_remote_submissions: Align64<AtomicU64>,
    /// Result submissions refused for stale/unknown shard_fencing — the
    /// fencing tripwire (an expired holder's late submit).
    pub job_remote_refused_stale: Align64<AtomicU64>,
    /// Shard leases expired past the TTL (missed heartbeats).
    pub job_remote_lease_expiries: Align64<AtomicU64>,
    /// Expired shards returned to the queue (reassignment — any
    /// population may pick them up, always with fresh destinations).
    pub job_remote_reassignments: Align64<AtomicU64>,
    /// Destination bytes verified-and-published from remote submissions.
    pub job_remote_bytes_moved: Align64<AtomicU64>,
    /// Bytes the coordinator verify-read before publishing (Issue-30:
    /// == bytes_moved on plaintext transports, sampled under TLS).
    pub job_remote_verify_read_bytes: Align64<AtomicU64>,
    /// Do-not-publish quarantined destination tuples (gauge) — an
    /// expired lease's destinations, never reused within the job.
    pub job_remote_quarantined_destinations: Align64<AtomicU64>,
    /// NVMe PR preemptions of expired worker-host registrations under
    /// the coordinator's WERO hold (per namespace preempted).
    pub job_remote_pr_preempts: Align64<AtomicU64>,
    /// Guarantee-class gauge: 1 = `pr` (coordinator holds WERO on every
    /// data namespace), 0 = `deferred-reclaim` (exported as the string).
    pub job_remote_fence_mode: Align64<AtomicU64>,
    pub writeback_superseded_noops: Align64<AtomicU64>,
    /// `FencingTokenExpired` failures that reached the retry ladder — a
    /// TRANSIENT generation-bump race post-FIND-M11-A (the merge
    /// credential is read fresh per attempt; superseded units no-op inside
    /// the flush unit). Sustained growth on a quiet mount = the
    /// constant-writeback livelock regressing.
    pub writeback_stale_token_retries: Align64<AtomicU64>,
    /// Orphan staged custody discarded by the flush unit after VERIFYING
    /// the inode record is gone (unlink+reclaim raced the flush; monotonic
    /// inos never come back) — the recovery contract's "missing inode meta
    /// discards orphan active blocks", applied live instead of the
    /// NotFound retry spin (FIND-M11-A's second face).
    pub writeback_orphan_discards: Align64<AtomicU64>,
    /// Copy-on-write duplications of an active-block accumulation buffer
    /// forced by a live reader snapshot (zero-copy write-path design §5.2).
    /// Sequential streams never pay this; spikes mean read/write contention
    /// on the same dirty block — the price of snapshot immutability, made
    /// observable.
    pub active_block_cow_copies: Align64<AtomicU64>,
    /// Content-complete blocks uploaded directly (crypto → allocate → DMA →
    /// block-map merge), bypassing the staging mmap + writeback round-trip
    /// (zero-copy write-path design §5.3, PR 4). Should ≈ the striped
    /// sequential write volume on healthy mounts.
    pub write_through_blocks: Align64<AtomicU64>,
    /// Bytes moved by write-through uploads (`write_through_blocks` ×
    /// block_size for the default shape).
    pub write_through_bytes: Align64<AtomicU64>,
    /// Write-throughs that degraded into the never-lossy staging fallback
    /// (device write failure / allocator failure / uring backpressure).
    /// ~0 on healthy mounts; sustained growth = device backpressure.
    pub write_through_fallbacks: Align64<AtomicU64>,
    // Terminal-free device reclaim economy (the shim-write-amplification
    // fix — `.benchmarks/2026-07-27-shim-write-amplification.md`;
    // classification `routing::free_reclaim_op`, contract
    // `tests/block_free_reclaim_tests.rs`). A field row's device-byte
    // delta reconciles against these: data writes ≈ `write_through_bytes`
    // family; reclaims live HERE and must never appear as device WRITE
    // bandwidth (BLKDISCARD on namespaces, PUNCH_HOLE on file backings).
    /// Terminal frees reclaimed via `BLKDISCARD` (block-device backing).
    pub block_free_discards: Align64<AtomicU64>,
    /// Bytes deallocated by `block_free_discards`.
    pub block_free_discard_bytes: Align64<AtomicU64>,
    /// Terminal frees reclaimed via `fallocate(PUNCH_HOLE)` (regular-file
    /// backing — host-FS sparse reclaim, no device I/O).
    pub block_free_file_punches: Align64<AtomicU64>,
    /// Bytes deallocated by `block_free_file_punches`.
    pub block_free_punch_bytes: Align64<AtomicU64>,
    /// Terminal frees whose reclaim was refused/unsupported (discard
    /// errno, non-file-non-bdev backing). Safe to skip — freed ranges are
    /// never read (hole semantics + incarnation seqlock) — and NEVER
    /// retried as a zeroing write; sustained growth on a thin-provisioned
    /// substrate means space is not being returned to it.
    pub block_free_reclaim_skipped: Align64<AtomicU64>,
    // Async block-reclaim (the overwrite-throughput fix —
    // `.benchmarks/2026-07-27-async-block-reclaim.md`; queue in
    // `crate::block_reclaim`, contract `tests/async_block_reclaim_tests.rs`):
    // terminal frees QUEUE their device reclaim; the punch/discard
    // counters above still account every displaced block exactly once,
    // now from the background worker.
    /// Terminal-free reclaims enqueued to the background reclaimer.
    pub block_free_reclaim_queued: Align64<AtomicU64>,
    /// GAUGE: bytes queued-or-in-flight in the background reclaimer.
    /// Returns to baseline when the queue is drained (unmount, valve).
    pub block_free_reclaim_queue_bytes: Align64<AtomicU64>,
    /// Background reclaim batches processed (adjacent ranges coalesce
    /// into fewer device commands inside a batch; counting stays
    /// per-block on the punch/discard counters).
    pub block_free_reclaim_batches: Align64<AtomicU64>,
    /// ENOSPC pressure-valve drains: an allocation that would refuse for
    /// space forced a synchronous drain of the queued reclaims first.
    /// **0 except under real space pressure** — growth on a non-full
    /// volume means the background reclaimer is not keeping up.
    pub block_free_reclaim_sync_drains: Align64<AtomicU64>,
    /// Reclaim entries dropped-without-finish_free because the writer
    /// guard fenced / the volume fail-stopped (the D0 `failed` latch —
    /// contract 6): a fenced zombie must never issue destructive device
    /// commands, so the entries' space return passes to the successor
    /// writer's recovery (un-returned thin space, re-covered on reuse —
    /// the kill-9 posture). **0 on healthy mounts**; any growth means
    /// this daemon was fenced — always investigate alongside
    /// `writer_guard_fenced`.
    pub block_free_reclaim_fence_halts: Align64<AtomicU64>,
    /// Seed-time memset bytes elided by §5.3 coverage tracking: for every
    /// Fresh accumulation buffer reaching content-validity, the block size
    /// minus the complement bytes actually zeroed. Sequential fills elide
    /// the whole block.
    pub active_block_memset_elided_bytes: Align64<AtomicU64>,
    /// Out-of-order written-coverage runs recorded (RW3b): a write landed
    /// disjoint from every existing run of its accumulation buffer — the
    /// kernel-split / `FOPEN_PARALLEL_DIRECT_WRITES` reorder signature
    /// (unaligned-buffer O_DIRECT writers) and legitimately-sparse fills.
    /// In-order streams keep it at 0; the coverage-based write-through
    /// trigger makes it perf-neutral (FIND-L1-A fix).
    pub active_block_ooo_runs: Align64<AtomicU64>,
    /// Meta-volume durability barriers actually issued (real `fdatasync` calls).
    /// A single FUSE fsync should raise this by exactly one (no redundant barrier).
    pub meta_device_syncs: Align64<AtomicU64>,
    /// Meta-volume barrier *requests* (callers of `sync_device_for_ino`). Under
    /// group commit `meta_sync_requests - meta_device_syncs` is the work saved by
    /// coalescing concurrent fsyncs into shared barriers.
    pub meta_sync_requests: Align64<AtomicU64>,
    /// Histograms for lock wait times and queue depths.
    pub write_lock_wait: Align64<LatencyHistogram>,
    pub block_lock_wait: Align64<LatencyHistogram>,
    pub lease_lock_wait: Align64<LatencyHistogram>,
    pub dlm_acquire_time: Align64<LatencyHistogram>,
    pub writeback_queue_depth: Align64<QueueDepthHistogram>,
    /// Deferred-flusher device barriers issued (timer path), vs
    /// strict/fsync barriers which land in `meta_device_syncs` directly.
    pub meta_flush_deferred: Align64<AtomicU64>,
    /// Reclaim group-commit fill: doomed inos per batched `destroy_inodes`
    /// transaction (design §4.5) — headroom before `SQUEEZEFS_RECLAIM_BATCH`
    /// needs raising.
    pub meta_reclaim_batch_size: Align64<QueueDepthHistogram>,
    /// D4.a fill-vs-window attribution (design-metadata-throughput §5.4):
    /// inos per gathered reclaim batch at the `drain_reclaim_batch` edge
    /// (post-dedup, pre-admission — pairs with `meta_reclaim_batch_size`,
    /// which records the post-admission destroy fill). Distinguishes "the
    /// gather window degenerates to singletons" from "admission thins the
    /// batch".
    pub meta_reclaim_gather_fill: Align64<QueueDepthHistogram>,
    /// Gather batches closed by hitting `SQUEEZEFS_RECLAIM_BATCH` (cap) —
    /// the healthy storm outcome (fill == cap).
    pub meta_reclaim_gather_cap_closes: Align64<AtomicU64>,
    /// Gather batches closed by `SQUEEZEFS_RECLAIM_BATCH_WINDOW_MS` expiry
    /// with the channel still open — the trickle outcome. A FORGET storm
    /// landing here with tiny fills is the batch-fill degeneration D4.c
    /// hunts (per-FORGET spawn jitter / window-vs-arrival cadence).
    pub meta_reclaim_gather_window_closes: Align64<AtomicU64>,
    /// Gather batches closed because the reclaim channel closed (unmount /
    /// teardown drain).
    pub meta_reclaim_gather_channel_closes: Align64<AtomicU64>,
    /// Staging dirs whose content was discarded at init because it was
    /// stamped by a DEAD filesystem generation (or predated generation
    /// stamping) — the reformat-over-stale-staging guard (`cache::nvme::
    /// bind_staging_generation`). One increment per discarded dir; exactly
    /// once per dir after a reformat, 0 on every warm restart.
    pub staging_generation_discards: Align64<AtomicU64>,
    /// PR VL5b (§5.5.2a): mutating routed-meta ops parked at a CLOSED
    /// per-slot cutover gate (before any 4a lock — planned, bounded
    /// parks; NEVER escalated to `disabled_volumes`). One increment per
    /// park event.
    pub meta_slot_gate_parked_commits: Align64<AtomicU64>,
    /// PR VL5b (§5.5.2): slot migrations completed (flip + teardown).
    pub meta_slot_migrations: Align64<AtomicU64>,
    /// PR VL5b: records bulk-copied by slot migrations (engagement
    /// instrument for migration rows).
    pub meta_slot_records_copied: Align64<AtomicU64>,
    /// PR VL5b: delta keys captured by the conveyor pass-task tee and
    /// re-read at apply.
    pub meta_slot_delta_keys: Align64<AtomicU64>,
    /// PR VL5b: side-log overflows (each flips the round to a fresh full
    /// snapshot; three consecutive abort the migration loud).
    pub meta_slot_delta_overflows: Align64<AtomicU64>,
    /// PR VL5b: the widest cutover window (gate close → reopen) in
    /// milliseconds — the G-VL-4 p99-window instrument (max gauge).
    pub meta_slot_cutover_ms_max: Align64<AtomicU64>,
    /// KD-8: staging drain barriers run before meta set changes (each =
    /// verify-custody-empty + generation restamp).
    pub staging_drain_barriers: Align64<AtomicU64>,
    /// Reads of a `staged` file whose payload is GONE — no staging-ring
    /// entry (crash-torn → discarded by segment index recovery, or lost
    /// before a kill) and no promoted mapping. Served as size-consistent
    /// zeros per the D0 staging degrade contract (acked-unfsynced staged
    /// data MAY be lost, must never error). Nonzero after a crash remount
    /// = data loss happened and was degraded, not errored.
    pub staged_payload_lost_reads: Align64<AtomicU64>,
    /// Staged reads that lost a race with an identity transition (re-stage /
    /// promotion / spill / layout flip) and re-resolved the fresh identity
    /// instead of serving zeros or freed bytes (the fstests 074/127/616
    /// transient-zeros family). A live health signal, not an error: bounded
    /// retries that converge. Sustained growth without churn = investigate.
    pub staged_identity_retries: Align64<AtomicU64>,
    // -----------------------------------------------------------------
    // RW1 rand-write device-byte ledger (docs/design-random-small-writes.md
    // §1.2 point 3–4 buckets — the corrected leg drivers). Always-on
    // counters (one relaxed fetch_add on 4 MiB-class paths): the G-RW2 red
    // gate reconciles per-op device bytes FROM these, keyed to the drivers
    // — the inline victim spill, the R5-pressure parked drain, the Red
    // parked-gate self-flush, and same-key re-stage churn — NEVER to the
    // writeback queue (§1.2 Issue-4 correction: `enqueue_writeback` is
    // fsync/dismount/fallback-driven and does not fire on the loss shape).
    // -----------------------------------------------------------------
    /// Bucket 1 read leg: deferred-seed materializations (`fetch_seed_image`
    /// = one whole-block device read) forced by the INLINE VICTIM SPILL in
    /// `insert_active_block_buffer` — the foreground writer paying a
    /// victim's RMW seed.
    pub spill_seed_reads: Align64<AtomicU64>,
    pub spill_seed_read_bytes: Align64<AtomicU64>,
    /// Bucket 1 write leg: victim images written to local staging by the
    /// inline spill (admitted `put_active_block` bytes).
    pub spill_staging_puts: Align64<AtomicU64>,
    pub spill_staging_put_bytes: Align64<AtomicU64>,
    /// Seed materializations forced at the flush/stage exits (parked drain,
    /// fsync/close flush, teardown, Red parked-gate self-flush) — the
    /// non-spill share of the read leg.
    pub flush_seed_read_bytes: Align64<AtomicU64>,
    /// Seed materializations inside `write_file_staged` itself. **Must
    /// stay 0 since RW3b** (the `patch_edge_rmw_reads` tripwire pattern):
    /// the coverage-based write-through trigger removed both in-path seed
    /// sites (the gap-materialize and the partial-coverage trigger misfire
    /// — FIND-L1-A's two write-path faces); any growth means an inline
    /// seed fetch crept back into the merge path.
    pub write_path_seed_read_bytes: Align64<AtomicU64>,
    /// Bucket 2 write leg: staging puts by driver. `drain` = the
    /// R5-pressure parked drain (`drain_parked_toward`); `flush` = the
    /// fsync/FLUSH/close family; `teardown` = dismount force-flush;
    /// `wt_fallback` = write-through never-lossy staging fallback.
    pub staging_put_bytes_drain: Align64<AtomicU64>,
    pub staging_put_bytes_flush: Align64<AtomicU64>,
    pub staging_put_bytes_teardown: Align64<AtomicU64>,
    pub staging_put_bytes_wt_fallback: Align64<AtomicU64>,
    /// Writeback-queue admissions by driver (the §1.2 attribution honesty
    /// check: on the pure O_DIRECT rand shape the foreground write path
    /// enqueues NOTHING — `wt_fallback` stays 0 and the queued durable
    /// uploads trace to the drain/flush drivers).
    pub writeback_enqueued_drain: Align64<AtomicU64>,
    pub writeback_enqueued_flush: Align64<AtomicU64>,
    pub writeback_enqueued_teardown: Align64<AtomicU64>,
    pub writeback_enqueued_wt_fallback: Align64<AtomicU64>,
    /// Durable-upload write leg by driver: `writeback` = the per-block
    /// flush unit (`flush_one_active_block` — queued units AND fsync/
    /// queue-full sweeps); `self_flush` = the Red parked-gate self-flush
    /// (bucket 3); `escalation` = staging-refusal escalations at the
    /// flush/teardown exits.
    pub durable_upload_bytes_writeback: Align64<AtomicU64>,
    pub durable_upload_bytes_self_flush: Align64<AtomicU64>,
    pub durable_upload_bytes_escalation: Align64<AtomicU64>,
    /// Bucket 4: same-key re-stage churn — a block revisit's checkout
    /// removing the staged sibling whose bytes an earlier spill already
    /// paid for (re-park + re-spill follows). `removes` counts staged
    /// siblings actually present at checkout; `bytes` their staged size.
    pub restage_churn_removes: Align64<AtomicU64>,
    pub restage_churn_bytes: Align64<AtomicU64>,
    /// Block revisits at checkout (an overlay — parked buffer or staged
    /// sibling — already owned the block): quantifies the §1.2 block-revisit
    /// discount (+12–26 % above the 12 MiB/op model on the scoreboard row).
    pub write_block_revisits: Align64<AtomicU64>,
    /// H1 evidence: the per-write staged-sibling `spawn_blocking` remove
    /// hop, counted per striped-write checkout (fires whether or not
    /// anything is staged — the pure-overhead face). The RW1 cost pin
    /// asserts probes == striped block writes; RW3's lock-free probe flips
    /// that pin when it elides the hop.
    pub staging_sibling_probes: Align64<AtomicU64>,
    /// H3 evidence: `ALIGNED_BUF_POOL`/`RANGED_BUF_POOL` handouts that
    /// missed the recycle queue and paid the mmap/page-fault allocation
    /// path.
    pub aligned_pool_misses: Align64<AtomicU64>,
    /// Companion hit counter (2026-07-25 ipc-miss-path fix): pooled
    /// handouts served from the recycle queue — the miss RATIO is the
    /// convoy instrument (misses/(hits+misses) ≈ 0.4 was the 6 ms/op
    /// ring-read collapse; ≈ 0 is the healthy posture).
    pub aligned_pool_hits: Align64<AtomicU64>,
    // -----------------------------------------------------------------
    // RW2 W1 sole-owner extent patch (docs/design-random-small-writes.md
    // §5.1/§5.4). `patch_writes`/`patch_write_bytes` count in-place
    // sub-block DMAs (device cost per op: ONE LBA-aligned write, zero
    // reads, zero meta, zero staging); the `patch_ineligible_*` family is
    // the predicate's decision ledger — every striped small-write that
    // does NOT patch counts exactly one bucket (its FIRST failing
    // predicate, in the documented order), so
    // Σ(patch_writes + patch_ineligible_*) reconciles against striped
    // write_file_staged invocations. Regression semantics (§9):
    // `patch_ineligible_*` growing on a shape that should patch =
    // predicate rot; `patch_edge_rmw_reads` > 0 = alignment/predicate
    // regression BY DEFINITION (v1 is aligned-only — the counter exists
    // for the phase-2 edge path and is a G-RW2 gate clause at 0).
    // -----------------------------------------------------------------
    /// Sole-owner in-place patches (one aligned sub-block DMA each).
    pub patch_writes: Align64<AtomicU64>,
    /// User bytes delivered by patches (== Σ patched lengths).
    pub patch_write_bytes: Align64<AtomicU64>,
    /// Phase-2 unaligned-edge RMW seed reads. **Must stay 0 in v1**
    /// (aligned-only): any growth is an alignment/predicate regression
    /// (G-RW2 gate clause).
    pub patch_edge_rmw_reads: Align64<AtomicU64>,
    /// Predicate 1 failures: target block unmapped (hole / not striped /
    /// no inline map entry — indirect-mapped files fall here too).
    pub patch_ineligible_unmapped: Align64<AtomicU64>,
    /// Predicate 1 failures: decorated `bk:off:len` mapping (promoted
    /// staged / spill / clip publishes) — patching must never scribble
    /// relative to a decorated window.
    pub patch_ineligible_decorated: Align64<AtomicU64>,
    /// Predicate 5 failures: offset or length not 4096-LBA-aligned
    /// (v1 disposition — today's accumulation path).
    pub patch_ineligible_unaligned: Align64<AtomicU64>,
    /// Predicate 2 failures: a RAM `ActiveBlockBuf` or staged
    /// `active_block:` entry owns the block (accumulation in progress —
    /// merge into it, today's path).
    pub patch_ineligible_overlay: Align64<AtomicU64>,
    /// Predicate 4 failures: block refcount != 1 (clone-shared — CoW).
    pub patch_ineligible_shared: Align64<AtomicU64>,
    /// Predicate 3 failures: compressed/encrypted volume (a transform
    /// image cannot be patched in place).
    pub patch_ineligible_transform: Align64<AtomicU64>,
    /// Predicate 6 failures: stream-adjacent (offset == the ino's
    /// previous write end) — sequential streams keep the whole-block
    /// write-through economy.
    pub patch_ineligible_adjacent: Align64<AtomicU64>,
    /// Predicate 5 size/window failures: length > `SQUEEZEFS_PATCH_MAX_BYTES`,
    /// the request spans blocks, or the write EXTENDS the file (i_size
    /// must grow ⇒ a meta commit is owed ⇒ ineligible) — the size/window
    /// class, one bucket by design (§5.4 family list).
    pub patch_ineligible_oversize: Align64<AtomicU64>,
    /// Patch DMA failures (EIO surfaced to exactly this write; tiers
    /// purged + incarnation re-stabilized — nothing acked, nothing lost).
    pub patch_dma_errors: Align64<AtomicU64>,

    // -----------------------------------------------------------------
    // W2 — extent-granular overlay / spill records / batched fold
    // (design-random-small-writes §5.2 / §5.4, PR RW4). The
    // patch-INELIGIBLE small-write shapes (compressed/encrypted volumes,
    // shared/decorated/hole/unaligned blocks) park compactly, spill as
    // 4 KiB-class staged extent records, and RMW exactly once per fold.
    // -----------------------------------------------------------------
    /// RAM bytes parked in extent-overlay payload slabs (RAII-gauged by
    /// [`crate::cache::active_block::ActiveBlockBuf`]) — the R5 authority
    /// component of the same name, sheddable via the parked drain (fold).
    pub parked_extent_bytes: Align64<AtomicU64>,
    /// RAM bytes parked in full-repr `ActiveBlockBuf` block backings
    /// (block-size class; RAII-gauged). Together with
    /// `parked_extent_bytes` this is the parked-write BYTE budget the old
    /// 256-COUNT cap became (§5.2 — the inline-spill convoy for the small
    /// shape dies with the count cap).
    pub parked_full_buffer_bytes: Align64<AtomicU64>,
    /// Small non-adjacent writes parked as extent-overlay runs (a 4 KiB
    /// write parks ~4 KiB, not a 4 MiB-class deferred buffer).
    pub extent_parks: Align64<AtomicU64>,
    /// Extent overlays escalated to full buffers (coverage ≥ 25 % of the
    /// block or a large merge).
    pub extent_escalations: Align64<AtomicU64>,
    /// Escalations taken IMPLICITLY by a legacy full-image call site
    /// reaching an extent-repr buffer (defensive correctness arm). The
    /// designed park/spill/fold routes never take it — growth here means
    /// a route regression, not corruption.
    pub extent_implicit_escalations: Align64<AtomicU64>,
    /// Extent overlays spilled as staged `active_block_ext:` records
    /// (**zero seed reads at spill, ever** — pinned by `get_obj` +
    /// `spill_seed_reads` deltas staying 0 across extent spills).
    pub extent_spills: Align64<AtomicU64>,
    /// Payload bytes carried by extent-record spills (4 KiB-class puts vs
    /// the retired seed-materialize + 4 MiB image put).
    pub extent_spill_bytes: Align64<AtomicU64>,
    /// Staged extent records absorbed back into a RAM overlay/buffer at
    /// checkout (custody moves staging → RAM, the one-authority law).
    pub extent_record_absorbs: Align64<AtomicU64>,
    /// Per-block folds completed: seed ONCE (item B's binding-validated
    /// `fetch_seed_image`), apply all k parked+staged extents, one durable
    /// upload — amp ≈ 2048/k + spill legs (§4).
    pub fold_passes: Align64<AtomicU64>,
    /// Old-block seed reads paid by folds (≈ `fold_passes` for
    /// data-backed blocks; 0 for hole-backed folds — never per extent).
    pub fold_seed_reads: Align64<AtomicU64>,
    /// Σ extents applied across folds (`fold_fill` mean = this ÷
    /// `fold_passes`; the G-RW6 amortization gauge — median ≥ 16 gates).
    pub fold_extents_folded: Align64<AtomicU64>,
    /// `fold_fill` histogram: extents applied per fold (k).
    pub fold_fill: Align64<QueueDepthHistogram>,
    /// Staged-layout rider (§5.2): sub-image staged-file overwrites
    /// captured as extent records instead of whole-image RMW re-stages.
    pub staged_rider_extent_writes: Align64<AtomicU64>,
    /// Rider records folded back into their staged image (fsync /
    /// extending write / promotion / teardown).
    pub staged_rider_folds: Align64<AtomicU64>,
    /// Extent records found at mount (orphans — a clean shutdown drains
    /// every record to fold, so ANY record here is kill-9-class residue;
    /// loud stderr line per sweep, the `bind_staging_generation` loudness
    /// class) that were RECOVERED (generation-bound, fencing-current).
    pub extent_records_recovered: Align64<AtomicU64>,
    /// Recovered-at-mount records DISCARDED for a stale fencing token
    /// (the remount law: "stale fencing tokens discard staged work").
    pub extent_records_stale_discarded: Align64<AtomicU64>,
    /// Extent-record blobs that failed structural validation (torn write /
    /// foreign bytes / checksum mismatch): detected-and-ignored loudly.
    pub extent_records_torn_discarded: Align64<AtomicU64>,
    /// Extent records naming a FUTURE record version: refused as units,
    /// loudly, and LEFT IN PLACE (acked custody of a newer binary — the
    /// §5.2 forward-only fence; the dir-level marker normally refuses the
    /// whole mount first, so growth here means a mixed-version dir).
    pub extent_records_future_refused: Align64<AtomicU64>,
    /// PR 6 / N6 (design-nvmeof-target-management §6.9): gauge —
    /// fabric-attached (`transport != pcie`) NVMe controllers under
    /// `/sys/class/nvme` at the last sampler beat. Zero on boxes with no
    /// fabric (including no sysfs root at all) — never an error.
    pub fabric_controllers: Align64<AtomicU64>,
    /// §6.9 gauge — sampled fabric controllers in any non-`live` state
    /// (`connecting`/`resetting`/…): the reconnect-storm detector.
    pub fabric_ctrl_not_live: Align64<AtomicU64>,
    /// §6.9 — a **sampled-transition counter, not a kernel counter**:
    /// sysfs exposes only instantaneous controller state (there is NO
    /// native cumulative reconnect count), so this counts *observed*
    /// not-live→live transitions per controller identity — the
    /// renumbering-stable `(transport, subsysnqn, address)` tuple — at
    /// the 10 s stats cadence and **undercounts flaps faster than the
    /// cadence** (a bounce that fits entirely between two beats counts
    /// zero). Acceptable for the storm detector it exists to be
    /// (measured storms run at 10 s cadence for ~10 min); do NOT try to
    /// "fix" it against a kernel counter that does not exist. Caveat
    /// pinned by
    /// `test_fabric_reconnects_is_sampled_transition_counter_undercounts_bursts`.
    pub fabric_ctrl_reconnects: Align64<AtomicU64>,
    /// L4 interception session host (design-preload-interception §8, PR
    /// L4-3 families). Gauge: live sessions — leaks show as
    /// active ≫ expected.
    pub ipc_sessions_active: Align64<AtomicU64>,
    /// Sessions ever established.
    pub ipc_sessions_total: Align64<AtomicU64>,
    /// Successful per-fd binds.
    pub ipc_binds: Align64<AtomicU64>,
    /// The §5.2 daemon-side refusal ledger, by class. `version` growth =
    /// mixed fleet (join with `build_commit`); `nonce` = stale/replayed
    /// HELLO; `flags` = fd type/status screen (O_PATH, non-regular,
    /// O_APPEND/O_SYNC/O_DSYNC/O_TMPFILE-class); `mode` = wrong-`st_dev`
    /// (not a capability on this mount / mount unresolved); `budget` =
    /// admission (arena cap, per-uid cap, shed); `peercred` = claimed
    /// identity contradicted SO_PEERCRED (defense-in-depth tripwire).
    pub ipc_bind_refused_version: Align64<AtomicU64>,
    pub ipc_bind_refused_nonce: Align64<AtomicU64>,
    pub ipc_bind_refused_flags: Align64<AtomicU64>,
    pub ipc_bind_refused_mode: Align64<AtomicU64>,
    pub ipc_bind_refused_budget: Align64<AtomicU64>,
    pub ipc_bind_refused_peercred: Align64<AtomicU64>,
    /// Data-plane HELLOs refused because the mount is control-plane-only
    /// (no `-o interception`) — the expected posture on default mounts
    /// with preloaded apps probing; counted apart from the security
    /// classes above so their signals stay clean.
    pub ipc_bind_refused_disabled: Align64<AtomicU64>,
    /// Degenerate (`unknown`/`-dirty`) identity pairs admitted via
    /// `SQUEEZEFS_IPC_ALLOW_DEV` — nonzero outside dev boxes is a
    /// fleet-hygiene alarm (KD-7).
    pub ipc_binds_dev_override: Align64<AtomicU64>,
    /// R5 admission refusals (arena budget / per-uid caps / shed target)
    /// — the budget-class refusals, counted on the R5 surface too.
    pub ipc_admission_refusals: Align64<AtomicU64>,
    /// Live session-shm bytes (the `ipc_session_arenas` mem-budget
    /// component gauge).
    pub ipc_arena_bytes: Align64<AtomicU64>,
    /// Malformed/forged ring descriptors completed `-EINVAL` and
    /// wrong-direction ops completed `-EBADF` — **must stay 0 in
    /// production**: nonzero = client bug or attack (loud log per site).
    pub ipc_descriptor_rejects: Align64<AtomicU64>,
    /// Sessions poisoned for protocol violations (§5.3 rule 4 / §5.7) —
    /// **must stay 0 in production**; one loud log line per poison.
    pub ipc_sessions_poisoned: Align64<AtomicU64>,
    /// L4 data plane (design-preload-interception §8, PR L4-4 families).
    /// Ring reads/writes SERVED (fast path + handoff; rejects excluded) —
    /// the §3 charter-rule-4 engagement instrument: an interception
    /// scoreboard row is INVALID unless these ≈ the row's ops.
    pub ipc_ops_read: Align64<AtomicU64>,
    pub ipc_ops_write: Align64<AtomicU64>,
    /// Payload bytes accepted from ring writes (severed at dequeue).
    pub ipc_bytes_in: Align64<AtomicU64>,
    /// Payload bytes returned to ring reads.
    pub ipc_bytes_out: Align64<AtomicU64>,
    /// Reads completed synchronously on the service thread (the §5.5.1
    /// fast path incl. the EOF short-circuit) — the 1 M+ engine gauge.
    pub ipc_fast_path_serves: Align64<AtomicU64>,
    /// Ops packaged onto the tokio runtime (read demotions + all v1
    /// writes). For writes this increments AFTER the §5.5.2 severance.
    pub ipc_async_handoffs: Align64<AtomicU64>,
    /// Fast-path `try_read()` contention demotions — growth on read-only
    /// workloads = unexpected writers (§5.5.1 counter split).
    pub ipc_fast_path_lock_demotions: Align64<AtomicU64>,
    /// Fast-path in-guard cache-miss demotions (attr/metadata/buffer) —
    /// growth on warm workloads = fast-path rot.
    pub ipc_fast_path_miss_demotions: Align64<AtomicU64>,
    /// Gauge: pinned IPC service threads (`SQUEEZEFS_IPC_SERVICE_THREADS`).
    pub ipc_service_threads: Align64<AtomicU64>,
    /// Owned-session doorbell parks taken by service threads (the
    /// bounded `futex_waitv` waits — idle no-session parks are not
    /// counted). Growth ≈ op rate on a busy stream = the spin window no
    /// longer covers the per-session inter-arrival gap (the 2026-07-26
    /// sessions-inversion engine: a park/wake cycle per burst).
    pub ipc_service_parks: Align64<AtomicU64>,
    /// L4-6 lifecycle families (§5.7 / §5.6.2 W1). Idle sessions torn
    /// down past `SQUEEZEFS_IPC_IDLE_SECS` — growth on busy clients =
    /// idle-clock regression.
    pub ipc_sessions_reaped: Align64<AtomicU64>,
    /// W1 inode invalidations pushed to the kernel (bind + rate-limited
    /// first ring write per (ino, window)).
    pub ipc_inval_notifies: Align64<AtomicU64>,
    /// W1 invalidations suppressed by the per-ino rate window — the
    /// write-storm economy gauge (notifies ≫ suppressed on rand-write
    /// workloads = window regression).
    pub ipc_inval_suppressed: Align64<AtomicU64>,
    /// DIALED P1 direct-drive families (design-preload-interception
    /// §5.5/§12 fallback shape; `perf/ipc-direct-drive`): governed ranged
    /// ring reads the service thread submitted DIRECTLY on the ipc-host-
    /// owned io_uring (no task, no tokio, no handler).
    pub ipc_direct_drive_submits: Align64<AtomicU64>,
    /// Direct-drive submits whose CQE revalidated (795 custody snapshot)
    /// and completed the ring slot. `serves + fallbacks_post == submits`.
    pub ipc_direct_drive_serves: Align64<AtomicU64>,
    /// Direct-drive ops that DMA'd via the 64 KiB ranged bounce pool
    /// (LBA-unaligned request window, or an arena destination that cannot
    /// take O_DIRECT-class DMA) instead of straight into the arena.
    pub ipc_direct_drive_bounces: Align64<AtomicU64>,
    /// Direct-drive CQEs that failed post-DMA revalidation (custody
    /// moved / binding changed / device error) and fell back to the
    /// handler path — races, ≈ 0 on quiet read workloads.
    pub ipc_direct_drive_fallbacks_post: Align64<AtomicU64>,
    /// The prelude decision ledger (the W1 `patch_ineligible_*` pattern):
    /// growth on a shape that should direct-drive = prelude rot.
    /// `shape` = len outside 4–64 KiB / multi-block / EOF-crossing.
    pub ipc_direct_ineligible_shape: Align64<AtomicU64>,
    /// `meta` = no RAM-resident metadata / non-striped layout / block map
    /// not RAM-resident (the honest map-resident-majority split).
    pub ipc_direct_ineligible_meta: Align64<AtomicU64>,
    /// `layout` = hole block (no binding) or decorated (`bk:off:len`)
    /// mapping — not a whole-block window read.
    pub ipc_direct_ineligible_layout: Align64<AtomicU64>,
    /// `overlay` = live RAM overlay / staged sibling / staged extent
    /// record on the block, or an unstable fill incarnation — correctness
    /// owns ambiguity; the handler serves acked custody.
    pub ipc_direct_ineligible_overlay: Align64<AtomicU64>,
    /// `backend` = unhealthy/unknown volume, ring unavailable, or a
    /// volume not registered with the direct-drive uring.
    pub ipc_direct_ineligible_backend: Align64<AtomicU64>,
    /// `policy` (DIALED P1.5, 2026-07-27): the DEFAULT-mount admission
    /// policy routed a governed-shape O_DIRECT miss to the handler on
    /// purpose — a non-`second-touch` admission mode, a §5.6
    /// ranged-dispatch exclusion (threshold / stream-classified file),
    /// or a GRANT-shaped escalation candidate (the admission fetch +
    /// publish machinery rides the handler). NOT prelude rot — the
    /// deliberate-routing sibling of the `ineligible_*` refusal classes
    /// (docs/design-read-path.md §Observability).
    pub ipc_direct_ineligible_policy: Align64<AtomicU64>,
}

pub static METRICS: Lazy<Metrics> = Lazy::new(Metrics::default);
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct ClientInfo {
    pub client_id: String,
    pub hostname: String,
    pub pid: u32,
    pub mountpoint: String,
    pub mounted_at: u64,
    pub last_heartbeat: u64,
    pub stats: ClientStats,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct ClientStats {
    pub fuse_ops: u64,
    pub meta_updates: u64,
    pub put_obj: u64,
    pub get_obj: u64,
    pub del_obj: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
}

/// Finish a virtual-inode payload (`.stats` / `.config` JSON): the exact
/// pretty-printed JSON plus ONE final newline — no tail padding. The
/// historical constant-size floor padding (`pad_virtual_payload`, retired
/// 2026-07-22 on user report: `cat .config` printed a screenful of
/// whitespace) papered over the kernel's stale-`i_size` copy bound with
/// up-to-256-KiB whitespace tails. Exact sizes are kept coherent by the
/// snapshot protocol instead: OPEN pins the generation per fh AND
/// publishes its size, GETATTR never regenerates once published, and
/// both virtual inodes reply with ZERO attr/entry TTLs so every fstat
/// reaches the daemon and reports the pinned generation's exact size
/// (`FOPEN_DIRECT_IO` already exempts plain reads). Single-reader `cat`
/// is exact by construction; concurrent readers race last-open-wins
/// (bounded, documented residual — see the OPEN pin point). Pinned by
/// `metrics_tests::stats_snapshot_getattr_size_matches_served_bytes_under_churn`
/// and `phantom_backend0_tests::test_config_payload_exact_size_no_tail_padding`.
fn finish_virtual_payload(mut s: String) -> String {
    s.push('\n');
    s
}

#[cold]
#[inline(never)]
fn map_squeezefs_err(e: SqueezefsError) -> Errno {
    let errno = e.to_errno();
    // ENOENT is the normal grammar of POSIX lookups (negative dentries,
    // unlink/stat probes): logging it at ERROR buried real faults under
    // thousands of benign lines per bench/rsync run.
    if errno == libc::ENOENT {
        debug!("Squeezefs operational error: {:?}", e);
    } else {
        error!("Squeezefs operational error: {:?}", e);
    }
    Errno::from(errno)
}

#[inline]
/// Inode times are **i64 nanoseconds carried in the u64 storage word**
/// (two's complement): every positive (post-1970) value reads
/// identically to the historical unsigned interpretation, and pre-epoch
/// timestamps survive instead of wrapping (fstests generic/258, VL10
/// release gate; pinned in
/// `tests/attr_refresh_tests.rs::negative_timestamps_round_trip`).
/// `div_euclid`/`rem_euclid` keep the nsec field non-negative for
/// negative totals (`-0.5 s` = sec −1, nsec 500e6 — the kernel
/// `timespec64` convention).
fn as_timestamp(ns: u64) -> Timestamp {
    let ns = ns as i64;
    Timestamp::new(
        ns.div_euclid(1_000_000_000),
        ns.rem_euclid(1_000_000_000) as u32,
    )
}

/// The inverse of [`as_timestamp`]: a FUSE `Timestamp` (possibly
/// pre-epoch) into the i64-in-u64 nanosecond storage word, saturating at
/// the i64 range (± year 2262/1677 — beyond it the kernel's own
/// `timespec64` ns math saturates the same way).
fn timestamp_to_ns_word(t: Timestamp) -> u64 {
    t.sec
        .saturating_mul(1_000_000_000)
        .saturating_add(t.nsec as i64) as u64
}

/// D1.d (design-metadata-throughput §5.1, PR M5): per-inode open-handle
/// state — the open count plus the dirty bit data-mutating ops set
/// (write / truncate / fallocate / copy_file_range dest). FLUSH/RELEASE
/// consult the bit to elide lease acquisition, buffer scans and the
/// per-close background flush spawn on never-dirtied handles.
///
/// The bit is per-INODE (regular-file handles are minted as `fh == ino`),
/// which is exact for the create-storm shape (one handle per file) and
/// conservative for multi-handle opens: any dirty handle keeps every
/// handle of that inode on the full path until the open count returns to
/// zero. `add_open` resets the bit on the 0→1 transition — safe because a
/// dirty close already handed its unflushed state to the background
/// flush path (and fsync remains the durable barrier), so a later clean
/// open-close pair has nothing new to flush.
///
/// `dirty` is atomic so the write hot path marks it through a SHARED map
/// guard (no dashmap shard write lock on the data path — the zero-copy /
/// latch-free rule).
#[derive(Debug, Default)]
pub struct OpenEntry {
    count: usize,
    dirty: std::sync::atomic::AtomicBool,
}

/// Max automatic retries for a single background writeback unit (P0-3).
const WRITEBACK_MAX_ATTEMPTS: u32 = 4;
/// P1-3: max partial blocks held only in RAM (not yet staged).
const MAX_ACTIVE_BLOCK_BUFFERS: usize = 256;

/// Kernel ABI (include/uapi/linux/fuse.h, fuse ≥ 7.38 / Linux ≥ 6.2):
/// open-reply flag that lets the kernel take the inode lock SHARED instead
/// of EXCLUSIVE for non-extending O_DIRECT writes on this open. Older
/// kernels ignore unknown open flags, so advertising it is always safe.
const FOPEN_PARALLEL_DIRECT_WRITES: u32 = 1 << 6;

/// Kernel ABI (include/uapi/linux/fuse.h, fuse ≥ 7.35 / Linux ≥ 5.16):
/// open-reply flag telling the kernel to elide the FLUSH request on close
/// of this handle — D2.a (design-metadata-throughput §5.2, PR M5).
///
/// **Scope, verified against the running kernel's own header and a live
/// probe (M5 acceptance)**: the kernel honors this bit only WITHOUT
/// writeback cache ("don't flush data cache on close (unless
/// FUSE_WRITEBACK_CACHE)") — so it covers `--no-writeback` mounts, while
/// the default writeback-cache config gets its −1.0 FLUSH round trip from
/// the **clean-handle ENOSYS latch** in `flush` (`fc->no_flush`, the
/// standard FUSE optional-op protocol honored by every kernel line).
///
/// Semantics review (the design's §5.2 D2.a argument, pinned by
/// `tests/write_visibility_tests.rs`):
/// - SqueezeFS FLUSH is already a **soft** flush (see `flush`: "sync_all/
///   fsync is the durable barrier"), so no durability contract weakens.
/// - Close-to-open visibility across *mounts* is moot under the M1
///   single-writer guard; within one mount, read-your-writes rides the
///   write path's own visibility machinery.
/// - Handles that dirty after open lose nothing: the kernel still writes
///   back dirty pages at close (`fuse_flush` runs `write_inode_now` and
///   reports filemap errors BEFORE its no_flush/NOFLUSH cuts), and dirty
///   handles keep their close-time daemon flush via RELEASE's background
///   path + fsync.
///
/// Older kernels ignore unknown open flags, so advertising is always safe
/// (the FOPEN_PARALLEL_DIRECT_WRITES precedent); the INIT probe in `init`
/// logs the kernel's capability split.
const FOPEN_NOFLUSH: u32 = 1 << 5;

/// Open/create reply flags for REGULAR files (the virtual .stats/.config
/// opens reply `FOPEN_DIRECT_IO` separately — see `open`). Parallel direct
/// writes are safe under this daemon's lock model: the write handler
/// serializes per inode via `active_inode_locks` for meta-prep and
/// per-block `BLOCK_FLUSH_LOCKS` for data merges (lock order P1), so
/// kernel-parallel submission cannot reorder a block's merges; extending
/// writes stay kernel-exclusive regardless (fuse_dio_lock's past-EOF
/// check), preserving size-extension ordering. NOFLUSH rides every
/// regular open/create reply — every handle is OPENED clean (D2.a).
const fn regular_open_reply_flags() -> u32 {
    FOPEN_NOFLUSH | FOPEN_PARALLEL_DIRECT_WRITES
}

#[derive(Debug, Clone)]
pub struct WritebackRequest {
    pub ino: u64,
    pub block_idx: u32,
    pub fencing_token: u64,
    /// 0-based attempt count; re-queued failures increment this.
    pub attempts: u32,
}

/// Aggregated result of the dismount active-block force-flush: one report
/// per unmount, never a log line per block.
#[derive(Debug, Default)]
pub struct TeardownFlushSummary {
    pub attempted: usize,
    pub flushed: usize,
    pub failed: usize,
    /// First few failure reasons (bounded) for the aggregated log line.
    pub error_samples: Vec<String>,
}

/// Outcome of the IPC read fast-path's guarded probe
/// ([`SqueezefsFilesystem::ipc_read_probe_locked`], §5.5.1 of the L4
/// design): the caller (the IPC service thread, holding the inode
/// `try_read` guard) serves `Eof`/`Hit` synchronously and demotes `Miss`
/// to the async handoff — guard dropped first (the
/// drop-guard-before-enqueue rule).
/// Why the direct-drive prelude refused an op (the decision ledger
/// classes — see the `ipc_direct_ineligible_*` counters).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcDirectIneligible {
    /// len outside the 4–64 KiB device class, multi-block, or
    /// EOF-crossing (the in-bounds contract is strict).
    Shape,
    /// No RAM-resident metadata, non-striped layout, or block map not
    /// RAM-resident (the honest map-resident-majority split).
    Meta,
    /// Hole block (no binding) or decorated (`bk:off:len`) mapping.
    Layout,
    /// Live RAM overlay / staged sibling / staged extent record on the
    /// block, or an unstable fill incarnation — correctness owns
    /// ambiguity.
    Overlay,
    /// Unhealthy/unknown backend volume, or one not registered with the
    /// direct-drive uring.
    Backend,
    /// DIALED P1.5 (default-mount direct-drive): the admission policy
    /// deliberately routed this governed-shape op to the handler —
    /// non-`second-touch` admission mode, §5.6 ranged-dispatch exclusion
    /// (threshold / stream-classified file), or a GRANT-shaped escalation
    /// candidate (the admission fetch + publish stays on the handler).
    Policy,
}

/// The direct-drive prelude's 795 custody snapshot: everything the CQE
/// revalidation needs to prove no custody transfer crossed the DMA
/// window. Captured before submit, checked at completion.
#[derive(Debug, Clone)]
pub struct IpcDirectSnapshot {
    pub ino: u64,
    pub block: u32,
    /// The RAM-authoritative durable binding (whole-block, undecorated).
    pub key: String,
    /// `BLOCK_CUSTODY_EPOCHS` word at probe time (bumped by every
    /// overlay/sibling/record retire — the 795 seqlock).
    pub epoch: u64,
    /// Whether the key is allocator-incarnation-tracked.
    pub tracked: bool,
    /// Pre-read fill-incarnation snapshot (tracked keys only).
    pub incarnation: Option<u64>,
    pub offset: u64,
    pub len: u32,
    /// Probe-time overlay/staging keys, carried so the CQE-side
    /// revalidation is allocation-free (the reaper thread is the
    /// deep-qd throughput governor — 3 string builds/op measured as
    /// part of its 100 %-CPU saturation at t32qd32).
    pub cache_key: String,
    pub ext_key: String,
}

pub enum IpcReadProbe {
    /// `offset ≥ size` under the guarded size authority: complete 0 bytes.
    Eof,
    /// Sync-servable bytes (active-buffer covered hit) — a CoW-stable
    /// snapshot slice, immutable for the completion's lifetime.
    Hit(bytes::Bytes),
    /// Any shape needing async work: demote (release the guard, then
    /// enqueue the handoff — never the reverse order).
    Miss,
}

pub struct SqueezefsFilesystem {
    pub router: DataRouter,
    dlm: DlmClient,
    pub meta_backend: Option<std::sync::Arc<crate::meta_backend::RoutedMetaBackend>>,
    uid: u32,
    gid: u32,
    active_leases: std::sync::Arc<dashmap::DashMap<u64, crate::dlm::LockLease, ahash::RandomState>>,
    lease_locks: std::sync::Arc<StripeLocks<tokio::sync::Mutex<()>, 4096>>,
    pub active_inode_locks: std::sync::Arc<StripeLocks<tokio::sync::RwLock<()>, 4096>>,
    /// P1-4: capacity-bounded attribute cache (moka TTL + max_capacity).
    pub attr_cache: moka::sync::Cache<u64, (FileAttr, std::time::Instant), ahash::RandomState>,
    /// §4.5 (PR K7): snapshots of directories ≤
    /// [`DIR_ENTRY_CACHE_MAX_ENTRIES`], cookie-ascending
    /// `(name, ino, §5.1 cookie, file_type)` so cache-served pages keep
    /// the resume contract bit-for-bit. Larger directories stream and
    /// never enter it.
    ///
    /// PR M4 (D1.c): keyed by `(parent, generation)` — mutators bump the
    /// parent's `dir_gen` counter instead of running a moka
    /// `invalidate` per create/unlink/rename (the measured
    /// cache-maintenance tax), and a snapshot built against a superseded
    /// generation lands under a dead key (the pre-M4 invalidate-then-
    /// insert race served such a snapshot until TTL — caught by
    /// `concurrent_readdir_during_create_is_coherent`). Dead generations
    /// age out by TTL/capacity.
    pub dir_entry_cache_v3:
        moka::sync::Cache<(u64, u64), std::sync::Arc<[(std::boxed::Box<str>, u64, u64, u32)]>>,
    /// PR M4 (D1.c): per-directory readdir-snapshot generation counters —
    /// latch-free (`scc` bucket read + one relaxed `fetch_add`), O(1),
    /// allocation-free on the mutate path for already-seen parents (one
    /// one-time entry insert per directory). Readdir snapshots key on
    /// `(parent, gen)`; every entry-set mutation (create/mknod/mkdir/
    /// symlink/link/unlink/rmdir/rename) bumps the affected parents so
    /// stale snapshots die by key mismatch, not by eager moka
    /// invalidation.
    dir_gen: std::sync::Arc<scc::HashMap<u64, std::sync::atomic::AtomicU64>>,
    pub dismount_wait: u64,
    /// Bounded writeback queue (P1-2). Full → synchronous flush of that block.
    writeback_tx: tokio::sync::mpsc::Sender<WritebackRequest>,
    writeback_rx:
        std::sync::Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<WritebackRequest>>>>,
    pub writeback_queue_cap: usize,
    pub client_id: std::sync::Arc<std::sync::Mutex<String>>,
    pub mountpoint: std::sync::Arc<std::sync::Mutex<String>>,
    pub max_background_uploads: usize,
    /// Dirty in-RAM striped block accumulation buffers. Exclusive-owner CoW
    /// values ([`crate::cache::active_block::ActiveBlockBuf`]): all mutation
    /// happens under [`BLOCK_FLUSH_LOCKS`]; readers stay lock-free
    /// (`DashMap::get` + `snapshot()`), and a live snapshot forces the next
    /// writer to copy (never mutate) that memory.
    active_block_buffers: std::sync::Arc<
        dashmap::DashMap<String, crate::cache::active_block::ActiveBlockBuf, ahash::RandomState>,
    >,
    /// O(1) lock-free gate over `active_block_buffers` occupancy: the count
    /// of live parked overlays, incremented BEFORE a park publishes and
    /// decremented AFTER a retire completes (conservative: `0` proves the
    /// map empty; a transient over-count only costs a per-block probe).
    /// The read hot path consults THIS — never `DashMap::is_empty()`/
    /// `len()`, which read-lock every shard (measured 45 % + 12 % of
    /// daemon CPU at 4k-randread saturation,
    /// .benchmarks/2026-07-25-odirect-randread-concurrency.md).
    parked_overlay_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// W1 §5.1 predicate 6 — the per-ino stream-adjacency word: the END
    /// offset of the ino's most recent striped write (one relaxed `swap`
    /// per write; latch-free `scc` map, shared across handler clones). A
    /// write whose offset equals it is stream-adjacent and routes to the
    /// accumulation path, so sequential small-block streams keep the
    /// whole-block write-through economy (G-RW3's seq rows pin it). Purely
    /// a routing heuristic: a false adjacency signal costs one
    /// accumulation-path write, never correctness.
    last_write_end: std::sync::Arc<scc::HashMap<u64, AtomicU64>>,
    /// §5.7 Red parked-buffer drain plumbing, shared across clones: the
    /// authority's shed closure AND the Red admission gate post a byte
    /// target + kick; one lazily-spawned worker runs
    /// [`Self::drain_parked_toward`]; waiters (gated writers) wake on
    /// `progress` after every inode flush.
    parked_drain_target: std::sync::Arc<std::sync::atomic::AtomicU64>,
    parked_drain_kick: std::sync::Arc<tokio::sync::Notify>,
    parked_drain_progress: std::sync::Arc<tokio::sync::Notify>,
    parked_drain_worker_started: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// W2 background fold plumbing (design-random-small-writes §5.2 fold
    /// triggers): extent overlays crossing the count/byte thresholds post
    /// `(ino, block)` hints; one lazily-spawned worker runs
    /// [`Self::fold_extent_block`]. Bounded + best-effort: a full queue
    /// drops the HINT only — the extents stay parked custody and the
    /// fsync/pressure/teardown drains fold them regardless.
    fold_tx: tokio::sync::mpsc::Sender<(u64, u32)>,
    fold_rx: std::sync::Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<(u64, u32)>>>>,
    fold_worker_started: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub open_virtual_files: dashmap::DashMap<u64, Vec<u8>, ahash::RandomState>,
    pub next_virtual_fh: std::sync::atomic::AtomicU64,
    pub latest_stats_json: arc_swap::ArcSwap<Option<std::sync::Arc<Vec<u8>>>>,
    pub latest_config_json: arc_swap::ArcSwap<Option<std::sync::Arc<Vec<u8>>>>,
    /// Byte length of the most recently PUBLISHED (lookup/first-touch) or
    /// PINNED (open) `.stats` generation — what GETATTR reports (0 = never
    /// generated). The kernel copies exactly `i_size` bytes out of a
    /// virtual file (`cat` → `copy_file_range`), so GETATTR must never
    /// regenerate-and-republish a different size than the generation an
    /// open fh serves — that clamp tears every snapshot read under counter
    /// churn (pinned by
    /// `metrics_tests::stats_snapshot_getattr_size_matches_served_bytes_under_churn`).
    /// Shared across handler clones (`Arc`): the publish point and the
    /// GETATTR reader may run on different clones.
    pub latest_stats_size: std::sync::Arc<AtomicU64>,
    /// `.config` twin of [`Self::latest_stats_size`].
    pub latest_config_size: std::sync::Arc<AtomicU64>,
    pub inodes_limit: std::sync::Arc<std::sync::OnceLock<u64>>,
    /// Formatted capacity in bytes (`FormatConfig.capacity`: the summed
    /// data-backend size, or the lower explicit `--capacity` quota) — the
    /// statfs `f_blocks` source. Set once at FUSE init from the format
    /// config, shared across clones like `inodes_limit`.
    pub capacity_limit: std::sync::Arc<std::sync::OnceLock<u64>>,
    /// Shared across every `SqueezefsFilesystem` clone. `start_mount` publishes
    /// the live `FuseConnection` here *after* `session.mount(fs.clone(), …)` has
    /// already consumed the clone the request handlers run on, so the cell must
    /// be shared (`Arc`): a per-clone `ArcSwap` would leave the handler's clone
    /// permanently seeing `None`, silently disabling the read zero-copy payload
    /// destination (`get_payload_buffer` in `read`).
    /// The kernel notify handle (FUSE_NOTIFY_* over the classical reply
    /// path), armed at mount like `session_connection`. Used by daemon-
    /// initiated attribute changes the kernel cannot see — currently the
    /// generic/683 setid strip (an attrs-only INVAL_INODE so the next
    /// stat refetches instead of serving the pre-strip mode for a TTL).
    pub kernel_notify: std::sync::Arc<arc_swap::ArcSwap<Option<fuse3::notify::Notify>>>,
    pub session_connection: std::sync::Arc<
        arc_swap::ArcSwap<Option<std::sync::Arc<fuse3::raw::connection::FuseConnection>>>,
    >,
    pub open_inodes: std::sync::Arc<dashmap::DashMap<u64, OpenEntry, ahash::RandomState>>,
    /// Per-class kernel cache TTLs (attr / entry / dir-entry / negative).
    /// Per-mount (DAOS per-container model): env-seeded at construction,
    /// mount-option-overridden in `start_mount`, direct-set in tests.
    pub kernel_ttls: KernelCacheTtls,
    pub reclaim_semaphore: std::sync::Arc<tokio::sync::Semaphore>,
    /// FUSE-over-io_uring surfaces Destroy once per queue; teardown must
    /// run exactly once.
    dismount_once: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// VL8 item 4: external unmount cancels the session task's `destroy`
    /// future mid-teardown (reply-task select / detached queue workers /
    /// daemon exit) — the real teardown therefore runs on a spawned task
    /// that survives the cancellation, and this pair signals its
    /// completion (`wait_dismount_teardown`) so the daemon does not exit
    /// (and tests do not proceed) before heartbeat records deregister.
    dismount_complete: std::sync::Arc<std::sync::atomic::AtomicBool>,
    dismount_done: std::sync::Arc<tokio::sync::Notify>,
    reclaim_tx: tokio::sync::mpsc::Sender<u64>,
    reclaim_rx: std::sync::Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<u64>>>>,
    /// FIND-RW5-A face 4: per-ino single-drive reclaim guard. RELEASE and
    /// FORGET both enqueue reclaims and concurrent batches both passed
    /// admission (the inode slot persists until the destroy commits), so
    /// duplicate drives ran `delete_file` twice — every mapped block freed
    /// TWICE, and a double-free interleaved with an allocation STEALS the
    /// offset from its new owner (one offset, two live layouts: permanent
    /// incarnation churn, rebind-exhaustion EIOs, cross-file corruption).
    /// An ino inserts here at admission and is removed only after its
    /// destroy + teardown completed; a failed destroy LEAVES the guard set
    /// — the slot and its blocks leak until remount (the never-lossy
    /// direction) instead of re-arming a second data teardown.
    reclaim_inflight: std::sync::Arc<scc::HashSet<u64>>,
    pub next_dir_fh: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// L4 interception session host (design-preload-interception §5.2, PR
    /// L4-3): `None` on non-interception mounts. Shared across handler
    /// clones (the `session_connection` precedent — `start_mount` arms it
    /// and the mounted handler clone must observe it); the getxattr
    /// bootstrap synthesis reads it per request.
    pub ipc_host:
        std::sync::Arc<arc_swap::ArcSwap<Option<std::sync::Arc<crate::ipc_host::IpcHost>>>>,
    /// The VL2 job fabric (populated at mount after the meta backend
    /// exists; admin verbs and the stats surface reach it here).
    pub job_fabric:
        std::sync::Arc<arc_swap::ArcSwap<Option<std::sync::Arc<crate::jobs::JobFabric>>>>,
    /// The §5.1.6 job-wire endpoint (`ip:port`) this coordinator
    /// listens on (PR VL2b) — set once at mount; the registration
    /// heartbeat publishes it as the ADDITIVE `job_endpoint` field
    /// remote workers discover the coordinator through.
    pub job_wire_endpoint: std::sync::Arc<std::sync::OnceLock<String>>,
}

impl Clone for SqueezefsFilesystem {
    fn clone(&self) -> Self {
        Self {
            router: self.router.clone(),
            dlm: self.dlm.clone(),
            meta_backend: self.meta_backend.clone(),
            uid: self.uid,
            gid: self.gid,
            active_leases: self.active_leases.clone(),
            lease_locks: self.lease_locks.clone(),
            active_inode_locks: self.active_inode_locks.clone(),
            attr_cache: self.attr_cache.clone(),
            dir_entry_cache_v3: self.dir_entry_cache_v3.clone(),
            dir_gen: self.dir_gen.clone(),
            dismount_wait: self.dismount_wait,
            writeback_tx: self.writeback_tx.clone(),
            writeback_rx: self.writeback_rx.clone(),
            writeback_queue_cap: self.writeback_queue_cap,
            client_id: self.client_id.clone(),
            mountpoint: self.mountpoint.clone(),
            max_background_uploads: self.max_background_uploads,
            active_block_buffers: self.active_block_buffers.clone(),
            parked_overlay_count: self.parked_overlay_count.clone(),
            last_write_end: self.last_write_end.clone(),
            parked_drain_target: self.parked_drain_target.clone(),
            parked_drain_kick: self.parked_drain_kick.clone(),
            parked_drain_progress: self.parked_drain_progress.clone(),
            parked_drain_worker_started: self.parked_drain_worker_started.clone(),
            fold_tx: self.fold_tx.clone(),
            fold_rx: self.fold_rx.clone(),
            fold_worker_started: self.fold_worker_started.clone(),
            open_virtual_files: self.open_virtual_files.clone(),
            next_virtual_fh: std::sync::atomic::AtomicU64::new(
                self.next_virtual_fh.load(Ordering::Relaxed),
            ),
            latest_stats_json: arc_swap::ArcSwap::new(self.latest_stats_json.load_full()),
            latest_config_json: arc_swap::ArcSwap::new(self.latest_config_json.load_full()),
            latest_stats_size: self.latest_stats_size.clone(),
            latest_config_size: self.latest_config_size.clone(),
            inodes_limit: self.inodes_limit.clone(),
            capacity_limit: self.capacity_limit.clone(),
            // Share the one cell — never split it per clone, or the mounted
            // handler clone would not observe the connection start_mount
            // publishes after mount (re-enables the read zero-copy dest).
            kernel_notify: self.kernel_notify.clone(),
            session_connection: self.session_connection.clone(),
            open_inodes: self.open_inodes.clone(),
            kernel_ttls: self.kernel_ttls,
            reclaim_semaphore: self.reclaim_semaphore.clone(),
            reclaim_inflight: self.reclaim_inflight.clone(),
            dismount_once: self.dismount_once.clone(),
            dismount_complete: self.dismount_complete.clone(),
            dismount_done: self.dismount_done.clone(),
            reclaim_tx: self.reclaim_tx.clone(),
            reclaim_rx: self.reclaim_rx.clone(),
            next_dir_fh: self.next_dir_fh.clone(),
            // Share the one cell (session_connection precedent): the
            // handler clone must see the host start_mount arms.
            ipc_host: self.ipc_host.clone(),
            job_fabric: self.job_fabric.clone(),
            job_wire_endpoint: self.job_wire_endpoint.clone(),
        }
    }
}

impl SqueezefsFilesystem {
    pub fn new(router: DataRouter, dlm: DlmClient, uid: u32, gid: u32) -> Self {
        let queue_cap = std::env::var("SQUEEZEFS_WRITEBACK_QUEUE_CAP")
            .ok()
            .and_then(|val| val.parse::<usize>().ok())
            .unwrap_or(4096);
        let reclaim_concurrency = std::env::var("SQUEEZEFS_RECLAIM_CONCURRENCY")
            .ok()
            .and_then(|val| val.parse::<usize>().ok())
            .unwrap_or_else(|| {
                // Process parallelism, not the (possibly core-pinned)
                // constructor thread's mask — the Hang-1 sizing poison.
                std::cmp::max(4, crate::cpu::process_parallelism())
            });
        info!(
            "Dynamic reclaim concurrency limit configured: {}",
            reclaim_concurrency
        );
        let (writeback_tx, writeback_rx) = tokio::sync::mpsc::channel(queue_cap);
        let (fold_tx, fold_rx) = tokio::sync::mpsc::channel(1024);
        let (reclaim_tx, reclaim_rx) = tokio::sync::mpsc::channel(100000);
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        let total_memory = sys.total_memory();
        let dir_entry_capacity = std::cmp::max(50_000, total_memory / 200_000);
        let dir_entry_cache_v3 = moka::sync::Cache::builder()
            .max_capacity(dir_entry_capacity)
            .time_to_live(Duration::from_secs(300))
            .build();
        // P1-4: bound attr cache growth (was unbounded DashMap).
        let attr_capacity = std::cmp::max(10_000, total_memory / 100_000);
        let attr_cache = moka::sync::Cache::builder()
            .max_capacity(attr_capacity)
            .time_to_live(Duration::from_secs(300))
            .build_with_hasher(ahash::RandomState::new());
        Self {
            router,
            dlm,
            meta_backend: None,
            uid,
            gid,
            active_leases: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            lease_locks: std::sync::Arc::new(StripeLocks::new()),
            active_inode_locks: std::sync::Arc::new(StripeLocks::new()),
            attr_cache,
            dir_entry_cache_v3,
            dir_gen: std::sync::Arc::new(scc::HashMap::new()),
            dismount_wait: 10,
            writeback_tx,
            writeback_rx: std::sync::Arc::new(std::sync::Mutex::new(Some(writeback_rx))),
            writeback_queue_cap: queue_cap,
            client_id: std::sync::Arc::new(std::sync::Mutex::new(String::new())),
            mountpoint: std::sync::Arc::new(std::sync::Mutex::new(String::new())),
            max_background_uploads: std::cmp::max(16, crate::cpu::process_parallelism() * 2),
            active_block_buffers: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            parked_overlay_count: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_write_end: std::sync::Arc::new(scc::HashMap::new()),
            parked_drain_target: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX)),
            parked_drain_kick: std::sync::Arc::new(tokio::sync::Notify::new()),
            parked_drain_progress: std::sync::Arc::new(tokio::sync::Notify::new()),
            parked_drain_worker_started: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
            fold_tx,
            fold_rx: std::sync::Arc::new(std::sync::Mutex::new(Some(fold_rx))),
            fold_worker_started: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            open_virtual_files: dashmap::DashMap::with_hasher(ahash::RandomState::new()),
            next_virtual_fh: std::sync::atomic::AtomicU64::new(0x1000_0000_0000_0000),
            latest_stats_json: arc_swap::ArcSwap::new(std::sync::Arc::new(None)),
            latest_config_json: arc_swap::ArcSwap::new(std::sync::Arc::new(None)),
            latest_stats_size: std::sync::Arc::new(AtomicU64::new(0)),
            latest_config_size: std::sync::Arc::new(AtomicU64::new(0)),
            inodes_limit: std::sync::Arc::new(std::sync::OnceLock::new()),
            capacity_limit: std::sync::Arc::new(std::sync::OnceLock::new()),
            kernel_notify: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(None)),
            session_connection: std::sync::Arc::new(arc_swap::ArcSwap::new(std::sync::Arc::new(
                None,
            ))),
            open_inodes: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            kernel_ttls: KernelCacheTtls::from_env(),
            reclaim_inflight: std::sync::Arc::new(scc::HashSet::new()),
            reclaim_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                reclaim_concurrency,
            )),
            dismount_once: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            dismount_complete: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            dismount_done: std::sync::Arc::new(tokio::sync::Notify::new()),
            reclaim_tx,
            reclaim_rx: std::sync::Arc::new(std::sync::Mutex::new(Some(reclaim_rx))),
            next_dir_fh: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                0x2000_0000_0000_0000,
            )),
            ipc_host: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(None)),
            job_fabric: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(None)),
            job_wire_endpoint: std::sync::Arc::new(std::sync::OnceLock::new()),
        }
    }

    pub fn max_background_uploads(&self) -> usize {
        self.max_background_uploads
    }

    pub fn add_open(&self, ino: u64) {
        let mut entry = self.open_inodes.entry(ino).or_default();
        if entry.count == 0 {
            // 0→1: a fresh open generation starts clean (any prior dirty
            // close already scheduled its background flush — see
            // [`OpenEntry`]).
            entry.dirty.store(false, Ordering::Release);
        }
        entry.count += 1;
    }

    pub fn remove_open(&self, ino: u64) {
        if let Some(mut entry) = self.open_inodes.get_mut(&ino) {
            if entry.count > 0 {
                entry.count -= 1;
            }
        }
    }

    pub fn is_open(&self, ino: u64) -> bool {
        if let Some(entry) = self.open_inodes.get(&ino) {
            entry.count > 0
        } else {
            false
        }
    }

    /// D1.d: mark this inode's open generation dirty (a data-mutating op
    /// ran). Shared map guard + atomic store — no shard write lock on the
    /// data hot path. Inodes mutated without a tracked open (defensive:
    /// the kernel should never order it that way) get an entry so the bit
    /// is never lost.
    pub fn mark_handle_dirty(&self, ino: u64) {
        if let Some(entry) = self.open_inodes.get(&ino) {
            entry.dirty.store(true, Ordering::Release);
            return;
        }
        self.open_inodes
            .entry(ino)
            .or_default()
            .dirty
            .store(true, Ordering::Release);
    }

    /// D1.d: has any data-mutating op (write / truncate / fallocate /
    /// copy_file_range dest) touched this inode since its open count last
    /// rose from zero? FLUSH/RELEASE on a never-dirtied handle take the
    /// fast path (no lease acquire, no buffer scan, no background flush
    /// spawn).
    pub fn handle_dirty(&self, ino: u64) -> bool {
        self.open_inodes
            .get(&ino)
            .map(|e| e.dirty.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    pub fn queue_reclaim_inode(&self, ino: u64) {
        if ino <= 1 || ino == CONFIG_INODE || ino == STATS_INODE {
            return;
        }
        if self.is_open(ino) {
            return;
        }
        let tx = self.reclaim_tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(ino).await;
        });
    }

    pub fn disable_background_writeback(&self) {
        let mut rx_guard = self.writeback_rx.lock().unwrap();
        let _ = rx_guard.take();
    }

    pub fn dlm(&self) -> &DlmClient {
        &self.dlm
    }

    /// The ONLINE `volume add-data` (design-volume-lifecycle §5.3, served
    /// by the admin-lane `volume-add-data` verb): validate + probe the
    /// device, build the runtime backend, stamp `KV_VOLUME_LIFECYCLE`
    /// (bit 3) on every member superblock, commit the durable record
    /// through the live meta backend (the conveyor), and only THEN enable
    /// write-path selection (§5.3 step-4 durability order: "runtime
    /// registration is completed only after the durable commit acks").
    /// Preflight refusals count `volume_preflight_refusals`.
    pub async fn admin_add_data_volume(
        &self,
        device: &str,
        no_rebalance: bool,
    ) -> std::result::Result<crate::DataVolumeRecord, String> {
        let refused = |msg: String| {
            METRICS
                .volume_preflight_refusals
                .fetch_add(1, Ordering::Relaxed);
            msg
        };
        let meta = self
            .meta_backend
            .as_ref()
            .ok_or_else(|| "no metadata backend mounted".to_string())?;
        let meta_paths: Vec<String> = meta
            .volumes
            .iter()
            .map(|v| v.device_path().display().to_string())
            .collect();

        // 2. Preflight against the durable set + the device probe — all
        //    refusals before any durable effect.
        let raw = meta
            .getxattr(1, crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR)
            .await
            .map_err(|e| format!("format config read failed: {e}"))?
            .ok_or_else(|| "format config not found on the volume set".to_string())?;
        let mut cfg: crate::FormatConfig =
            serde_json::from_slice(&raw).map_err(|e| format!("format config undecodable: {e}"))?;
        let mut records = cfg.resolved_data_volumes();
        let capacity = crate::config_ops::validate_new_data_volume(device, &records, &meta_paths)
            .map_err(|e| refused(e.to_string()))?;
        crate::config_ops::probe_data_volume_rw(device)
            .await
            .map_err(|e| refused(e.to_string()))?;

        // 3. Build the runtime backend — placement NOT enabled yet.
        let record = crate::DataVolumeRecord {
            id: crate::new_data_volume_id(),
            backing_dev: device.to_string(),
            state: crate::VOL_STATE_ACTIVE.to_string(),
            added_ts: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        };
        let backend = self
            .router
            .backend_router
            .build_backend(
                &record.id,
                &record.backing_dev,
                self.router.dlm.meta_client().clone(),
            )
            .await
            .map_err(|e| format!("backend construction failed: {e}"))?;

        // 4a. Bit-before-durable-record (§7): stamp every member
        //     superblock before the record exists. Sector 0 is never
        //     rewritten by the live backend (checkpoints flip the root
        //     ledger), so this is race-free under the mount's claims.
        for path in &meta_paths {
            crate::meta_backend::kv::superblock::set_volume_lifecycle_bit(std::path::Path::new(
                path,
            ))
            .await
            .map_err(|e| format!("lifecycle-bit stamp failed on {path}: {e}"))?;
        }

        // 4b. Durable record commit through the live conveyor.
        records.push(record.clone());
        cfg.data_lv = Some(records.iter().map(|r| r.backing_dev.clone()).collect());
        cfg.data_volumes = Some(records.clone());
        let bytes =
            serde_json::to_vec(&cfg).map_err(|e| format!("format config serialize failed: {e}"))?;
        meta.setxattr(
            1,
            crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR,
            &bytes,
        )
        .await
        .map_err(|e| format!("durable volume-record commit failed: {e}"))?;

        // 5. Placement enabled LAST — the new capacity participates only
        //    after the next mount is guaranteed to know the id.
        self.router
            .backend_router
            .publish_backend(&record.id, backend)
            .map_err(|e| format!("backend publish failed after the durable commit: {e}"))?;
        self.router.backend_router.set_volume_records(records);
        info!(
            "volume add-data: '{}' added as {} ({} bytes) — durable record committed, \
             placement enabled",
            device, record.id, capacity
        );

        // 6. §5.3 step 6 (KD-12): the automatic rebalance pass is the
        //    DEFAULT — a bounded fabric job at the conservative throttle,
        //    visible/pausable/cancellable like any job. `--no-rebalance`
        //    opts out (allocation participation only).
        if !no_rebalance {
            match self.job_fabric.load().as_ref() {
                Some(fabric) => {
                    match fabric
                        .submit(crate::jobs::JobSpec {
                            job_type: crate::jobs::JobType::Rebalance,
                            throttle_pct: crate::jobs::REBALANCE_DEFAULT_THROTTLE_PCT,
                        })
                        .await
                    {
                        Ok(job_id) => info!(
                            "volume add-data: auto-rebalance pass submitted as job {job_id} \
                             (throttle {} % — KD-12 default; --no-rebalance opts out)",
                            crate::jobs::REBALANCE_DEFAULT_THROTTLE_PCT
                        ),
                        Err(e) => log::error!(
                            "volume add-data: auto-rebalance submission failed: {e} — the \
                             volume participates in placement; run `squeezefs defrag \
                             --rebalance` (VL7) or re-add capacity pressure manually"
                        ),
                    }
                }
                None => log::warn!(
                    "volume add-data: no job fabric wired — the auto-rebalance pass was \
                     not submitted"
                ),
            }
        }
        Ok(record)
    }

    /// Durable volume-state commit through the LIVE meta backend (the
    /// conveyor): rewrite the `FormatConfig` record set with `volume_id`
    /// flipped to `new_state`, mirror `data_lv`, publish the runtime
    /// snapshot. Shared by remove-data (→ draining) and undrain
    /// (→ active).
    async fn commit_volume_state(
        &self,
        volume_id: &str,
        new_state: &str,
    ) -> std::result::Result<(), String> {
        let meta = self
            .meta_backend
            .as_ref()
            .ok_or_else(|| "no metadata backend mounted".to_string())?;
        let raw = meta
            .getxattr(1, crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR)
            .await
            .map_err(|e| format!("format config read failed: {e}"))?
            .ok_or_else(|| "format config not found on the volume set".to_string())?;
        let mut cfg: crate::FormatConfig =
            serde_json::from_slice(&raw).map_err(|e| format!("format config undecodable: {e}"))?;
        let mut records = cfg.resolved_data_volumes();
        let rec = records
            .iter_mut()
            .find(|r| r.id == volume_id)
            .ok_or_else(|| format!("unknown data volume '{volume_id}'"))?;
        rec.state = new_state.to_string();
        cfg.data_lv = Some(
            records
                .iter()
                .filter(|r| r.state != crate::VOL_STATE_RETIRED)
                .map(|r| r.backing_dev.clone())
                .collect(),
        );
        cfg.data_volumes = Some(records.clone());
        let bytes =
            serde_json::to_vec(&cfg).map_err(|e| format!("format config serialize: {e}"))?;
        meta.setxattr(
            1,
            crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR,
            &bytes,
        )
        .await
        .map_err(|e| format!("durable volume-state commit failed: {e}"))?;
        self.router.backend_router.set_volume_records(records);
        Ok(())
    }

    /// The ONLINE `volume remove-data` (design-volume-lifecycle §5.4,
    /// served by the admin-lane `volume-remove-data` verb): the §5.2
    /// capacity preflight (honest refusal with the exact numbers —
    /// `volume_preflight_refusals` counted), the durable
    /// `Active → Draining` flip (bit 3 already stamped by any prior
    /// lifecycle verb; stamped here for sets whose first lifecycle use
    /// is a remove), and the `evacuate-data-volume` job submission.
    /// Returns the job id.
    pub async fn admin_remove_data_volume(
        &self,
        volume_id: &str,
        throttle_pct: u32,
    ) -> std::result::Result<String, String> {
        let refused = |msg: String| {
            METRICS
                .volume_preflight_refusals
                .fetch_add(1, Ordering::Relaxed);
            msg
        };
        let meta = self
            .meta_backend
            .as_ref()
            .ok_or_else(|| "no metadata backend mounted".to_string())?;
        let fabric = self
            .job_fabric
            .load()
            .as_ref()
            .clone()
            .ok_or_else(|| "no job fabric wired on this mount".to_string())?;
        let ctx = fabric
            .mover_ctx()
            .ok_or_else(|| "the mover context is not wired on this fabric".to_string())?;

        // State gate: only an Active member can start draining.
        match self.router.backend_router.volume_state(volume_id) {
            Some(state) if state == crate::VOL_STATE_ACTIVE => {}
            Some(state) => {
                return Err(refused(format!(
                    "volume '{volume_id}' is '{state}' — only an active volume can be \
                     removed (undrain first, or see `squeezefs volume list`)"
                )))
            }
            None => {
                return Err(refused(format!(
                    "unknown data volume '{volume_id}' (see `squeezefs volume list`)"
                )))
            }
        }

        // §5.2 preflight: dedupe census, survivor availability, the
        // worker-window transient, write-rate/ETA headroom.
        let pf = crate::jobs::drain_preflight(meta, ctx, volume_id, fabric.worker_count())
            .await
            .map_err(|e| format!("capacity preflight census failed: {e}"))?;
        if !pf.admits() {
            return Err(refused(pf.refusal()));
        }
        METRICS
            .evacuate_needed_bytes
            .store(pf.needed_bytes, Ordering::Relaxed);
        METRICS
            .evacuate_avail_bytes
            .store(pf.avail_bytes, Ordering::Relaxed);
        METRICS
            .evacuate_transient_bytes
            .store(pf.transient_bytes, Ordering::Relaxed);

        // Lifecycle bit before the durable state flip (§7 ordering law —
        // idempotent for sets that already used a lifecycle verb).
        for vol in &meta.volumes {
            let path = vol.device_path().display().to_string();
            crate::meta_backend::kv::superblock::set_volume_lifecycle_bit(std::path::Path::new(
                &path,
            ))
            .await
            .map_err(|e| format!("lifecycle-bit stamp failed on {path}: {e}"))?;
        }

        // Durable Active → Draining, then the evacuation job.
        self.commit_volume_state(volume_id, crate::VOL_STATE_DRAINING)
            .await?;
        let job_id = fabric
            .submit(crate::jobs::JobSpec {
                job_type: crate::jobs::JobType::EvacuateVolume {
                    volume_id: volume_id.to_string(),
                },
                throttle_pct,
            })
            .await
            .map_err(|e| format!("evacuation job submission failed: {e}"))?;
        info!(
            "volume remove-data: '{volume_id}' draining (job {job_id}, throttle {throttle_pct} %) \
             — preflight: needed {} B, avail {} B, transient {} B, headroom {} B",
            pf.needed_bytes, pf.avail_bytes, pf.transient_bytes, pf.headroom_bytes
        );
        Ok(job_id)
    }

    /// The ONLINE `volume undrain` (§5.4): cancel the volume's
    /// evacuation job(s) and flip the durable state back to `active`.
    pub async fn admin_undrain_data_volume(
        &self,
        volume_id: &str,
    ) -> std::result::Result<(), String> {
        match self.router.backend_router.volume_state(volume_id) {
            Some(state) if state == crate::VOL_STATE_DRAINING => {}
            Some(state) => {
                return Err(format!(
                    "volume '{volume_id}' is '{state}', not draining — nothing to undrain \
                     (retired volumes never come back: ids are permanent, KD-5)"
                ))
            }
            None => return Err(format!("unknown data volume '{volume_id}'")),
        }
        // Durable flip FIRST: a mover pass observing the non-draining
        // state self-cancels even if the explicit cancel below races it.
        self.commit_volume_state(volume_id, crate::VOL_STATE_ACTIVE)
            .await?;
        if let Some(fabric) = self.job_fabric.load().as_ref() {
            for (job_id, state) in fabric.jobs_matching(|jt| {
                matches!(jt, crate::jobs::JobType::EvacuateVolume { volume_id: v } if v == volume_id)
            }) {
                if !state.is_terminal() {
                    if let Err(e) = fabric.cancel(&job_id).await {
                        log::warn!("undrain: cancelling evacuation job {job_id} failed: {e}");
                    }
                }
            }
        }
        info!("volume undrain: '{volume_id}' back to active; evacuation cancelled");
        Ok(())
    }

    /// The mover quiescence probe (§5.4 step 3) for THIS mount: a block
    /// is quiescent when it has no live RAM `ActiveBlockBuf`, no staged
    /// `active_block:` ring entry, and no spilled `active_block_ext:`
    /// record.
    pub fn mover_quiesce_probe(&self) -> crate::jobs::QuiesceProbe {
        let bufs = self.active_block_buffers.clone();
        let router = self.router.clone();
        std::sync::Arc::new(move |ino, b| {
            let key = crate::keys::active_block(ino, b).to_string();
            let ext = crate::keys::active_block_ext(ino, b).to_string();
            !bufs.contains_key(&key)
                && !router.cache.nvme.has_staged_active_block(&key)
                && !router.cache.nvme.has_staged_extent_record(&ext)
        })
    }

    /// PR VL7 (design-volume-lifecycle §5.7 D3): the fold hook the defrag
    /// fabric drives — targets are this mount's parked-extent custody
    /// (RAM extent-repr overlays + spilled `active_block_ext:` records),
    /// and the kick IS [`Self::fold_extent_block`]: the existing W2 fold
    /// machinery run to completion, never a reimplementation.
    pub fn defrag_fold_hook(&self) -> crate::jobs::FoldHook {
        let bufs = self.active_block_buffers.clone();
        let router = self.router.clone();
        // Cycle hygiene (load-bearing): the hook lives INSIDE the fabric
        // (fabric → MoverCtx → FoldHook), and a verbatim clone carries
        // the SHARED `job_fabric` cell that points back at the fabric —
        // an Arc cycle that would leak this filesystem (and its cached
        // DLM leases in the process-global lock table) past teardown.
        // The captured clone gets a fresh, empty fabric cell instead;
        // folds never consult it.
        let mut fs = self.clone();
        fs.job_fabric = std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(None));
        crate::jobs::FoldHook {
            targets: std::sync::Arc::new(move || {
                let mut seen = std::collections::HashSet::new();
                let mut out = Vec::new();
                for entry in bufs.iter() {
                    if entry.value().is_extent_repr() {
                        if let Some(t) = parse_block_family_key(entry.key()) {
                            if seen.insert(t) {
                                out.push(t);
                            }
                        }
                    }
                }
                for key in router.cache.nvme.extent_record_keys("") {
                    if let Some(t) = parse_block_family_key(&key) {
                        if seen.insert(t) {
                            out.push(t);
                        }
                    }
                }
                out.sort_unstable();
                out
            }),
            kick: std::sync::Arc::new(move |ino, b| {
                // One deep clone of the Arc'd handles per kick — folds are
                // 4 MiB-class device ops; the clone is noise (contrast the
                // per-op-clone lesson on `DataPlaneSink::fs`).
                let fs = fs.clone();
                Box::pin(async move {
                    fs.fold_extent_block(ino, b)
                        .await
                        .map_err(|e| format!("fold of ino {ino} block {b} failed: {e}"))
                })
            }),
        }
    }

    /// Live metadata-volume health override (`config metadata-volume
    /// enable/disable`, re-homed from the retired `/dev/shm` runtime
    /// config): fail-stop only — a disabled volume's inos error until
    /// re-enabled; no data moves (slot migration is PR VL5b). Runtime
    /// state, deliberately not durable (no durable meta-volume records
    /// exist until VL5a).
    pub fn admin_set_meta_volume_health(
        &self,
        volume_id: &str,
        disabled: bool,
    ) -> std::result::Result<(), String> {
        let meta = self
            .meta_backend
            .as_ref()
            .ok_or_else(|| "no metadata backend mounted".to_string())?;
        let idx: usize = volume_id
            .strip_prefix("meta_volume_")
            .unwrap_or(volume_id)
            .parse()
            .map_err(|_| {
                format!("unknown metadata volume '{volume_id}' (use meta_volume_<index>)")
            })?;
        if idx >= meta.volumes.len() {
            return Err(format!(
                "metadata volume index {idx} out of range ({} volumes mounted)",
                meta.volumes.len()
            ));
        }
        if disabled {
            meta.disabled_volumes.insert(idx, true);
        } else {
            meta.disabled_volumes.remove(&idx);
        }
        Ok(())
    }

    async fn generate_config_json(&self) -> String {
        // The `/dev/shm` runtime-config ingest that used to run here was
        // deleted in PR VL3: health overrides arrive over the admin lane
        // (`volume-disable`/`volume-enable`) or as durable record state
        // applied at registration — `.config` reads live router state.
        let mut format_fields = std::collections::HashMap::new();
        let active_dirs = self.router.cache.nvme.staging_dirs();
        let active_dirs_str = active_dirs
            .iter()
            .map(|d| d.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(",");
        format_fields.insert("disk_cache_paths".to_string(), active_dirs_str);

        // The data-volume table lists EXACTLY the registered named volumes.
        // `backend_0` is a legacy key-resolution alias of the default slot,
        // not a volume: surfacing it here (pre-fix) made it a phantom entry
        // in `.config` on every multi-volume mount. Only a bare router (no
        // named registrations — offline tools, tests) still reports its
        // default slot under the legacy name. Entries carry the volume's
        // REAL backing device path (the registered device — user report
        // 2026-07-22: the name was recycled into `backing_dev`), its
        // durable id, and its canonical `sqdata://` URI; the durable
        // record snapshot fixes a deterministic (volume) order.
        let records = self.router.backend_router.volume_records();
        let mut data_vol_names: Vec<String> = Vec::new();
        let mut covered = std::collections::HashSet::new();
        for rec in records.iter() {
            if self.router.backend_router.backends.contains_key(&rec.id) {
                data_vol_names.push(rec.id.clone());
                covered.insert(rec.id.clone());
            }
        }
        let mut recordless: Vec<String> = self
            .router
            .backend_router
            .backends
            .iter()
            .map(|item| item.key().clone())
            .filter(|name| !covered.contains(name))
            .collect();
        recordless.sort();
        data_vol_names.extend(recordless);
        if data_vol_names.is_empty() {
            data_vol_names.push("backend_0".to_string());
        }

        let mut data_volumes = serde_json::Map::new();
        let mut data_paths: Vec<String> = Vec::new();
        for name in data_vol_names {
            let is_unhealthy = self
                .router
                .backend_router
                .unhealthy_backends
                .contains_key(&name);
            let status = if is_unhealthy { "disabled" } else { "enabled" };
            let health = self.router.backend_router.get_backend_health(&name);
            // The registered device is authoritative for the live path;
            // the durable record backs it up; the bare-router default
            // slot falls through to the default device.
            let backing_dev = self
                .router
                .backend_router
                .backends
                .get(&name)
                .map(|be| be.device.device_path.clone())
                .or_else(|| {
                    records
                        .iter()
                        .find(|r| r.id == name)
                        .map(|r| r.backing_dev.clone())
                })
                .unwrap_or_else(|| {
                    self.router
                        .backend_router
                        .default_device
                        .device_path
                        .clone()
                });
            data_volumes.insert(
                name.clone(),
                serde_json::json!({
                    "id": name,
                    "backing_dev": backing_dev,
                    "uri": format!("sqdata://{backing_dev}"),
                    "status": status,
                    "health": health,
                }),
            );
            data_paths.push(backing_dev);
        }

        let mut metadata_volumes = serde_json::Map::new();
        let mut meta_paths: Vec<String> = Vec::new();
        if let Some(ref meta) = self.meta_backend {
            for (idx, vol) in meta.volumes.iter().enumerate() {
                let name = format!("meta_volume_{}", idx);
                let path_str = vol.device_path().to_string_lossy().to_string();
                let is_disabled = meta.disabled_volumes.contains_key(&idx);
                let status = if is_disabled { "disabled" } else { "enabled" };
                let health = meta.get_volume_health(idx).await;
                metadata_volumes.insert(
                    name,
                    serde_json::json!({
                        "backing_dev": path_str,
                        "uri": format!("sqmeta://{path_str}"),
                        "status": status,
                        "health": health,
                    }),
                );
                meta_paths.push(path_str);
            }
        }

        let mut config_obj = serde_json::json!({
            "client_version": crate::version::version_line(),
            "format": format_fields,
            "data_volumes": data_volumes,
            "metadata_volumes": metadata_volumes,
            "uid": self.uid,
            "gid": self.gid,
            "block_size": self.router.block_size.load(Ordering::Relaxed),
        });
        // Canonical joined-member URI spellings (`parse_block_uri`
        // grammar: `scheme://p1,p2` in volume order) — the exact strings
        // the CLI verbs accept. Only mounts with a metadata backend can
        // name their set.
        if let Some(obj) = config_obj.as_object_mut() {
            if !meta_paths.is_empty() {
                obj.insert(
                    "sqmeta_uri".to_string(),
                    serde_json::json!(format!("sqmeta://{}", meta_paths.join(","))),
                );
            }
            obj.insert(
                "sqdata_uri".to_string(),
                serde_json::json!(format!("sqdata://{}", data_paths.join(","))),
            );
        }

        finish_virtual_payload(serde_json::to_string_pretty(&config_obj).unwrap_or_default())
    }

    fn get_stats_attr(&self, size: u64) -> FileAttr {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let sec = now.as_secs() as i64;
        let nsec = now.subsec_nanos();

        FileAttr {
            ino: STATS_INODE,
            size,
            blocks: size.div_ceil(512),
            atime: Timestamp::new(sec, nsec),
            mtime: Timestamp::new(sec, nsec),
            ctime: Timestamp::new(sec, nsec),
            kind: FileType::RegularFile,
            perm: 0o444, // read-only by all
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
        }
    }

    pub async fn generate_stats_json(&self) -> String {
        let read_lru_keys = self.router.cache.read_lru.keys();
        let write_lru_keys = self.router.cache.write_lru.keys();
        let nvme_read_cache_block_keys = self.router.cache.nvme.list_cached_blocks();

        let mut nvme_staged_write_file_ids = Vec::new();
        let mut active_writes = serde_json::Map::new();

        for key in self.router.cache.nvme.list_staged_files() {
            if key.starts_with("active_block:") {
                let parts: Vec<&str> = key.split(':').collect();
                if parts.len() == 3 {
                    let inode_name = parts[1].to_string();
                    let block_name = parts[2].to_string();
                    active_writes
                        .entry(inode_name)
                        .or_insert_with(|| serde_json::Value::Array(Vec::new()))
                        .as_array_mut()
                        .unwrap()
                        .push(serde_json::Value::String(block_name));
                }
            } else {
                nvme_staged_write_file_ids.push(key);
            }
        }

        let hits = METRICS.cache_hits.load(Ordering::Relaxed);
        let misses = METRICS.cache_misses.load(Ordering::Relaxed);
        let ratio = if hits + misses > 0 {
            hits as f64 / (hits + misses) as f64
        } else {
            0.0
        };

        #[cfg(target_os = "linux")]
        let fuse_over_uring_sessions = fuse3::over_uring_sessions_active();
        #[cfg(not(target_os = "linux"))]
        let fuse_over_uring_sessions = 0u64;
        #[cfg(target_os = "linux")]
        let (fou_req, fou_rep, fou_err, fou_reg) = fuse3::over_uring_stats();
        #[cfg(not(target_os = "linux"))]
        let (fou_req, fou_rep, fou_err, fou_reg) = (0u64, 0u64, 0u64, 0u64);
        // §5.4 transport payload-lease signals: adoption, parked-ent
        // pressure, and the severance-boundary enforcement pair
        // (outstanding hovers at in-flight write count and returns to 0 at
        // quiesce; max age is bounded by one handler invocation).
        #[cfg(target_os = "linux")]
        let (t_leases, t_parked, t_outstanding, t_max_age) = fuse3::transport_lease_stats();
        #[cfg(not(target_os = "linux"))]
        let (t_leases, t_parked, t_outstanding, t_max_age) = (0u64, 0u64, 0u64, 0u64);
        // Post-arm classical sideband deliveries (kernel-mandated FORGET/
        // INTERRUPT/resend traffic + fiq->ops switchover stragglers). Must
        // move under unlink storms; a permanent zero here while forgets flow
        // means the sideband is stranded — the stuck-request unmount wedge
        // class.
        #[cfg(target_os = "linux")]
        let t_classical_sideband = fuse3::over_uring_classical_sideband();
        #[cfg(not(target_os = "linux"))]
        let t_classical_sideband = 0u64;
        // L3 lever B wake economy: queue-eventfd writes performed vs elided
        // by the per-queue coalescer (submit_reply + lease-drop sites).
        // writes/(writes+elided) ≈ 1 under saturated load means the
        // coalescer stopped eliding — the pre-L3 1.67 eventfd writes/op
        // posture.
        #[cfg(target_os = "linux")]
        let (t_wake_writes, t_wakes_elided) = fuse3::transport_wake_stats();
        #[cfg(not(target_os = "linux"))]
        let (t_wake_writes, t_wakes_elided) = (0u64, 0u64);
        // D3.a (design-metadata-throughput §5.3/§9): COMMIT_AND_FETCH SQEs
        // per queue-worker ring flush. Mean batch (commits / flushes) ≈ 1
        // under storm load means the S2 submit batching regressed to
        // per-message syscalls. Buckets export zero-valued pre-session so
        // operators can always key on the field.
        #[cfg(target_os = "linux")]
        let (t_cb_flushes, t_cb_commits, t_cb_hist) = {
            let (flushes, commits, buckets) = fuse3::over_uring_commit_batch_stats();
            let hist: serde_json::Map<String, serde_json::Value> = fuse3::COMMIT_BATCH_LABELS
                .iter()
                .zip(buckets)
                .map(|(label, count)| ((*label).to_string(), serde_json::Value::from(count)))
                .collect();
            (flushes, commits, hist)
        };
        #[cfg(not(target_os = "linux"))]
        let (t_cb_flushes, t_cb_commits, t_cb_hist) = (
            0u64,
            0u64,
            serde_json::Map::<String, serde_json::Value>::new(),
        );
        // L1 transport geometry gauges (IOPS-parity program): the live
        // session's over-uring ring shape, its registered payload-arena
        // bytes (queues × depth × payload_sz — the RSS the depth policy
        // budgets), and the INIT-reply max_background actually negotiated.
        #[cfg(target_os = "linux")]
        let (t_queues, t_depth, _t_payload_sz, t_buffer_bytes, t_max_background) =
            fuse3::over_uring_geometry();
        #[cfg(not(target_os = "linux"))]
        let (t_queues, t_depth, t_buffer_bytes, t_max_background) = (0u64, 0u64, 0u64, 0u64);

        // PR K7 (design §10): the `meta_kv_*` family is emitted only when
        // a metadata volume is mounted (v3 is the only metadata format).
        let has_meta = self
            .meta_backend
            .as_ref()
            .is_some_and(|mb| !mb.volumes.is_empty());

        // Volume-lifecycle gauge rows (design-volume-lifecycle §10, PR
        // VL3): durable records joined with live allocator accounting.
        let volume_states: Vec<serde_json::Value> = self
            .router
            .backend_router
            .volume_states()
            .into_iter()
            .map(|row| {
                serde_json::json!({
                    "id": row.id,
                    "backing_dev": row.backing_dev,
                    "state": row.state,
                    "healthy": row.healthy,
                    "capacity_bytes": row.capacity_bytes,
                    "used_bytes": row.used_bytes,
                    "free_bytes": row.free_bytes,
                })
            })
            .collect();

        // §5.9 placement gauges (design-volume-lifecycle §10, PR VL4b):
        // the live PlacementTable snapshot — per-backend picks/weight/
        // fill plus the set-level fill spread (the G-VL-8 instrument).
        let placement_table = self.router.backend_router.placement_snapshot();
        let placement_backends: Vec<serde_json::Value> = placement_table
            .rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "id": row.id,
                    "backend_placement_picks": row.picks.load(Ordering::Relaxed),
                    "backend_placement_weight": row.weight,
                    "backend_fill_ratio": row.fill_ratio,
                    "eligible": row.eligible,
                })
            })
            .collect();
        let placement_obj = serde_json::json!({
            "backend_fill_spread": placement_table.fill_spread,
            "backends": placement_backends,
        });

        let mut stats_obj = serde_json::json!({
            // Build identity (docs/operations.md §Versioning & releases):
            // the fleet mixed-version detector. `build_commit` is the full
            // git commit (with `-dirty` when built from a modified tree);
            // `build_tag` always exports, empty string when the commit is
            // not a `stable-*`/`lts-*` release.
            "build_commit": crate::version::build_commit(),
            "build_tag": crate::version::build_tag(),
            "read_lru_keys": read_lru_keys,
            "write_lru_keys": write_lru_keys,
            "nvme_staged_write_file_ids": nvme_staged_write_file_ids,
            "nvme_read_cache_block_keys": nvme_read_cache_block_keys,
            "active_writes": active_writes,
            "active_leases_count": self.active_leases.len(),
            "volume_states": volume_states,
            "placement": placement_obj,
            "metrics": {
                "fuse_ops": METRICS.fuse_ops.load(Ordering::Relaxed),
                "fuse_op_watchdog_overdue": METRICS.fuse_op_watchdog_overdue.load(Ordering::Relaxed),
                "fuse_flush_clean_fastpath": METRICS.fuse_flush_clean_fastpath.load(Ordering::Relaxed),
                "fuse_release_clean_fastpath": METRICS.fuse_release_clean_fastpath.load(Ordering::Relaxed),
                "fuse_lookup_negative_replies": METRICS.fuse_lookup_negative_replies.load(Ordering::Relaxed),
                "fuse_attr_cache_refreshes": METRICS.fuse_attr_cache_refreshes.load(Ordering::Relaxed),
                "meta_updates": METRICS.meta_updates.load(Ordering::Relaxed),
                "put_obj": METRICS.put_obj.load(Ordering::Relaxed),
                "get_obj": METRICS.get_obj.load(Ordering::Relaxed),
                "del_obj": METRICS.del_obj.load(Ordering::Relaxed),
                "cache_hits": hits,
                "cache_misses": misses,
                "cache_hit_ratio": ratio,
                "stale_binding_rebinds": METRICS.stale_binding_rebinds.load(Ordering::Relaxed),
                "singleflight_waiter_result_serves": METRICS.singleflight_waiter_result_serves.load(Ordering::Relaxed),
                "hot_block_hits": METRICS.hot_block_hits.load(Ordering::Relaxed),
                "hot_block_misses": METRICS.hot_block_misses.load(Ordering::Relaxed),
                "hot_block_evictions": METRICS.hot_block_evictions.load(Ordering::Relaxed),
                "hot_block_probation_drops": METRICS.hot_block_probation_drops.load(Ordering::Relaxed),
                "hot_block_dehydrate_skips": METRICS.hot_block_dehydrate_skips.load(Ordering::Relaxed),
                "ranged_reads": METRICS.ranged_reads.load(Ordering::Relaxed),
                "ranged_read_bytes": METRICS.ranged_read_bytes.load(Ordering::Relaxed),
                "ranged_read_unaligned_bounces": METRICS.ranged_read_unaligned_bounces.load(Ordering::Relaxed),
                "ranged_read_rebinds": METRICS.ranged_read_rebinds.load(Ordering::Relaxed),
                "ranged_read_ghost_escalations": METRICS.ranged_read_ghost_escalations.load(Ordering::Relaxed),
                "read_admission_evicted_unhit": METRICS.read_admission_evicted_unhit.load(Ordering::Relaxed),
                "read_admission_wasted_bytes": METRICS.read_admission_wasted_bytes.load(Ordering::Relaxed),
                "read_admission_governor_denials": METRICS.read_admission_governor_denials.load(Ordering::Relaxed),
                "read_admission_governor_clamped": self.router.cache.admission_governor.clamped(),
                "mem_budget_bytes": crate::mem_budget::MEM_BUDGET.budget_bytes(),
                "mem_budget_pressure_bytes": crate::mem_budget::MEM_BUDGET.pressure_bytes(),
                "mem_budget_gauge_sum_bytes": crate::mem_budget::MEM_BUDGET.gauge_sum_bytes(),
                "mem_budget_level": crate::mem_budget::MEM_BUDGET.level() as u8,
                "mem_budget_yellow_events": crate::mem_budget::MEM_BUDGET.yellow_events(),
                "mem_budget_red_events": crate::mem_budget::MEM_BUDGET.red_events(),
                "mem_budget_floors_clamped": crate::mem_budget::MEM_BUDGET.floors_clamped(),
                "mem_budget_dehydrate_paused": METRICS.mem_budget_dehydrate_paused.load(Ordering::Relaxed),
                "mem_budget_unreclaimable_bytes": crate::mem_budget::MEM_BUDGET.unreclaimable_bytes(),
                "mem_budget_hard_backstops": crate::mem_budget::MEM_BUDGET.hard_backstops(),
                "mem_budget_backstop_active": crate::mem_budget::MEM_BUDGET.backstop_active(),
                "mem_budget_tier_publish_paused": crate::mem_budget::MEM_BUDGET.tier_publish_paused(),
                "read_tier_publishes_paused": METRICS.read_tier_publishes_paused.load(Ordering::Relaxed),
                "parked_gate_waits": METRICS.parked_gate_waits.load(Ordering::Relaxed),
                "parked_gate_self_flushes": METRICS.parked_gate_self_flushes.load(Ordering::Relaxed),
                "parked_gate_timeouts": METRICS.parked_gate_timeouts.load(Ordering::Relaxed),
                "staged_rmw_pooled_seeds": METRICS.staged_rmw_pooled_seeds.load(Ordering::Relaxed),
                "overwrite_seed_deferred": METRICS.overwrite_seed_deferred.load(Ordering::Relaxed),
                "overwrite_seed_skipped": METRICS.overwrite_seed_skipped.load(Ordering::Relaxed),
                "overwrite_seed_materialized": METRICS.overwrite_seed_materialized.load(Ordering::Relaxed),
                "staged_truncate_inplace_shrinks": METRICS.staged_truncate_inplace_shrinks.load(Ordering::Relaxed),
                "staged_truncate_durable_clips": METRICS.staged_truncate_durable_clips.load(Ordering::Relaxed),
                "mem_budget_components": crate::mem_budget::MEM_BUDGET
                    .stats_components()
                    .into_iter()
                    .map(|(name, current, floor, weight, sheds)| {
                        (
                            name.to_string(),
                            serde_json::json!({
                                "current": current,
                                "floor": floor,
                                "weight": weight,
                                "sheds": sheds,
                            }),
                        )
                    })
                    .collect::<serde_json::Map<String, serde_json::Value>>(),
                "hot_block_current_bytes": self.router.cache.hot_block.current_bytes(),
                "hot_block_max_bytes": self.router.cache.hot_block.max_bytes(),
                "read_fill_publishes_skipped": METRICS.read_fill_publishes_skipped.load(Ordering::Relaxed),
                "read_tier_admissions": METRICS.read_tier_admissions.load(Ordering::Relaxed),
                "read_tier_admission_ghost_hits": METRICS.read_tier_admission_ghost_hits.load(Ordering::Relaxed),
                "read_streams_classified": METRICS.read_streams_classified.load(Ordering::Relaxed),
                "read_odirect_requests": METRICS.read_odirect_requests.load(Ordering::Relaxed),
                "read_odirect_tier_serves": METRICS.read_odirect_tier_serves.load(Ordering::Relaxed),
                "read_odirect_ghost_admits": METRICS.read_odirect_ghost_admits.load(Ordering::Relaxed),
                "read_device_true_reads": METRICS.read_device_true_reads.load(Ordering::Relaxed),
                "direct_device_true": self.router.direct_device_true(),
                "read_tier_admission_mode": format!("{:?}", self.router.tier_admission),
                "prefetch_issued": METRICS.prefetch_issued.load(Ordering::Relaxed),
                "prefetch_completed": METRICS.prefetch_completed.load(Ordering::Relaxed),
                "prefetch_wasted": METRICS.prefetch_wasted.load(Ordering::Relaxed),
                "prefetch_inflight_bytes": METRICS.prefetch_inflight_bytes.load(Ordering::Relaxed),
                "prefetch_window_hwm": METRICS.prefetch_window_hwm.load(Ordering::Relaxed),
                "prefetch_foreground_waits": METRICS.prefetch_foreground_waits.load(Ordering::Relaxed),
                "prefetch_evicted_unconsumed": METRICS.prefetch_evicted_unconsumed.load(Ordering::Relaxed),
                "prefetch_active_streams": METRICS.prefetch_active_streams.load(Ordering::Relaxed),
                "layout_inline_writes": METRICS.layout_inline_writes.load(Ordering::Relaxed),
                "layout_staged_writes": METRICS.layout_staged_writes.load(Ordering::Relaxed),
                "staged_spill_escalations": METRICS.staged_spill_escalations.load(Ordering::Relaxed),
                "block_double_frees": METRICS.block_double_frees.load(Ordering::Relaxed),
                "block_untracked_free_refusals": METRICS.block_untracked_free_refusals.load(Ordering::Relaxed),
                "layout_striped_writes": METRICS.layout_striped_writes.load(Ordering::Relaxed),
                "compress_stored_raw": METRICS.compress_stored_raw.load(Ordering::Relaxed),
                "bg_spawn_admitted": METRICS.bg_spawn_admitted.load(Ordering::Relaxed),
                "bg_spawn_rejected": METRICS.bg_spawn_rejected.load(Ordering::Relaxed),
                "uring_queue_full": METRICS.uring_queue_full.load(Ordering::Relaxed),
                "staging_generation_discards": METRICS.staging_generation_discards.load(Ordering::Relaxed),
                "meta_slot_gate_parked_commits": METRICS.meta_slot_gate_parked_commits.load(Ordering::Relaxed),
                "meta_slot_migrations": METRICS.meta_slot_migrations.load(Ordering::Relaxed),
                "meta_slot_records_copied": METRICS.meta_slot_records_copied.load(Ordering::Relaxed),
                "meta_slot_delta_keys": METRICS.meta_slot_delta_keys.load(Ordering::Relaxed),
                "meta_slot_delta_overflows": METRICS.meta_slot_delta_overflows.load(Ordering::Relaxed),
                "meta_slot_cutover_ms_max": METRICS.meta_slot_cutover_ms_max.load(Ordering::Relaxed),
                "staging_drain_barriers": METRICS.staging_drain_barriers.load(Ordering::Relaxed),
                "staged_payload_lost_reads": METRICS.staged_payload_lost_reads.load(Ordering::Relaxed),
                "staged_identity_retries": METRICS.staged_identity_retries.load(Ordering::Relaxed),
                "nvme_unaligned_write_fallbacks": METRICS.nvme_unaligned_write_fallbacks.load(Ordering::Relaxed),
                "lease_acquire_ok": METRICS.lease_acquire_ok.load(Ordering::Relaxed),
                "lease_acquire_fail": METRICS.lease_acquire_fail.load(Ordering::Relaxed),
                "writeback_retry_exhaustions": METRICS.writeback_retry_exhaustions.load(Ordering::Relaxed),
                "fuse_reserved_xattr_refusals": METRICS.fuse_reserved_xattr_refusals.load(Ordering::Relaxed),
                "job_submitted": METRICS.job_submitted.load(Ordering::Relaxed),
                "job_completed": METRICS.job_completed.load(Ordering::Relaxed),
                "job_cancelled": METRICS.job_cancelled.load(Ordering::Relaxed),
                "job_failed": METRICS.job_failed.load(Ordering::Relaxed),
                "job_tasks_done": METRICS.job_tasks_done.load(Ordering::Relaxed),
                "job_checkpoint_writes": METRICS.job_checkpoint_writes.load(Ordering::Relaxed),
                "job_copy_buffer_bytes": METRICS.job_copy_buffer_bytes.load(Ordering::Relaxed),
                "job_paused_mem_pressure": METRICS.job_paused_mem_pressure.load(Ordering::Relaxed),
                "job_serialized_waits": METRICS.job_serialized_waits.load(Ordering::Relaxed),
                "job_paused_capacity": METRICS.job_paused_capacity.load(Ordering::Relaxed),
                "evacuate_blocks_moved": METRICS.evacuate_blocks_moved.load(Ordering::Relaxed),
                "evacuate_bytes_moved": METRICS.evacuate_bytes_moved.load(Ordering::Relaxed),
                "evacuate_shared_blocks_moved": METRICS.evacuate_shared_blocks_moved.load(Ordering::Relaxed),
                "evacuate_inflight_bytes": METRICS.evacuate_inflight_bytes.load(Ordering::Relaxed),
                "evacuate_stale_token_noops": METRICS.evacuate_stale_token_noops.load(Ordering::Relaxed),
                "evacuate_deferred_staged_blocks": METRICS.evacuate_deferred_staged_blocks.load(Ordering::Relaxed),
                "evacuate_replans": METRICS.evacuate_replans.load(Ordering::Relaxed),
                "evacuate_needed_bytes": METRICS.evacuate_needed_bytes.load(Ordering::Relaxed),
                "evacuate_avail_bytes": METRICS.evacuate_avail_bytes.load(Ordering::Relaxed),
                "evacuate_transient_bytes": METRICS.evacuate_transient_bytes.load(Ordering::Relaxed),
                "volume_preflight_refusals": METRICS.volume_preflight_refusals.load(Ordering::Relaxed),
                "placement_table_refreshes": METRICS.placement_table_refreshes.load(Ordering::Relaxed),
                "fsck_inodes_scanned": METRICS.fsck_inodes_scanned.load(Ordering::Relaxed),
                "fsck_nodes_walked": METRICS.fsck_nodes_walked.load(Ordering::Relaxed),
                "fsck_blocks_checked": METRICS.fsck_blocks_checked.load(Ordering::Relaxed),
                "fsck_refcounts_checked": METRICS.fsck_refcounts_checked.load(Ordering::Relaxed),
                "fsck_suspects": METRICS.fsck_suspects.load(Ordering::Relaxed),
                "fsck_suspects_cleared": METRICS.fsck_suspects_cleared.load(Ordering::Relaxed),
                "fsck_epoch_exempted": METRICS.fsck_epoch_exempted.load(Ordering::Relaxed),
                "fsck_inflight_exempted": METRICS.fsck_inflight_exempted.load(Ordering::Relaxed),
                "fsck_mover_ledger_exempted": METRICS.fsck_mover_ledger_exempted.load(Ordering::Relaxed),
                "fsck_findings": METRICS.fsck_findings.load(Ordering::Relaxed),
                "fsck_scan_secs": METRICS.fsck_scan_secs.load(Ordering::Relaxed),
                "scrub_blocks_scanned": METRICS.scrub_blocks_scanned.load(Ordering::Relaxed),
                "scrub_bytes_scanned": METRICS.scrub_bytes_scanned.load(Ordering::Relaxed),
                "scrub_aead_verified": METRICS.scrub_aead_verified.load(Ordering::Relaxed),
                "scrub_frame_verified": METRICS.scrub_frame_verified.load(Ordering::Relaxed),
                "scrub_readability_only": METRICS.scrub_readability_only.load(Ordering::Relaxed),
                "scrub_failures": METRICS.scrub_failures.load(Ordering::Relaxed),
                // PR VL6b repair family (§5.6a / §10).
                "fsck_repairs_planned": METRICS.fsck_repairs_planned.load(Ordering::Relaxed),
                "fsck_repairs_applied": METRICS.fsck_repairs_applied.load(Ordering::Relaxed),
                "fsck_repairs_refused": METRICS.fsck_repairs_refused.load(Ordering::Relaxed),
                "fsck_quarantined_records": METRICS.fsck_quarantined_records.load(Ordering::Relaxed),
                "fsck_quarantined_blocks": METRICS.fsck_quarantined_blocks.load(Ordering::Relaxed),
                "fsck_quarantined_bytes": METRICS.fsck_quarantined_bytes.load(Ordering::Relaxed),
                "fsck_repair_classC1": METRICS.fsck_repair_class[0].load(Ordering::Relaxed),
                "fsck_repair_classC2": METRICS.fsck_repair_class[1].load(Ordering::Relaxed),
                "fsck_repair_classC3": METRICS.fsck_repair_class[2].load(Ordering::Relaxed),
                "fsck_repair_classC4": METRICS.fsck_repair_class[3].load(Ordering::Relaxed),
                "fsck_repair_classC5": METRICS.fsck_repair_class[4].load(Ordering::Relaxed),
                "fsck_repair_classC6": METRICS.fsck_repair_class[5].load(Ordering::Relaxed),
                "fsck_repair_classC7": METRICS.fsck_repair_class[6].load(Ordering::Relaxed),
                // PR VL7 defrag family (§5.7 / §10, KD-11). Ratio gauges
                // decode permille+1 (null = never measured); worst-volume
                // semantics — per-volume rows ride `defrag --report-only`.
                "defrag_blocks_moved": METRICS.defrag_blocks_moved.load(Ordering::Relaxed),
                "defrag_bytes_moved": METRICS.defrag_bytes_moved.load(Ordering::Relaxed),
                "defrag_folds_kicked": METRICS.defrag_folds_kicked.load(Ordering::Relaxed),
                "defrag_meta_compactions_kicked": METRICS.defrag_meta_compactions_kicked.load(Ordering::Relaxed),
                "frag_d1_contiguity": crate::defrag::decode_ratio(METRICS.frag_d1_contiguity.load(Ordering::Relaxed)),
                "frag_d1_reclaimable_tail": crate::defrag::decode_ratio(METRICS.frag_d1_reclaimable_tail.load(Ordering::Relaxed)),
                "frag_d2_locality": crate::defrag::decode_ratio(METRICS.frag_d2_locality.load(Ordering::Relaxed)),
                "frag_d3_pressure_bytes": METRICS.frag_d3_pressure_bytes.load(Ordering::Relaxed),
                "frag_d4_dead_bset_ratio": crate::defrag::decode_ratio(METRICS.frag_d4_dead_bset_ratio.load(Ordering::Relaxed)),
                "job_remote_workers": METRICS.job_remote_workers.load(Ordering::Relaxed),
                "job_remote_enrollments": METRICS.job_remote_enrollments.load(Ordering::Relaxed),
                "job_remote_enroll_refused": METRICS.job_remote_enroll_refused.load(Ordering::Relaxed),
                "job_remote_shards": METRICS.job_remote_shards.load(Ordering::Relaxed),
                "job_remote_submissions": METRICS.job_remote_submissions.load(Ordering::Relaxed),
                "job_remote_refused_stale": METRICS.job_remote_refused_stale.load(Ordering::Relaxed),
                "job_remote_lease_expiries": METRICS.job_remote_lease_expiries.load(Ordering::Relaxed),
                "job_remote_reassignments": METRICS.job_remote_reassignments.load(Ordering::Relaxed),
                "job_remote_bytes_moved": METRICS.job_remote_bytes_moved.load(Ordering::Relaxed),
                "job_remote_verify_read_bytes": METRICS.job_remote_verify_read_bytes.load(Ordering::Relaxed),
                "job_remote_quarantined_destinations": METRICS.job_remote_quarantined_destinations.load(Ordering::Relaxed),
                "job_remote_pr_preempts": METRICS.job_remote_pr_preempts.load(Ordering::Relaxed),
                // The §5.1.6 guarantee-class gauge, exported as its class
                // name (the writer_guard_mode precedent).
                "job_remote_fence_mode": if METRICS.job_remote_fence_mode.load(Ordering::Relaxed) == 1 { "pr" } else { "deferred-reclaim" },
                "writeback_superseded_noops": METRICS.writeback_superseded_noops.load(Ordering::Relaxed),
                "writeback_stale_token_retries": METRICS.writeback_stale_token_retries.load(Ordering::Relaxed),
                "writeback_orphan_discards": METRICS.writeback_orphan_discards.load(Ordering::Relaxed),
                "active_block_cow_copies": METRICS.active_block_cow_copies.load(Ordering::Relaxed),
                "write_through_blocks": METRICS.write_through_blocks.load(Ordering::Relaxed),
                "write_through_bytes": METRICS.write_through_bytes.load(Ordering::Relaxed),
                "write_through_fallbacks": METRICS.write_through_fallbacks.load(Ordering::Relaxed),
                // Terminal-free reclaim economy (shim-write-amplification
                // fix): reclaims must never surface as device WRITE bytes.
                "block_free_discards": METRICS.block_free_discards.load(Ordering::Relaxed),
                "block_free_discard_bytes": METRICS.block_free_discard_bytes.load(Ordering::Relaxed),
                "block_free_file_punches": METRICS.block_free_file_punches.load(Ordering::Relaxed),
                "block_free_punch_bytes": METRICS.block_free_punch_bytes.load(Ordering::Relaxed),
                "block_free_reclaim_skipped": METRICS.block_free_reclaim_skipped.load(Ordering::Relaxed),
                "block_free_reclaim_queued": METRICS.block_free_reclaim_queued.load(Ordering::Relaxed),
                "block_free_reclaim_queue_bytes": METRICS.block_free_reclaim_queue_bytes.load(Ordering::Relaxed),
                "block_free_reclaim_batches": METRICS.block_free_reclaim_batches.load(Ordering::Relaxed),
                "block_free_reclaim_sync_drains": METRICS.block_free_reclaim_sync_drains.load(Ordering::Relaxed),
                "block_free_reclaim_fence_halts": METRICS.block_free_reclaim_fence_halts.load(Ordering::Relaxed),
                // RW1 rand-write device-byte ledger (design-random-small-
                // writes §1.2 buckets; always-on — the G-RW2 gate's
                // attribution source).
                "spill_seed_reads": METRICS.spill_seed_reads.load(Ordering::Relaxed),
                "spill_seed_read_bytes": METRICS.spill_seed_read_bytes.load(Ordering::Relaxed),
                "spill_staging_puts": METRICS.spill_staging_puts.load(Ordering::Relaxed),
                "spill_staging_put_bytes": METRICS.spill_staging_put_bytes.load(Ordering::Relaxed),
                "flush_seed_read_bytes": METRICS.flush_seed_read_bytes.load(Ordering::Relaxed),
                "write_path_seed_read_bytes": METRICS.write_path_seed_read_bytes.load(Ordering::Relaxed),
                "staging_put_bytes_drain": METRICS.staging_put_bytes_drain.load(Ordering::Relaxed),
                "staging_put_bytes_flush": METRICS.staging_put_bytes_flush.load(Ordering::Relaxed),
                "staging_put_bytes_teardown": METRICS.staging_put_bytes_teardown.load(Ordering::Relaxed),
                "staging_put_bytes_wt_fallback": METRICS.staging_put_bytes_wt_fallback.load(Ordering::Relaxed),
                "writeback_enqueued_drain": METRICS.writeback_enqueued_drain.load(Ordering::Relaxed),
                "writeback_enqueued_flush": METRICS.writeback_enqueued_flush.load(Ordering::Relaxed),
                "writeback_enqueued_teardown": METRICS.writeback_enqueued_teardown.load(Ordering::Relaxed),
                "writeback_enqueued_wt_fallback": METRICS.writeback_enqueued_wt_fallback.load(Ordering::Relaxed),
                "durable_upload_bytes_writeback": METRICS.durable_upload_bytes_writeback.load(Ordering::Relaxed),
                "durable_upload_bytes_self_flush": METRICS.durable_upload_bytes_self_flush.load(Ordering::Relaxed),
                "durable_upload_bytes_escalation": METRICS.durable_upload_bytes_escalation.load(Ordering::Relaxed),
                "restage_churn_removes": METRICS.restage_churn_removes.load(Ordering::Relaxed),
                "restage_churn_bytes": METRICS.restage_churn_bytes.load(Ordering::Relaxed),
                "write_block_revisits": METRICS.write_block_revisits.load(Ordering::Relaxed),
                "staging_sibling_probes": METRICS.staging_sibling_probes.load(Ordering::Relaxed),
                "aligned_pool_misses": METRICS.aligned_pool_misses.load(Ordering::Relaxed),
                "aligned_pool_hits": METRICS.aligned_pool_hits.load(Ordering::Relaxed),
                // RW2 W1 sole-owner extent patch families (design-random-
                // small-writes §5.4 — the decision ledger + gate tripwires).
                "patch_writes": METRICS.patch_writes.load(Ordering::Relaxed),
                "patch_write_bytes": METRICS.patch_write_bytes.load(Ordering::Relaxed),
                "patch_edge_rmw_reads": METRICS.patch_edge_rmw_reads.load(Ordering::Relaxed),
                "patch_ineligible_unmapped": METRICS.patch_ineligible_unmapped.load(Ordering::Relaxed),
                "patch_ineligible_decorated": METRICS.patch_ineligible_decorated.load(Ordering::Relaxed),
                "patch_ineligible_unaligned": METRICS.patch_ineligible_unaligned.load(Ordering::Relaxed),
                "patch_ineligible_overlay": METRICS.patch_ineligible_overlay.load(Ordering::Relaxed),
                "patch_ineligible_shared": METRICS.patch_ineligible_shared.load(Ordering::Relaxed),
                "patch_ineligible_transform": METRICS.patch_ineligible_transform.load(Ordering::Relaxed),
                "patch_ineligible_adjacent": METRICS.patch_ineligible_adjacent.load(Ordering::Relaxed),
                "patch_ineligible_oversize": METRICS.patch_ineligible_oversize.load(Ordering::Relaxed),
                "patch_dma_errors": METRICS.patch_dma_errors.load(Ordering::Relaxed),
                // RW4 W2 extent overlay / spill records / batched fold
                // families (design-random-small-writes §5.2 / §5.4).
                "parked_extent_bytes": METRICS.parked_extent_bytes.load(Ordering::Relaxed),
                "parked_full_buffer_bytes": METRICS.parked_full_buffer_bytes.load(Ordering::Relaxed),
                "extent_parks": METRICS.extent_parks.load(Ordering::Relaxed),
                "extent_escalations": METRICS.extent_escalations.load(Ordering::Relaxed),
                "extent_implicit_escalations": METRICS.extent_implicit_escalations.load(Ordering::Relaxed),
                "extent_spills": METRICS.extent_spills.load(Ordering::Relaxed),
                "extent_spill_bytes": METRICS.extent_spill_bytes.load(Ordering::Relaxed),
                "extent_record_absorbs": METRICS.extent_record_absorbs.load(Ordering::Relaxed),
                "fold_passes": METRICS.fold_passes.load(Ordering::Relaxed),
                "fold_seed_reads": METRICS.fold_seed_reads.load(Ordering::Relaxed),
                "fold_extents_folded": METRICS.fold_extents_folded.load(Ordering::Relaxed),
                "fold_fill": METRICS.fold_fill.to_json(),
                "staged_rider_extent_writes": METRICS.staged_rider_extent_writes.load(Ordering::Relaxed),
                "staged_rider_folds": METRICS.staged_rider_folds.load(Ordering::Relaxed),
                "extent_records_recovered": METRICS.extent_records_recovered.load(Ordering::Relaxed),
                "extent_records_stale_discarded": METRICS.extent_records_stale_discarded.load(Ordering::Relaxed),
                "extent_records_torn_discarded": METRICS.extent_records_torn_discarded.load(Ordering::Relaxed),
                "extent_records_future_refused": METRICS.extent_records_future_refused.load(Ordering::Relaxed),
                "active_block_memset_elided_bytes": METRICS.active_block_memset_elided_bytes.load(Ordering::Relaxed),
                "active_block_ooo_runs": METRICS.active_block_ooo_runs.load(Ordering::Relaxed),
                "meta_device_syncs": METRICS.meta_device_syncs.load(Ordering::Relaxed),
                "meta_sync_requests": METRICS.meta_sync_requests.load(Ordering::Relaxed),
                "bg_admit_available_permits": crate::bg_admit::available_permits(),
                "bg_admit_capacity": crate::bg_admit::capacity(),
                "striped_block_concurrency": crate::bg_admit::striped_block_concurrency(),
                "fuse_over_uring_sessions_active": fuse_over_uring_sessions,
                "fuse_over_uring_requests": fou_req,
                "fuse_over_uring_replies": fou_rep,
                "fuse_over_uring_cqe_errors": fou_err,
                "fuse_over_uring_registers": fou_reg,
                "transport_payload_leases": t_leases,
                "transport_parked_commits": t_parked,
                "transport_leases_outstanding": t_outstanding,
                "transport_lease_max_age_ms": t_max_age,
                "transport_classical_sideband": t_classical_sideband,
                "transport_queues": t_queues,
                "transport_q_depth": t_depth,
                "transport_payload_buffer_bytes": t_buffer_bytes,
                "transport_max_background": t_max_background,
                "transport_commit_batch": serde_json::Value::Object(t_cb_hist),
                "transport_commit_batch_flushes": t_cb_flushes,
                "transport_commit_batch_commits": t_cb_commits,
                "transport_wake_writes": t_wake_writes,
                "transport_wakes_elided": t_wakes_elided,
                // PR 6 / N6 (design-nvmeof-target-management §6.9): the
                // fabric_* family — box-wide sysfs controller-state
                // sample published by the 10 s sampler task
                // (`start_mount`). Always exported; zero-valued on
                // fabric-less boxes. `fabric_ctrl_reconnects` is the
                // SAMPLED-transition counter (undercount caveat on the
                // Metrics field doc).
                "fabric_controllers": METRICS.fabric_controllers.load(Ordering::Relaxed),
                "fabric_ctrl_not_live": METRICS.fabric_ctrl_not_live.load(Ordering::Relaxed),
                "fabric_ctrl_reconnects": METRICS.fabric_ctrl_reconnects.load(Ordering::Relaxed),
                // L4 interception session-host families (design-preload-
                // interception §8, PR L4-3): lifecycle, the §5.2 refusal
                // ledger, R5 admission, and the two must-stay-0 tripwires
                // (`ipc_descriptor_rejects`, `ipc_sessions_poisoned`).
                "ipc_sessions_active": METRICS.ipc_sessions_active.load(Ordering::Relaxed),
                "ipc_sessions_total": METRICS.ipc_sessions_total.load(Ordering::Relaxed),
                "ipc_binds": METRICS.ipc_binds.load(Ordering::Relaxed),
                "ipc_bind_refused_version": METRICS.ipc_bind_refused_version.load(Ordering::Relaxed),
                "ipc_bind_refused_nonce": METRICS.ipc_bind_refused_nonce.load(Ordering::Relaxed),
                "ipc_bind_refused_flags": METRICS.ipc_bind_refused_flags.load(Ordering::Relaxed),
                "ipc_bind_refused_mode": METRICS.ipc_bind_refused_mode.load(Ordering::Relaxed),
                "ipc_bind_refused_budget": METRICS.ipc_bind_refused_budget.load(Ordering::Relaxed),
                "ipc_bind_refused_peercred": METRICS.ipc_bind_refused_peercred.load(Ordering::Relaxed),
                "ipc_bind_refused_disabled": METRICS.ipc_bind_refused_disabled.load(Ordering::Relaxed),
                "ipc_binds_dev_override": METRICS.ipc_binds_dev_override.load(Ordering::Relaxed),
                "ipc_admission_refusals": METRICS.ipc_admission_refusals.load(Ordering::Relaxed),
                "ipc_arena_bytes": METRICS.ipc_arena_bytes.load(Ordering::Relaxed),
                "ipc_descriptor_rejects": METRICS.ipc_descriptor_rejects.load(Ordering::Relaxed),
                "ipc_sessions_poisoned": METRICS.ipc_sessions_poisoned.load(Ordering::Relaxed),
                // L4 data plane (§8, PR L4-4): ops/bytes are the charter-
                // rule-4 engagement instrument; the serve/demote split is
                // the fast-path health signal.
                "ipc_ops_read": METRICS.ipc_ops_read.load(Ordering::Relaxed),
                "ipc_ops_write": METRICS.ipc_ops_write.load(Ordering::Relaxed),
                "ipc_bytes_in": METRICS.ipc_bytes_in.load(Ordering::Relaxed),
                "ipc_bytes_out": METRICS.ipc_bytes_out.load(Ordering::Relaxed),
                "ipc_fast_path_serves": METRICS.ipc_fast_path_serves.load(Ordering::Relaxed),
                "ipc_async_handoffs": METRICS.ipc_async_handoffs.load(Ordering::Relaxed),
                "ipc_fast_path_lock_demotions": METRICS.ipc_fast_path_lock_demotions.load(Ordering::Relaxed),
                "ipc_fast_path_miss_demotions": METRICS.ipc_fast_path_miss_demotions.load(Ordering::Relaxed),
                "ipc_service_threads": METRICS.ipc_service_threads.load(Ordering::Relaxed),
                "ipc_service_parks": METRICS.ipc_service_parks.load(Ordering::Relaxed),
                "ipc_sessions_reaped": METRICS.ipc_sessions_reaped.load(Ordering::Relaxed),
                "ipc_inval_notifies": METRICS.ipc_inval_notifies.load(Ordering::Relaxed),
                "ipc_inval_suppressed": METRICS.ipc_inval_suppressed.load(Ordering::Relaxed),
                // DIALED P1 direct-drive (perf/ipc-direct-drive): the
                // governed miss shape served from the ipc-host uring +
                // the prelude decision ledger.
                "ipc_direct_drive_submits": METRICS.ipc_direct_drive_submits.load(Ordering::Relaxed),
                "ipc_direct_drive_serves": METRICS.ipc_direct_drive_serves.load(Ordering::Relaxed),
                "ipc_direct_drive_bounces": METRICS.ipc_direct_drive_bounces.load(Ordering::Relaxed),
                "ipc_direct_drive_fallbacks_post": METRICS.ipc_direct_drive_fallbacks_post.load(Ordering::Relaxed),
                "ipc_direct_ineligible_shape": METRICS.ipc_direct_ineligible_shape.load(Ordering::Relaxed),
                "ipc_direct_ineligible_meta": METRICS.ipc_direct_ineligible_meta.load(Ordering::Relaxed),
                "ipc_direct_ineligible_layout": METRICS.ipc_direct_ineligible_layout.load(Ordering::Relaxed),
                "ipc_direct_ineligible_overlay": METRICS.ipc_direct_ineligible_overlay.load(Ordering::Relaxed),
                "ipc_direct_ineligible_backend": METRICS.ipc_direct_ineligible_backend.load(Ordering::Relaxed),
                "ipc_direct_ineligible_policy": METRICS.ipc_direct_ineligible_policy.load(Ordering::Relaxed),
                "write_lock_wait": METRICS.write_lock_wait.to_json(),
                "block_lock_wait": METRICS.block_lock_wait.to_json(),
                "lease_lock_wait": METRICS.lease_lock_wait.to_json(),
                "dlm_acquire_time": METRICS.dlm_acquire_time.to_json(),
                "writeback_queue_depth": METRICS.writeback_queue_depth.to_json(),
                "meta_flush_deferred": METRICS.meta_flush_deferred.load(Ordering::Relaxed),
                "meta_reclaim_batch_size": METRICS.meta_reclaim_batch_size.to_json(),
                // D4.a fill-vs-window attribution (design-metadata-
                // throughput §5.4): gather fill + close reasons decide
                // between the batch-fill degeneration suspects from
                // `.stats` alone.
                "meta_reclaim_gather_fill": METRICS.meta_reclaim_gather_fill.to_json(),
                "meta_reclaim_gather_cap_closes": METRICS.meta_reclaim_gather_cap_closes.load(Ordering::Relaxed),
                "meta_reclaim_gather_window_closes": METRICS.meta_reclaim_gather_window_closes.load(Ordering::Relaxed),
                "meta_reclaim_gather_channel_closes": METRICS.meta_reclaim_gather_channel_closes.load(Ordering::Relaxed),
                // Per-volume atomicity fields (design §Observability —
                // live signals over ad-hoc logging). Resolved OQ 2
                // (design-cow-kv-metadata §4.10): TWO fields per volume —
                // `meta_volume_atomicity` is the contract class
                // ("cow-checksummed" by construction) and
                // `meta_volume_atomicity_physical` is the hardware probe,
                // kept alongside for operator visibility.
                "meta_volume_atomicity": self
                    .meta_backend
                    .as_ref()
                    .map(|mb| {
                        mb.volumes
                            .iter()
                            .map(|v| v.atomicity_contract())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
                "meta_volume_atomicity_physical": self
                    .meta_backend
                    .as_ref()
                    .map(|mb| {
                        mb.volumes
                            .iter()
                            .map(|v| v.atomicity_physical())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
                // Constant "3" per volume (v3 is the only metadata
                // format); kept as a field because operators key on it.
                "meta_format_version": self
                    .meta_backend
                    .as_ref()
                    .map(|mb| {
                        mb.volumes
                            .iter()
                            .map(|_| "3".to_string())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
            },
            "cache_capacities": {
                "read_lru_current_bytes": self.router.cache.read_lru.current_bytes(),
                "read_lru_max_bytes": self.router.cache.read_lru.max_bytes(),
                "write_lru_current_bytes": self.router.cache.write_lru.current_bytes(),
                "write_lru_max_bytes": self.router.cache.write_lru.max_bytes(),
                "nvme_staging_current_bytes": self.router.cache.nvme.current_staged_write_bytes(),
                "nvme_staging_max_bytes": self.router.cache.nvme.max_write_bytes(),
                "nvme_read_cache_current_bytes": self.router.cache.nvme.current_read_cache_bytes(),
                "nvme_read_cache_max_bytes": self.router.cache.nvme.max_read_bytes(),
            },
            "internal_caches": {
                "metadata_cache_size": self.router.metadata_cache.entry_count(),
            }
        });

        // PR K7 (design §10): format-scoped metric families (see has_v2 /
        // has_v3 above). Inserted post-macro so each family exists only
        // when a volume of its format is actually mounted.
        {
            use crate::meta_backend::kv as meta_kv;
            let metrics = stats_obj
                .get_mut("metrics")
                .and_then(|m| m.as_object_mut())
                .expect("stats JSON carries a metrics object");
            if has_meta {
                let load = |c: &std::sync::atomic::AtomicU64| -> serde_json::Value {
                    c.load(Ordering::Relaxed).into()
                };
                metrics.insert(
                    "meta_kv_node_cache_hits".into(),
                    load(&meta_kv::META_KV_NODE_CACHE_HITS),
                );
                metrics.insert(
                    "meta_kv_node_cache_misses".into(),
                    load(&meta_kv::META_KV_NODE_CACHE_MISSES),
                );
                metrics.insert(
                    "meta_kv_node_cache_evictions".into(),
                    load(&meta_kv::META_KV_NODE_CACHE_EVICTIONS),
                );
                metrics.insert(
                    "meta_kv_node_appends".into(),
                    load(&meta_kv::META_KV_NODE_APPENDS),
                );
                metrics.insert(
                    "meta_kv_node_append_bytes".into(),
                    load(&meta_kv::META_KV_NODE_APPEND_BYTES),
                );
                metrics.insert(
                    "meta_kv_node_rewrite_bytes".into(),
                    load(&meta_kv::META_KV_NODE_REWRITE_BYTES),
                );
                metrics.insert(
                    "meta_kv_node_compactions".into(),
                    load(&meta_kv::META_KV_NODE_COMPACTIONS),
                );
                metrics.insert(
                    "meta_kv_node_splits".into(),
                    load(&meta_kv::META_KV_NODE_SPLITS),
                );
                metrics.insert(
                    "meta_kv_journal_bytes".into(),
                    load(&meta_kv::META_KV_JOURNAL_BYTES),
                );
                metrics.insert(
                    "meta_kv_journal_entries".into(),
                    load(&meta_kv::META_KV_JOURNAL_ENTRIES),
                );
                // PR M7 (design-metadata-throughput §5.5/§9): conveyor
                // group formation — txs per batch (exact 1–8 buckets so
                // the G3 median ≥ 4 gate reads straight off the stats
                // surface), batch bytes vs the caps, pass count, and the
                // panic-guard tripwire (> 0 ⇒ a batch failed LOUD; never
                // a silent completed_upto wedge).
                metrics.insert(
                    "meta_commit_group_size".into(),
                    serde_json::Value::Object(
                        crate::meta_backend::kv::COMMIT_GROUP_SIZE_LABELS
                            .iter()
                            .zip(meta_kv::META_COMMIT_GROUP_SIZE.snapshot())
                            .map(|(label, n)| (label.to_string(), serde_json::Value::from(n)))
                            .collect(),
                    ),
                );
                metrics.insert(
                    "meta_commit_group_size_median_lb".into(),
                    serde_json::Value::from(
                        meta_kv::META_COMMIT_GROUP_SIZE
                            .median_lower_bound()
                            .unwrap_or(0),
                    ),
                );
                metrics.insert(
                    "meta_commit_group_bytes".into(),
                    load(&meta_kv::META_COMMIT_GROUP_BYTES),
                );
                metrics.insert(
                    "meta_conveyor_leader_passes".into(),
                    load(&meta_kv::META_CONVEYOR_LEADER_PASSES),
                );
                metrics.insert(
                    "meta_conveyor_pass_panics".into(),
                    load(&meta_kv::META_CONVEYOR_PASS_PANICS),
                );
                metrics.insert(
                    "meta_kv_checkpoints".into(),
                    load(&meta_kv::META_KV_CHECKPOINTS),
                );
                // Option A — pending-free coverage (design-smo-replay-
                // currency §2-A): FIFO entry/exit counters; parked −
                // released ≈ the per-volume `meta_kv_pending_free` sum
                // (the backlog gauge — ≈ 1.4 % of the 65,536 cap at the
                // measured 939-SMO/s storm rate). `parked` outrunning
                // `released` on a quiet mount = a wedged coverage tail.
                metrics.insert(
                    "meta_kv_pending_free_parked".into(),
                    load(&meta_kv::META_KV_PENDING_FREE_PARKED),
                );
                metrics.insert(
                    "meta_kv_pending_free_released".into(),
                    load(&meta_kv::META_KV_PENDING_FREE_RELEASED),
                );
                metrics.insert(
                    "meta_kv_pending_free_overflow".into(),
                    load(&meta_kv::META_KV_PENDING_FREE_OVERFLOW),
                );
                metrics.insert(
                    "meta_kv_commit_smo_retries".into(),
                    load(&meta_kv::META_KV_COMMIT_SMO_RETRIES),
                );
                metrics.insert(
                    "meta_kv_node_dropped_tail_bsets".into(),
                    load(&meta_kv::META_KV_NODE_DROPPED_TAIL_BSETS),
                );
                metrics.insert(
                    "meta_kv_dentry_collision_overflows".into(),
                    load(&meta_kv::META_KV_DENTRY_COLLISION_OVERFLOWS),
                );
                metrics.insert(
                    "meta_kv_delta_orphans".into(),
                    load(&meta_kv::META_KV_DELTA_ORPHANS),
                );
                // PR M9 (design-metadata-throughput §5.7 D7 / §9):
                // record-fold slimming — overlay-head serves (D7.a),
                // snapshot fold-memo hit rate (D7.b; the create-storm hit
                // rate is the acceptance number), and the memo-bytes
                // gauge that rides the node-cache budget (the tiny-budget
                // storm gate reads it).
                metrics.insert(
                    "meta_kv_fold_head_serves".into(),
                    load(&meta_kv::META_KV_FOLD_HEAD_SERVES),
                );
                metrics.insert(
                    "meta_kv_fold_memo_hits".into(),
                    load(&meta_kv::META_KV_FOLD_MEMO_HITS),
                );
                metrics.insert(
                    "meta_kv_fold_memo_misses".into(),
                    load(&meta_kv::META_KV_FOLD_MEMO_MISSES),
                );
                metrics.insert(
                    "meta_kv_fold_memo_bytes".into(),
                    load(&meta_kv::META_KV_FOLD_MEMO_BYTES),
                );
                // PR M6 (design-metadata-throughput §5.4 D4): the
                // SETATTR-echo absorber — echoes absorbed with zero
                // entries, batched drain commits (the amortized residual
                // an M7 conveyor would inherit), refinements drained.
                metrics.insert(
                    "meta_kv_times_echo_absorbed".into(),
                    load(&meta_kv::META_KV_TIMES_ECHO_ABSORBED),
                );
                metrics.insert(
                    "meta_kv_times_echo_drain_commits".into(),
                    load(&meta_kv::META_KV_TIMES_ECHO_DRAIN_COMMITS),
                );
                metrics.insert(
                    "meta_kv_times_echo_drained".into(),
                    load(&meta_kv::META_KV_TIMES_ECHO_DRAINED),
                );
                // PR M2 (design-metadata-throughput §5.4 D4.a): journal
                // entries per commit_tx construction site — the named-
                // committer decomposition of `meta_kv_journal_entries`
                // (the OQ-1 attribution surface).
                metrics.insert(
                    "meta_kv_commit_sites".into(),
                    serde_json::Value::Object(
                        meta_kv::commit_sites_snapshot()
                            .into_iter()
                            .map(|(site, n)| (site, serde_json::Value::from(n)))
                            .collect(),
                    ),
                );
                // Per-volume gauges (mount-scoped replay stats, allocator
                // occupancy, ring-admission parks), arrays parallel to
                // `meta_format_version`.
                let per_volume = |f: &dyn Fn(
                    &crate::meta_backend::kv::backend::KvMetaBackend,
                ) -> u64|
                 -> serde_json::Value {
                    self.meta_backend
                        .as_ref()
                        .map(|mb| {
                            serde_json::Value::Array(
                                mb.volumes.iter().map(|be| f(be).into()).collect(),
                            )
                        })
                        .unwrap_or_default()
                };
                metrics.insert(
                    "meta_kv_replay_entries".into(),
                    per_volume(&|be| be.replay_stats().entries),
                );
                // PR M6: refinements currently parked (per volume) — a
                // growing gauge on a quiet mount means the drain task
                // stalled.
                metrics.insert(
                    "meta_kv_times_echo_pending".into(),
                    per_volume(&|be| be.pending_times_len() as u64),
                );
                metrics.insert(
                    "meta_kv_replay_dropped_torn".into(),
                    per_volume(&|be| be.replay_stats().dropped_torn),
                );
                metrics.insert(
                    "meta_kv_replay_ms".into(),
                    per_volume(&|be| be.replay_stats().replay_ms),
                );
                metrics.insert(
                    "meta_kv_journal_full_stalls".into(),
                    per_volume(&|be| be.journal_full_stalls()),
                );
                metrics.insert(
                    "meta_kv_free_extents".into(),
                    per_volume(&|be| be.free_extents()),
                );
                metrics.insert(
                    "meta_kv_pending_free".into(),
                    per_volume(&|be| be.pending_free_extents()),
                );
                // PR M1 — the single-writer mount guard (design
                // §9 Observability): per-volume guarantee class + the
                // fence / PTPL-reacquire counters.
                metrics.insert(
                    "writer_guard_mode".into(),
                    self.meta_backend
                        .as_ref()
                        .map(|mb| {
                            serde_json::Value::Array(
                                mb.volumes
                                    .iter()
                                    .map(|v| v.writer_guard_mode().into())
                                    .collect(),
                            )
                        })
                        .unwrap_or_default(),
                );
                metrics.insert(
                    "writer_guard_fenced".into(),
                    per_volume(&|be| be.writer_guard_fenced()),
                );
                metrics.insert(
                    "writer_guard_pr_reacquires".into(),
                    per_volume(&|be| be.writer_guard_pr_reacquires()),
                );
            }
            // PR M2 (design-metadata-throughput §5.1/§9): the D1.a rig's
            // fields exist only when the rig is armed — a disabled mount's
            // stats surface is byte-identical to pre-M2 (the zero-cost
            // contract extends to the JSON).
            if op_profile_enabled() {
                metrics.insert("fuse_op_phase_ns".into(), op_profile_phase_json());
                metrics.insert(
                    "fuse_create_under_lock_ns".into(),
                    op_profile_under_lock_json(),
                );
                metrics.insert(
                    "fuse_op_profile_inflight".into(),
                    op_profile_inflight().into(),
                );
                // PR RW1 (design-random-small-writes §5.3/§5.4): the write
                // rig's families ride the same armed-only contract — a
                // disabled mount's stats surface stays byte-identical.
                metrics.insert("fuse_write_phase_ns".into(), write_profile_phase_json());
                metrics.insert("block_lock_wait_by_site".into(), block_lock_site_json());
                metrics.insert(
                    "block_lock_stripe_audit".into(),
                    block_lock_stripe_audit_json(),
                );
                metrics.insert("fuse_write_inflight".into(), write_inflight_json());
            }
        }

        finish_virtual_payload(serde_json::to_string_pretty(&stats_obj).unwrap_or_default())
    }

    fn get_config_attr(&self, size: u64) -> FileAttr {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let sec = now.as_secs() as i64;
        let nsec = now.subsec_nanos();

        FileAttr {
            ino: CONFIG_INODE,
            size,
            blocks: size.div_ceil(512),
            atime: Timestamp::new(sec, nsec),
            mtime: Timestamp::new(sec, nsec),
            ctime: Timestamp::new(sec, nsec),
            kind: FileType::RegularFile,
            perm: 0o444, // read-only by all
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
        }
    }

    /// Current readdir-snapshot generation for a directory (PR M4 D1.c).
    /// 0 until the first entry-set mutation; monotonic per mutation.
    /// Latch-free read (`scc` bucket read, no allocation); `Acquire`
    /// pairs with the mutators' `Release` bump.
    pub fn dir_generation(&self, dir: u64) -> u64 {
        self.dir_gen
            .read_sync(&dir, |_, g| g.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    /// Bump a directory's readdir-snapshot generation (PR M4 D1.c) — the
    /// allocation-free replacement for the per-op
    /// `dir_entry_cache_v3.invalidate(&parent)`: an `scc` bucket read +
    /// one relaxed-cost `fetch_add` for already-seen parents (one-time
    /// entry insert per directory, the `note_commit_site` pattern).
    /// Called by every entry-set mutation AFTER its backend commit
    /// returns, so any snapshot whose build began before the mutation
    /// lands under the superseded generation and dies by key mismatch.
    fn bump_dir_generation(&self, dir: u64) {
        if self
            .dir_gen
            .read_sync(&dir, |_, g| {
                g.fetch_add(1, Ordering::Release);
            })
            .is_none()
        {
            match self.dir_gen.entry_sync(dir) {
                scc::hash_map::Entry::Occupied(o) => {
                    o.get().fetch_add(1, Ordering::Release);
                }
                scc::hash_map::Entry::Vacant(v) => {
                    v.insert_entry(std::sync::atomic::AtomicU64::new(1));
                }
            }
        }
    }

    pub fn get_inode_lock(&self, ino: u64) -> &tokio::sync::RwLock<()> {
        self.active_inode_locks.get_inode_lock(ino)
    }

    /// The two-inode guard acquisition SEQUENCE for `copy_file_range` (the
    /// only two-ino guard site): returns `(first, second)` — the ino whose
    /// guard is taken first, then the other. Deadlock freedom over the
    /// hash-STRIPED `active_inode_locks` requires a total order on the
    /// LOCK INSTANCES actually acquired (the shard indexes), because two
    /// distinct ino pairs can map onto the same two stripes in opposite
    /// raw-ino order (the ABBA the VL8 item-2 wedge capture implicated:
    /// two copy_file_range handlers permanently in flight, watchdog-blind,
    /// with every visible victim queued behind striped inode guards).
    ///
    /// Order: ascending SHARD INDEX (the total order on the lock
    /// instances); raw ino only tie-breaks equal shards for determinism —
    /// the caller's `same_lock`/`ptr_eq` collapse already handles the
    /// equal-shard case with a single exclusive guard before consulting
    /// this sequence.
    pub fn inode_pair_lock_order(&self, src: u64, dst: u64) -> (u64, u64) {
        let s_src = self.active_inode_locks.shard_index(src);
        let s_dst = self.active_inode_locks.shard_index(dst);
        if s_src < s_dst || (s_src == s_dst && src <= dst) {
            (src, dst)
        } else {
            (dst, src)
        }
    }

    pub fn get_inode_lock_ref(&self, ino: u64) -> &tokio::sync::RwLock<()> {
        self.active_inode_locks.get_inode_lock(ino)
    }

    /// fstests generic/683 family (VL10 release gate): an UNPRIVILEGED
    /// data-modifying fallocate (prealloc/punch/zero — every arm) drops
    /// suid+sgid, both bits regardless of exec bits (the 5.19-era vfs
    /// setgid-series law the 683 golden encodes). The kernel's killpriv
    /// machinery covers write/truncate and chmods suid away itself, but
    /// FUSE fallocate leaves the strip to the daemon — without this the
    /// non-group-exec sgid survived (6666 → 2666, not 666). Idempotent
    /// with any kernel-sent killpriv chmod; root (uid 0) keeps its bits.
    async fn strip_setid_after_unprivileged_datamod(
        &self,
        req: &Request,
        ino: u64,
    ) -> FuseResult<()> {
        if req.uid == 0 {
            return Ok(());
        }
        let backend = self
            .meta_backend
            .as_ref()
            .expect("meta_backend must be configured");
        let inode = backend.getattr(ino).await.map_err(map_squeezefs_err)?;
        if inode.mode & 0o6000 == 0 {
            return Ok(());
        }
        backend
            .setattr(
                ino,
                Some(inode.mode & !0o6000),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .map_err(map_squeezefs_err)?;
        self.refresh_attr_cache(ino).await;
        // The kernel's incore mode still shows the pre-strip bits for an
        // attr-TTL window (fallocate replies carry no attrs) — push an
        // attrs-only INVAL_INODE so the very next stat refetches. Awaited
        // inline: the reply to this FALLOCATE then strictly follows the
        // invalidation. Absent handle (in-process tests) skips.
        if let Some(notify) = self.kernel_notify.load().as_ref() {
            let notify = notify.clone();
            notify.invalid_inode(ino, 0, 0).await;
        }
        Ok(())
    }

    /// PR L4-4 (§5.5.1): the read handler's guarded **hit-path** probe
    /// sequence, factored for the IPC sync fast path. MUST be called with
    /// this inode's read guard held (the service thread's `try_read()`).
    ///
    /// Mirrors the short critical section at the top of [`Filesystem::
    /// read`] — attr-cache size, `metadata_cache` size override (the
    /// load-bearing stale-size guard), EOF bound, single-block
    /// active-buffer probe — but **synchronously only**: every shape whose
    /// serve needs an await (the in-guard attr fallback, multi-block,
    /// extent overlays, deferred seeds, backend/tier reads) reports
    /// [`IpcReadProbe::Miss`], and the caller demotes per the
    /// drop-guard-before-enqueue rule. Behavior parity with the handler on
    /// the shapes it does serve is pinned by `tests/preload_parity_tests.rs`.
    /// DIALED P1 direct-drive prelude (`docs/design-preload-interception.md`
    /// §5.5/§12 fallback shape): decide — synchronously, from
    /// RAM-authoritative state only, taking NO inode guards and NO node
    /// locks — whether a governed ranged read may be submitted directly
    /// on the ipc-host uring, and capture the 795 custody snapshot that
    /// must revalidate at the CQE. Any miss ⇒ the caller falls back to
    /// the existing handler path (fallback-is-correctness).
    pub fn ipc_direct_read_probe(
        &self,
        ino: u64,
        offset: u64,
        len: u32,
    ) -> Result<IpcDirectSnapshot, IpcDirectIneligible> {
        use IpcDirectIneligible as I;
        // Device-class shape: 4–64 KiB (the R3 ranged window class the
        // 64 KiB bounce pool sizes; sub-4 KiB and jumbo shapes are not
        // the governed miss shape).
        if !(4096..=64 * 1024).contains(&(len as u64)) {
            return Err(I::Shape);
        }
        // Ranged window reads decode nothing — passthrough volumes only
        // (the handler's own ranged-leg gate; transform volumes must
        // fetch whole stored images).
        if !self.router.get_crypto().is_passthrough() {
            return Err(I::Meta);
        }
        // RAM-authoritative metadata only. The RAM entry is the binding
        // authority on a live mount (the write path updates it
        // synchronously; D0 excludes remote writers) — exactly the
        // handler's `current_block_binding` source. Not resident ⇒
        // fall back (the honest map-resident-majority split).
        let Some(meta) = self.router.metadata_cache.get(&ino) else {
            return Err(I::Meta);
        };
        if meta.file_type != "striped" {
            return Err(I::Meta);
        }
        // Strict in-bounds: EOF-clipping shapes keep the handler's
        // short-read/zero-fill semantics.
        let Some(end) = offset.checked_add(u64::from(len)) else {
            return Err(I::Shape);
        };
        if end > meta.size {
            return Err(I::Shape);
        }
        let block_size = self.router.block_size.load(Ordering::Relaxed);
        if block_size == 0 || block_size % 4096 != 0 {
            return Err(I::Shape);
        }
        let b = offset / block_size;
        if (end - 1) / block_size != b {
            return Err(I::Shape); // multi-block
        }
        let b32 = b as u32;
        let Some(map) = meta.block_map.as_ref() else {
            // Indirect / not-RAM-resident maps: sibling shapes stay on
            // the handler (recorded split — never force a partial
            // design to claim the whole shape).
            return Err(I::Meta);
        };
        let Some(key) = map.get(&b32) else {
            return Err(I::Layout); // hole block — handler serves zeros
        };
        if !crate::routing::is_whole_block_mapping(key) {
            return Err(I::Layout); // decorated `bk:off:len` mapping
        }
        // Overlay screen — ANY overlay presence ⇒ handler (correctness
        // owns ambiguity). The O(1) gate first (the capture_parked_runs
        // discipline: never a map scan when no overlay exists).
        let file_path = crate::keys::inode_path(ino);
        let cache_key = crate::keys::active_block_for_path(&file_path, b32).to_string();
        if self
            .parked_overlay_count
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
            && self.active_block_buffers.contains_key(&cache_key)
        {
            return Err(I::Overlay);
        }
        // Staged sibling (the striped block's staging-ring image) — the
        // handler probes it before any device read; so must we.
        if self
            .router
            .cache
            .nvme
            .read_staged_zero_copy(&cache_key)
            .is_some()
        {
            return Err(I::Overlay);
        }
        // W2 staged extent record (newer than every base tier).
        let ext_key = crate::keys::active_block_ext_for_path(&file_path, b32).to_string();
        if self.router.cache.nvme.has_staged_extent_record(&ext_key) {
            return Err(I::Overlay);
        }
        // The 795 custody snapshot: epoch + binding + fill incarnation.
        let epoch = block_custody_epoch(ino, b32);
        let tracked = self.router.backend_router.key_incarnation_tracked(key);
        let incarnation = if tracked {
            match self.router.backend_router.fill_incarnation(key) {
                Some(v) => Some(v),
                // Unstable incarnation word = a patch/free is mid-flight
                // on the block — movement, not a stable serve source.
                None => return Err(I::Overlay),
            }
        } else {
            None
        };
        Ok(IpcDirectSnapshot {
            ino,
            block: b32,
            key: key.clone(),
            epoch,
            tracked,
            incarnation,
            offset,
            len,
            cache_key,
            ext_key,
        })
    }

    /// The CQE-side half of the 795 protocol for direct-drive: `true`
    /// only if the prelude snapshot still binds — same RAM authorities,
    /// same lock-free posture. `false` ⇒ the op must fall back to the
    /// handler (which re-runs the full moving-custody read protocol).
    /// The epoch equality proves no custody TRANSFER (overlay/sibling/
    /// record retire) crossed the DMA window; the overlay screens prove
    /// no custody was CREATED inside it (creation does not bump the
    /// epoch — the handler's post-read `capture_parked_runs` face); the
    /// binding + incarnation checks prove the device bytes belong to
    /// the block's live incarnation (the validated-ranged serve rule).
    pub fn ipc_direct_revalidate(&self, snap: &IpcDirectSnapshot) -> bool {
        let Some(meta) = self.router.metadata_cache.get(&snap.ino) else {
            return false;
        };
        if meta.file_type != "striped" {
            return false;
        }
        if snap.offset + u64::from(snap.len) > meta.size {
            return false;
        }
        let Some(map) = meta.block_map.as_ref() else {
            return false;
        };
        if map.get(&snap.block).map(String::as_str) != Some(snap.key.as_str()) {
            return false;
        }
        if block_custody_epoch(snap.ino, snap.block) != snap.epoch {
            return false;
        }
        if self
            .parked_overlay_count
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
            && self.active_block_buffers.contains_key(&snap.cache_key)
        {
            return false;
        }
        if self
            .router
            .cache
            .nvme
            .read_staged_zero_copy(&snap.cache_key)
            .is_some()
        {
            return false;
        }
        if self
            .router
            .cache
            .nvme
            .has_staged_extent_record(&snap.ext_key)
        {
            return false;
        }
        if snap.tracked {
            match snap.incarnation {
                Some(before) => {
                    if !self
                        .router
                        .backend_router
                        .fill_incarnation_still(&snap.key, before)
                    {
                        return false;
                    }
                }
                None => return false,
            }
        }
        true
    }

    pub fn ipc_read_probe_locked(&self, ino: u64, offset: u64, size: u32) -> IpcReadProbe {
        // Attr-cache size — a miss would need the handler's async
        // `backend.getattr` fallback, unreachable from a sync service
        // thread: demote (§5.5.1 normative rule).
        let mut file_size = match self.attr_cache.get(&ino) {
            Some((attr, _)) => attr.size,
            None => return IpcReadProbe::Miss,
        };
        // Size coherency override (same rationale as the handler): the
        // router metadata cache is updated synchronously by the write
        // path; the durable attr caches can lag a just-committed write.
        if let Some(m) = self.router.metadata_cache.get(&ino) {
            file_size = m.size;
        }
        if offset >= file_size {
            return IpcReadProbe::Eof;
        }
        let read_len = std::cmp::min(size as u64, file_size - offset) as usize;
        let block_size = self.router.block_size.load(Ordering::Relaxed);
        let start_block = offset / block_size;
        let end_block = (offset + read_len as u64 - 1) / block_size;
        if start_block != end_block {
            // Multi-block reads flush dirty active blocks first (async).
            return IpcReadProbe::Miss;
        }
        let cache_key = crate::keys::active_block(ino, start_block).to_string();
        let Some(buf) = self.active_block_buffers.get(&cache_key) else {
            // No active buffer: try the SYNC tier serves (staging mmap
            // ring, R4 hot tier — L4-8, the §5.5.1 tier→arena engine);
            // anything they decline demotes to the handoff.
            if let Some(m) = self.router.metadata_cache.get(&ino) {
                let file_path = crate::keys::inode_path(ino);
                if let Some(bytes) = self
                    .router
                    .try_read_range_sync(&file_path, &m, offset, read_len)
                {
                    return IpcReadProbe::Hit(bytes);
                }
            }
            return IpcReadProbe::Miss;
        };
        let block_start = start_block * block_size;
        let rel_offset = (offset - block_start) as usize;
        let rel_end = rel_offset + read_len;
        // Extent overlays may owe a base read; uncovered ranges owe
        // zeros-composition or a materializing fetch — handler business,
        // not sync-servable. A fully-covered range is servable from the
        // snapshot regardless of seed deferral (exactly the handler's
        // `contained` branch: covered runs never overlap the owed gaps).
        if buf.value().is_extent_repr() || !buf.value().covered_contains(rel_offset, rel_end) {
            return IpcReadProbe::Miss;
        }
        // The handler's common case: zero-copy CoW-stable snapshot slice
        // (immutable for the reply's lifetime — a later write copies).
        let snapshot = buf.value().snapshot();
        drop(buf);
        IpcReadProbe::Hit(snapshot.slice(rel_offset..rel_end))
    }

    /// Write/refresh this client's mount registration (`client:{id}` xattr on the
    /// root inode) with a fresh heartbeat timestamp. A peer reads the timestamp to
    /// tell a live mount from a crashed one: an entry older than
    /// [`CLIENT_STALE_TTL_SECS`] is treated as stale (kill -9 leaves no chance to
    /// unregister). Best-effort; never fails a caller.
    pub async fn refresh_client_registration(&self) {
        let client_id_str = self.client_id.lock().unwrap().clone();
        if client_id_str.is_empty() {
            return;
        }
        let Some(backend) = self.meta_backend.as_ref() else {
            return;
        };
        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        // Compact JSON: {"ts":<unix_secs>,"pid":<pid>[,"job_endpoint":"ip:port"]}.
        // pid aids same-host diagnosis; the timestamp is the authoritative
        // cross-node liveness signal; job_endpoint is the ADDITIVE §5.1.6
        // discovery field (PR VL2b) remote workers dial the coordinator by.
        let val = match self.job_wire_endpoint.get() {
            Some(ep) => format!(
                "{{\"ts\":{},\"pid\":{},\"job_endpoint\":\"{}\"}}",
                ts,
                std::process::id(),
                ep
            ),
            None => format!("{{\"ts\":{},\"pid\":{}}}", ts, std::process::id()),
        };
        let attr_name = format!("client:{}", client_id_str);
        let _ = backend.setxattr(1, &attr_name, val.as_bytes()).await;
    }

    async fn get_or_acquire_lease(&self, ino: u64) -> Result<u64, SqueezefsError> {
        // Hot path: return cached fencing token without re-validating the DLM map
        // on every write (was a lock/hash hit per op). Stale tokens are rejected by
        // write_file / save_metadata fencing checks; callers invalidate on that path.
        if let Some(lease) = self.active_leases.get(&ino) {
            return Ok(lease.fencing_token());
        }

        let lock_arc = self.lease_locks.get_lock(ino, 0);
        let start_lease_lock = std::time::Instant::now();
        let _guard = lock_arc.lock().await;
        METRICS.lease_lock_wait.record(start_lease_lock.elapsed());

        if let Some(lease) = self.active_leases.get(&ino) {
            return Ok(lease.fencing_token());
        }

        let file_path = crate::keys::inode_path(ino);
        let start_dlm = std::time::Instant::now();
        let lease = self
            .dlm
            .acquire_lock(&file_path, None, Duration::from_secs(5))
            .await?;
        METRICS.dlm_acquire_time.record(start_dlm.elapsed());
        let token = lease.fencing_token();
        self.active_leases.insert(ino, lease);
        Ok(token)
    }

    /// Drop a locally cached lease (e.g. after `FencingTokenExpired` or lock loss).
    pub fn invalidate_local_lease(&self, ino: u64) {
        if let Some((_, lease)) = self.active_leases.remove(&ino) {
            // Best-effort async release if runtime present.
            drop(lease);
        }
    }

    fn block_write_needs_existing_data(
        existing_size: u64,
        block_start: u64,
        block_end: u64,
        write_start: u64,
        write_end: u64,
    ) -> bool {
        let existing_block_end = std::cmp::min(existing_size, block_end);
        existing_block_end > block_start
            && (write_start > block_start || write_end < existing_block_end)
    }

    /// Item B: fetch a deferred RMW seed's OLD block image. The
    /// binding-validated fetch is verbatim the old eager-seed discipline
    /// (the 8e3995e follow-up): `bk` can be displaced, freed, and
    /// reallocated under the same key while we read it, so the fill routes
    /// through `get_block_for_index` (single-flight validated fill:
    /// read_lru → NVMe tier → device read under the incarnation seqlock,
    /// PLUS the block-index→key recheck once the bytes are in hand); a
    /// hole rebind (concurrent truncate/punch pruned the mapping) returns
    /// `None` — the block IS a hole now, its complement seeds zeros.
    ///
    /// OVERLAY NEVER INVISIBLE: this await deliberately takes NO buffer —
    /// callers keep the deferred buffer PARKED (visible to every
    /// concurrent single-block read: kernel readahead, AIO) across the
    /// device read and apply the image afterwards with the synchronous
    /// [`crate::cache::active_block::ActiveBlockBuf::fill_complement_from`]
    /// under this block's `BLOCK_FLUSH_LOCKS` (the same lock the eager
    /// seed read under). Checking the buffer out across this await is the
    /// generic/075-in-QUICK transient: readers fell to the backend and
    /// served pre-merge bytes.
    async fn fetch_seed_image(
        &self,
        file_path: &str,
        b: u32,
    ) -> Result<Option<crate::cache::pool::ReadBlockValue>, SqueezefsError> {
        // RW1: the seed-fetch phase (the item-B RMW read, all drivers —
        // per-driver BYTE attribution happens at the call sites).
        let wp_seed = write_phase_start();
        let mut block_map_id = None;
        let mut block_map = None;
        let seed_ino = crate::routing::parse_inode_from_path(file_path);
        if let Some(entry) = self.router.metadata_cache.get(&seed_ino) {
            if entry.cached_at.elapsed() < Duration::from_secs(1) {
                block_map_id = entry.block_map_id.clone();
                block_map = entry.block_map.clone();
            }
        }
        if block_map_id.is_none() && block_map.is_none() {
            if let Ok(meta) = self.router.fetch_metadata(file_path).await {
                block_map_id = meta.block_map_id.clone();
                block_map = meta.block_map.clone();
            }
        }
        let mut existing: Option<crate::cache::pool::ReadBlockValue> = None;
        if block_map_id.is_some() || block_map.is_some() {
            let mut old_block_key: Option<String> = None;
            if let Some(ref bm) = block_map {
                old_block_key = bm.get(&b).cloned();
            }
            if old_block_key.is_none() {
                if let Ok(meta) = self.router.fetch_metadata(file_path).await {
                    if let Some(ref bm) = meta.block_map {
                        old_block_key = bm.get(&b).cloned();
                    }
                }
            }
            if let Some(bk) = old_block_key {
                existing = self
                    .router
                    // VL8 item 7: NO contention escalation — several
                    // fetch_seed_image callers hold this very block's
                    // BLOCK_FLUSH_LOCKS stripe across the seed fetch
                    // (OVERLAY NEVER INVISIBLE); the stripe is not
                    // reentrant.
                    .get_block_for_index(file_path, b, Some(&bk), false, false)
                    .await?;
            }
        }
        write_phase_record(WritePhase::SeedFetch, wp_seed);
        Ok(existing)
    }

    pub async fn flush_memory_buffers_for_inode(
        &self,
        ino: u64,
        fencing_token: u64,
    ) -> Result<(), SqueezefsError> {
        self.flush_memory_buffers_driven(ino, fencing_token, FlushDriver::FsyncClose)
            .await
    }

    /// Loud disposition of an extent-record parse failure (shared by every
    /// consumer): torn ⇒ discard + count (`extent_records_torn_discarded`,
    /// the detected-and-ignored contract); FUTURE version ⇒ count + LEAVE
    /// (`extent_records_future_refused` — custody of a newer binary, never
    /// wiped; the dir-level format gate normally refuses the whole mount
    /// first). Returns `None` in both cases so callers proceed without the
    /// record. The torn-record ring removal is DETACHED to the blocking
    /// pool (shard-lock invariant rule 2): this helper is reachable from
    /// live read/checkout paths on fuse3 TPC executor threads, and the
    /// shard WRITE lock legitimately waits for §5.5 read guards with
    /// await-side lifetimes — a synchronous remove here is the Hang-1
    /// executor-block shape (the VL8 generic/464 wedge family). Disposal
    /// is idempotent cleanup: callers proceed without the record either
    /// way, and a racing reader that still sees it disposes it again.
    fn dispose_bad_extent_record(
        &self,
        key: &str,
        err: crate::cache::nvme::ExtentRecordError,
    ) -> Option<crate::cache::nvme::ExtentRecord> {
        match err {
            crate::cache::nvme::ExtentRecordError::Torn(what) => {
                METRICS
                    .extent_records_torn_discarded
                    .fetch_add(1, Ordering::Relaxed);
                let msg = format!(
                    "EXTENT RECORD TORN at {key}: {what} — detected-and-ignored loudly \
                     (crash-torn or foreign bytes; the acked window inside a torn \
                     un-fsynced record is POSIX-unspecified)"
                );
                eprintln!("{msg}");
                warn!("{msg}");
                let nvme = self.router.cache.nvme.clone();
                let key_owned = key.to_string();
                // Detached blocking-pool removal (see fn doc): never take
                // the staging shard WRITE lock on an executor thread.
                tokio::task::spawn_blocking(move || {
                    nvme.remove_active_block(&key_owned);
                });
                None
            }
            crate::cache::nvme::ExtentRecordError::FutureVersion(v) => {
                METRICS
                    .extent_records_future_refused
                    .fetch_add(1, Ordering::Relaxed);
                let msg = format!(
                    "EXTENT RECORD at {key} names FUTURE format version {v} (this binary \
                     reads ≤ {}): refused as a unit and LEFT IN PLACE — mount the newer \
                     binary to drain it (forward-only law)",
                    crate::cache::nvme::EXTENT_RECORD_VERSION
                );
                eprintln!("{msg}");
                warn!("{msg}");
                None
            }
        }
    }

    /// Read + validate the staged extent record for `key`, with the loud
    /// per-failure disposition. `None` = no usable record.
    fn read_valid_extent_record(&self, key: &str) -> Option<crate::cache::nvme::ExtentRecord> {
        match self.router.cache.nvme.read_extent_record(key)? {
            Ok(rec) => Some(rec),
            Err(e) => self.dispose_bad_extent_record(key, e),
        }
    }

    /// W2 per-block fold (design-random-small-writes §5.2): seed ONCE via
    /// item B's binding-validated `fetch_seed_image`, apply all k
    /// parked (RAM overlay) + staged (`active_block_ext:` record) extents,
    /// and upload the composed block durably — one 4 MiB-class read + one
    /// 4 MiB-class write per k user writes (`fold_fill`). Returns
    /// `Ok(true)` when a fold ran (the block's extent state is drained),
    /// `Ok(false)` when the block holds no foldable extent state (full-repr
    /// buffers belong to the ordinary flush machinery). Never-lossy:
    /// nothing is removed until the durable upload + merge committed; a
    /// failed fold re-parks by construction (the overlay/record were never
    /// touched). FIND-M11-A discipline: the merge presents the ino's
    /// CURRENT DLM generation, read per attempt — the record's staged-time
    /// stamp is the supersession/remount test, never the merge credential.
    pub async fn fold_extent_block(&self, ino: u64, b: u32) -> Result<bool, SqueezefsError> {
        let cache_key = crate::keys::active_block(ino, b as u64).to_string();
        let ext_key = crate::keys::active_block_ext(ino, b as u64).to_string();
        let file_path = crate::keys::inode_path(ino);

        // Rider dispatch (block 0 of a non-striped layout): the staged /
        // inline whole-image machinery owns the fold — do NOT take the
        // block lock here (the router path takes the same (ino, 0) stripe).
        if b == 0 && self.router.cache.nvme.has_staged_extent_record(&ext_key) {
            let is_striped = self
                .router
                .fetch_metadata(&file_path)
                .await
                .map(|m| m.file_type == "striped")
                .unwrap_or(true);
            if !is_striped {
                let token = self.dlm.get_fencing_token_ino(ino);
                return self.router.fold_rider_record(ino, token).await;
            }
        }

        let (block_guard, lock_waited) =
            block_lock_acquire_timed(ino, b, BlockLockSite::Fold).await;
        METRICS.block_lock_wait.record(lock_waited);

        // Gather under the lock. The RAM overlay and the staged record stay
        // IN PLACE across every await below (overlay never invisible): the
        // composed buffer is a separate allocation, and removal happens only
        // after the durable merge published.
        let ram_extent = match self.active_block_buffers.get(&cache_key) {
            Some(e) if e.value().is_extent_repr() => true,
            Some(_) => {
                // Full-repr buffers belong to the ordinary flush machinery.
                drop(block_guard);
                return Ok(false);
            }
            None => false,
        };
        let record = self
            .router
            .cache
            .nvme
            .has_staged_extent_record(&ext_key)
            .then(|| self.read_valid_extent_record(&ext_key))
            .flatten();
        if !ram_extent && record.is_none() {
            drop(block_guard);
            return Ok(false);
        }

        let block_size = self.router.block_size.load(Ordering::Relaxed) as usize;

        // Seed base, in authority order: a staged FULL sibling image (newer
        // than the durable block — the crash-recovered coexistence shape) >
        // the durable block (item-B binding-validated fetch, only when the
        // complement is owed) > zeros (hole/fresh — NO seed read, the G-RW6
        // hole-write clause).
        let deferred = self
            .active_block_buffers
            .get(&cache_key)
            .map(|e| e.value().seed_deferred())
            .or(record.as_ref().map(|r| r.base_deferred))
            .unwrap_or(false);
        let staged_full = self.router.cache.nvme.read_staged(&cache_key);
        let had_staged_full = staged_full.is_some();
        let mut composed = if let Some(img) = staged_full {
            crate::cache::active_block::ActiveBlockBuf::seeded(&img, block_size)
        } else if deferred {
            let image = self.fetch_seed_image(&file_path, b).await?;
            if let Some(ref img) = image {
                METRICS.fold_seed_reads.fetch_add(1, Ordering::Relaxed);
                METRICS
                    .flush_seed_read_bytes
                    .fetch_add(img.len() as u64, Ordering::Relaxed);
            }
            crate::cache::active_block::ActiveBlockBuf::seeded(
                image.as_deref().unwrap_or(&[]),
                block_size,
            )
        } else {
            let mut zeros = crate::cache::active_block::ActiveBlockBuf::fresh(block_size);
            zeros.zero_complete();
            zeros
        };

        // Apply staged-record extents (older custody), then the RAM slabs
        // (newest) — both synchronously under the held lock.
        let mut k = 0u64;
        if let Some(ref rec) = record {
            let slice = composed.make_mut();
            for (s, d) in &rec.extents {
                let s = *s as usize;
                if s + d.len() <= block_size {
                    slice[s..s + d.len()].copy_from_slice(d);
                    k += 1;
                }
            }
        }
        if ram_extent {
            if let Some(entry) = self.active_block_buffers.get(&cache_key) {
                let runs = entry.value().extent_table();
                drop(entry);
                let slice = composed.make_mut();
                for (s, d) in &runs {
                    let s = *s as usize;
                    if s + d.len() <= block_size {
                        slice[s..s + d.len()].copy_from_slice(d);
                        k += 1;
                    }
                }
            }
        }

        // One durable upload + map merge under the CURRENT generation.
        fold_upload_block(ino, b, composed.snapshot(), &self.router).await?;

        // Authority transferred (the merge published): drain the extent
        // state. Reads between the publish and these removals compose the
        // same bytes over the already-folded base — idempotent.
        self.retire_parked_overlay(&cache_key);
        if record.is_some() || had_staged_full {
            let nvme = self.router.cache.nvme.clone();
            let ek = ext_key.clone();
            let ck = cache_key.clone();
            tokio::task::spawn_blocking(move || {
                nvme.remove_active_block(&ek);
                if had_staged_full {
                    // The staged-full sibling was the fold's seed base and
                    // is strictly ⊆ the folded block: superseded.
                    nvme.remove_active_block(&ck);
                }
            })
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        drop(block_guard);

        METRICS.fold_passes.fetch_add(1, Ordering::Relaxed);
        METRICS.fold_extents_folded.fetch_add(k, Ordering::Relaxed);
        METRICS.fold_fill.record(k as usize);
        Ok(true)
    }

    /// W2 background fold worker (lazily spawned): drains `(ino, block)`
    /// hints posted by threshold-crossing extent merges. Failures are
    /// logged and DROPPED — the extents stay parked custody and the
    /// fsync / pressure / teardown drains own the error surface.
    fn ensure_fold_worker(&self) {
        if self.fold_worker_started.swap(true, Ordering::Relaxed) {
            return;
        }
        let mut rx = match self.fold_rx.lock().unwrap().take() {
            Some(rx) => rx,
            None => return,
        };
        let fs = self.clone();
        tokio::spawn(async move {
            while let Some((ino, b)) = rx.recv().await {
                if let Err(e) = fs.fold_extent_block(ino, b).await {
                    warn!(
                        "background fold of ino {ino} block {b} failed ({e:?}); extents \
                         stay parked (fsync/pressure drains own the retry)"
                    );
                }
            }
        });
    }

    /// W2 mount-time extent-record sweep (design §5.2 crash/recovery/
    /// downgrade): validates every staged `active_block_ext:` record —
    /// torn ⇒ discarded loudly; FUTURE record version ⇒ refused loudly
    /// (left in place); stale fencing token ⇒ discarded (the remount law:
    /// "stale fencing tokens discard staged work"); else recovered
    /// (composable + foldable custody). ANY record found here is
    /// kill-9-class residue (clean shutdowns drain every record to fold),
    /// so the sweep logs one loud stderr line — the §5.2 forward-detection
    /// arm of the below-RW4 downgrade residual. Returns the recovered
    /// count.
    pub async fn recover_extent_records(&self) -> usize {
        let keys = self.router.cache.nvme.extent_record_keys("");
        if keys.is_empty() {
            return 0;
        }
        let mut recovered = 0usize;
        let mut stale = 0usize;
        for key in keys {
            let Some((ino, _b)) = Self::parse_extent_record_key(&key) else {
                // Unparseable key shape: treat as torn (loud discard).
                self.dispose_bad_extent_record(
                    &key,
                    crate::cache::nvme::ExtentRecordError::Torn("unparseable key".into()),
                );
                continue;
            };
            let Some(rec) = self.read_valid_extent_record(&key) else {
                continue;
            };
            let current = self.dlm.get_fencing_token_ino(ino);
            if rec.fencing_token < current {
                // The remount law: a superseded writer era's staged work is
                // discarded, loudly.
                METRICS
                    .extent_records_stale_discarded
                    .fetch_add(1, Ordering::Relaxed);
                warn!(
                    "extent record {key} stamped by superseded fencing generation \
                     {} (current {current}): discarded (the remount law)",
                    rec.fencing_token
                );
                self.router.cache.nvme.remove_active_block(&key);
                stale += 1;
                continue;
            }
            METRICS
                .extent_records_recovered
                .fetch_add(1, Ordering::Relaxed);
            recovered += 1;
        }
        if recovered > 0 || stale > 0 {
            // The forward-detection loud line (the bind_staging_generation
            // loudness class): a clean shutdown drains every record, so
            // this population is kill-9-class residue — and the named
            // detection surface for the below-RW4 downgrade residual.
            let msg = format!(
                "EXTENT RECORDS AT MOUNT: {recovered} recovered, {stale} discarded \
                 (stale fencing) — staging was not cleanly drained (crash residue); \
                 recovered records remain readable and fold on fsync/writeback"
            );
            eprintln!("{msg}");
            warn!("{msg}");
        }
        recovered
    }

    /// `(ino, block)` of an `active_block_ext:inode_{ino}:block_{b}` key.
    fn parse_extent_record_key(key: &str) -> Option<(u64, u32)> {
        let rest = key.strip_prefix("active_block_ext:inode_")?;
        let (ino_str, block_str) = rest.split_once(":block_")?;
        Some((ino_str.parse().ok()?, block_str.parse().ok()?))
    }

    /// [`Self::flush_memory_buffers_for_inode`] with the RW1 §1.2 driver
    /// tag: the parked drain calls this directly so the ledger attributes
    /// its staging puts + writeback requests to the DRAIN driver (the loss
    /// shape's actual durable-upload driver), while every fsync/close
    /// caller rides the public wrapper's `FsyncClose` attribution.
    async fn flush_memory_buffers_driven(
        &self,
        ino: u64,
        fencing_token: u64,
        driver: FlushDriver,
    ) -> Result<(), SqueezefsError> {
        let mut keys_to_flush = Vec::new();
        for r in self.active_block_buffers.iter() {
            let key = r.key();
            let prefix = crate::keys::active_block_ino_prefix(ino);
            if key.starts_with(prefix.as_str()) {
                keys_to_flush.push(key.clone());
            }
        }

        for key in keys_to_flush {
            let Some((_, b)) = Self::parse_active_block_key(&key) else {
                continue;
            };
            // W2 (§5.2): extent overlays FOLD — seed once, apply all
            // extents, one durable upload — instead of the whole-image
            // staging pipeline (fold_extent_block takes its own block
            // lock; escalated-meanwhile buffers fall through to the full
            // path below).
            let is_extent = self
                .active_block_buffers
                .get(&key)
                .map(|e| e.value().is_extent_repr())
                .unwrap_or(false);
            if is_extent && self.fold_extent_block(ino, b).await? {
                continue;
            }
            // Stage/upload exit under the victim's block lock (§5.3 exit 2;
            // normal await — this path holds no other block locks):
            // zero-complete Fresh buffers so recycled pool bytes never
            // reach staging or the device, and serialize against a
            // concurrent write's checkout of the same block.
            let block_guard = block_lock_acquire(ino, b, BlockLockSite::FlushExit).await;
            // OVERLAY NEVER INVISIBLE (the QUICK 075/112 transient): the
            // buffer stays PARKED across every await on this exit —
            // concurrent single-block reads (kernel readahead, AIO) keep
            // serving the ACKed bytes from the overlay. The block lock
            // excludes every mutator, so the entry cannot change under us;
            // the seed image is fetched first (await, buffer visible),
            // applied synchronously, and authority transfers
            // STAGE-THEN-REMOVE so there is no window where neither copy
            // is readable.
            let deferred = match self.active_block_buffers.get(&key) {
                Some(entry) => entry.value().seed_deferred(),
                None => {
                    drop(block_guard);
                    continue;
                }
            };
            if deferred {
                // Item B stage exit: the unwritten complement owes old
                // bytes — never zeros. A failed fetch propagates with the
                // buffer still parked (never-lossy: the ACKed bytes stay
                // in RAM custody, readers keep serving them, and a healed
                // retry flushes the SAME preserved bytes).
                //
                // RW3b covered flush-seed elision: this fetch is reachable
                // only for genuinely-partial coverage — `seed_deferred() ⇒
                // union partial` (every full-coverage transition clears the
                // deferral), so a fully covered buffer can never pay a
                // flush seed read (the measured ~1 GiB/row waste class).
                let file_path = crate::keys::inode_path(ino);
                let image = match self.fetch_seed_image(&file_path, b).await {
                    Ok(img) => img,
                    Err(e) => {
                        drop(block_guard);
                        return Err(e);
                    }
                };
                METRICS.flush_seed_read_bytes.fetch_add(
                    image.as_deref().map(|d| d.len()).unwrap_or(0) as u64,
                    Ordering::Relaxed,
                );
                if let Some(mut entry) = self.active_block_buffers.get_mut(&key) {
                    entry
                        .value_mut()
                        .fill_complement_from(image.as_deref().unwrap_or(&[]));
                }
            }
            let staging_copy = match self.active_block_buffers.get_mut(&key) {
                Some(mut entry) => {
                    entry.value_mut().zero_complete();
                    entry.value().snapshot()
                }
                None => {
                    drop(block_guard);
                    continue;
                }
            };
            let nvme_clone = self.router.cache.nvme.clone();
            let key_clone = key.clone();
            let staging_snapshot = staging_copy.clone();
            let put_len = staging_copy.len() as u64;
            let wp_put = write_phase_start();
            let admitted = tokio::task::spawn_blocking(move || {
                nvme_clone.put_active_block(&key_clone, &staging_snapshot, fencing_token)
            })
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
            write_phase_record(WritePhase::StagingPut, wp_put);

            if admitted {
                driver
                    .staging_put_bytes_counter()
                    .fetch_add(put_len, Ordering::Relaxed);
                // Authority transferred: the staged copy is identical and
                // router reads serve it — the RAM copy can go.
                self.retire_parked_overlay(&key);
                drop(block_guard);
                let req = WritebackRequest {
                    ino,
                    block_idx: b,
                    fencing_token,
                    attempts: 0,
                };
                self.enqueue_writeback(req).await?;
                driver
                    .writeback_enqueued_counter()
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                // Staging refused (never-lossy backpressure): this is the
                // fsync path, so make the block durable right now. The
                // escalation merges via the shared primitive
                // (INODE_META_LOCKS — after BLOCK_FLUSH_LOCKS in the P1-9
                // extended order), so holding the block guard is legal.
                // The RAM copy stays parked (readable) until the durable
                // merge has published.
                upload_active_block_bytes(ino, b, staging_copy, &self.router).await?;
                METRICS
                    .durable_upload_bytes_escalation
                    .fetch_add(put_len, Ordering::Relaxed);
                self.retire_parked_overlay(&key);
                drop(block_guard);
            }
        }

        // W2 (§5.2 mandate): fsync/close/drain FOLD the ino's staged-only
        // extent records too — a cleanly-flushed ino leaves none behind.
        let ext_prefix = crate::keys::active_block_ext_ino_prefix(ino);
        for key in self
            .router
            .cache
            .nvme
            .extent_record_keys(ext_prefix.as_str())
        {
            let Some((r_ino, r_b)) = Self::parse_extent_record_key(&key) else {
                continue;
            };
            if r_ino == ino {
                self.fold_extent_block(r_ino, r_b).await?;
            }
        }
        Ok(())
    }

    /// W1 §5.1 predicate 6: record this striped write's end offset in the
    /// ino's stream word (one relaxed `swap`) and return the PREVIOUS end
    /// (`None` on the ino's first write — never adjacent).
    fn note_last_write_end(&self, ino: u64, offset: u64, len: u64) -> Option<u64> {
        let end = offset + len;
        match self.last_write_end.entry_sync(ino) {
            scc::hash_map::Entry::Occupied(occ) => Some(occ.get().swap(end, Ordering::Relaxed)),
            scc::hash_map::Entry::Vacant(vac) => {
                let _ = vac.insert_entry(AtomicU64::new(end));
                None
            }
        }
    }

    /// W1 sole-owner extent patch (docs/design-random-small-writes.md §5.1
    /// — the in-place, sub-block, LBA-aligned DMA for isolated small
    /// overwrites of exclusively-owned, passthrough, whole-block-mapped
    /// striped blocks). Called with the block's [`BLOCK_FLUSH_LOCKS`]
    /// guard HELD and the request-shape predicates (5: aligned / sized /
    /// non-extending / single-block; 6: not stream-adjacent) already
    /// passed. Runs the remaining predicates (2: no RAM/staged overlay —
    /// lock-free probes; 3: passthrough; 1: undecorated whole-block
    /// mapping, resolved from the authoritative cached map under the held
    /// lock; 4: refcount == 1 re-checked AFTER the unstable-mark, per the
    /// §5.1 clone/patch fence) and, when they hold, performs the patch:
    ///
    /// 1. `mark_incarnation_unstable` → `fence(SeqCst)` → refcount re-check
    ///    ([`crate::block_allocator::BlockAllocator::begin_patch_sole_owner`]);
    ///    any back-off `publish_block`s (content unchanged) and falls back.
    /// 2. One DMA: `write_block(offset + rel, payload)` — the severed
    ///    payload rides a pooled 4 KiB-aligned buffer (`WriteData::Aligned`
    ///    by construction; opt-in write verification covers exactly the
    ///    patched window inside `write_block`).
    /// 3. `publish_block`; purge the key from every read tier
    ///    (`purge_block_key`'s 4-arm law) + drop stale whole-file
    ///    `read_lru`/`write_lru` entries — the `upload_full_block`
    ///    invalidation set and ordering.
    /// 4. ACK. **No allocate, no free, no block-map merge, no journal
    ///    entry, no staging** — the map names the same key; size and
    ///    layout are unchanged; mtime rides the attr-cache + times-echo
    ///    absorber exactly as today.
    ///
    /// Failure: a failed DMA fails exactly THIS write (nothing acked,
    /// nothing parked, nothing lost) — `publish_block` re-stabilizes and
    /// the tiers are purged on the error path too, so no stale serve.
    /// `Ok(false)` = predicate fallback (its `patch_ineligible_*` bucket
    /// counted): the caller continues into today's accumulation path.
    async fn try_sole_owner_patch(
        &self,
        ino: u64,
        b: u32,
        rel_start: u64,
        payload: &[u8],
        cache_key: &str,
        file_path: &str,
    ) -> Result<bool, SqueezefsError> {
        // Predicate 2 — no accumulation overlay owns the block. Lock-free:
        // dashmap probe + the staged occupancy index (review Issue 10 —
        // never the spawn_blocking/shard-write-lock hop on this path).
        if self.active_block_buffers.contains_key(cache_key)
            || self.router.cache.nvme.has_staged_active_block(cache_key)
            || self
                .router
                .cache
                .nvme
                .has_staged_extent_record(&crate::keys::active_block_ext(ino, b as u64))
        {
            // W2: a staged extent record is an overlay too — its extents
            // are NEWER than the base block, so an in-place patch under it
            // would let a later fold re-apply them over the patched bytes.
            METRICS
                .patch_ineligible_overlay
                .fetch_add(1, Ordering::Relaxed);
            return Ok(false);
        }
        // Predicate 3 — passthrough only (a compressed/encrypted image
        // cannot be patched in place).
        if !self.router.get_crypto().is_passthrough() {
            METRICS
                .patch_ineligible_transform
                .fetch_add(1, Ordering::Relaxed);
            return Ok(false);
        }
        // Predicate 1 — the mapping, resolved from the AUTHORITATIVE cached
        // map under the held block lock (block `b`'s entry mutates only
        // under this lock: write-through merges hold it, and writeback
        // merges are excluded by predicate 2 — their staged source exists
        // until after the merge publishes). Cache miss falls back to the
        // backend, which the merge discipline keeps current for `b`.
        let mapping = {
            let cached = self
                .router
                .metadata_cache
                .get(&ino)
                .filter(|m| m.file_type == "striped");
            let meta = match cached {
                Some(m) => Some(m),
                None => self
                    .router
                    .fetch_metadata(file_path)
                    .await
                    .ok()
                    .filter(|m| m.file_type == "striped"),
            };
            meta.and_then(|m| m.block_map.as_ref().and_then(|bm| bm.get(&b).cloned()))
        };
        let Some(mapping) = mapping else {
            // Hole / not striped / indirect-mapped: nothing to patch.
            METRICS
                .patch_ineligible_unmapped
                .fetch_add(1, Ordering::Relaxed);
            return Ok(false);
        };
        if !crate::routing::is_whole_block_mapping(&mapping) {
            // Decorated `bk:off:len` (promoted staged): patch arithmetic
            // must never scribble relative to a decorated window.
            METRICS
                .patch_ineligible_decorated
                .fetch_add(1, Ordering::Relaxed);
            return Ok(false);
        }
        let Ok((be_id, dev_offset)) = self.router.backend_router.parse_block_key(&mapping) else {
            METRICS
                .patch_ineligible_unmapped
                .fetch_add(1, Ordering::Relaxed);
            return Ok(false);
        };
        let Ok((allocator, device)) = self.router.backend_router.get_backend(&be_id) else {
            METRICS
                .patch_ineligible_unmapped
                .fetch_add(1, Ordering::Relaxed);
            return Ok(false);
        };

        // Predicate 4 + the §5.1 fence: mark unstable (racing validated
        // read-tier fills of this key now fail their seqlock re-check
        // instead of publishing mid-patch bytes) → fence(SeqCst) →
        // refcount re-check. A clone whose pin lands before this re-check
        // is observed here (count 2 ⇒ CoW fallback); one that lands after
        // observes instability at its validate-after-pin and retries.
        if !allocator.begin_patch_sole_owner(dev_offset) {
            // Back off: re-stabilize (content never changed) and CoW.
            allocator.publish_block(dev_offset);
            METRICS
                .patch_ineligible_shared
                .fetch_add(1, Ordering::Relaxed);
            return Ok(false);
        }

        // The severed pooled payload: ONE userspace copy into a 4 KiB-
        // aligned pooled backing (§5.4 lease severance — the transport
        // lease slice never crosses the DMA), then ONE aligned DMA.
        let mut buf = crate::cache::pool::BUFFER_POOL.alloc();
        if payload.len() > buf.capacity() {
            buf.resize(payload.len(), 0);
        }
        buf.backing_mut()[..payload.len()].copy_from_slice(payload);
        buf.set_written_len(payload.len());
        let dma = device
            .write_block(dev_offset + rel_start, buf.into_bytes())
            .await;

        // Re-stabilize + purge on BOTH exits (the upload_full_block
        // invalidation set and ordering — after the DMA, before the ACK):
        // RAM LRU / hot-block / NVMe read tier / GDS under the block key,
        // plus the stale whole-file snapshots under the path.
        allocator.publish_block(dev_offset);
        self.router.cache.purge_block_key(&mapping);
        self.router.cache.write_lru.remove(file_path);
        self.router.cache.read_lru.remove(file_path);

        match dma {
            Ok(()) => {
                METRICS.patch_writes.fetch_add(1, Ordering::Relaxed);
                METRICS
                    .patch_write_bytes
                    .fetch_add(payload.len() as u64, Ordering::Relaxed);
                Ok(true)
            }
            Err(e) => {
                // EIO for exactly this write: nothing acked, nothing
                // parked, nothing lost; tiers purged and the word
                // re-stabilized above, so no stale serve.
                METRICS.patch_dma_errors.fetch_add(1, Ordering::Relaxed);
                Err(e)
            }
        }
    }

    /// W2 §5.2 extent-overlay park: the patch-INELIGIBLE small-write route.
    /// Called under the block's held `BLOCK_FLUSH_LOCKS` guard after the W1
    /// patch declined (or was shape-ineligible). Returns `Ok(true)` when
    /// the write was fully absorbed by the extent machinery (merged into /
    /// created an overlay — the caller ACKs), `Ok(false)` to fall through
    /// to the ordinary full-buffer checkout.
    ///
    /// Representation choice (§5.2): a small (< 25 % of the block) write
    /// that is not stream-adjacent parks compactly; escalation to a full
    /// buffer at coverage ≥ 25 % or on a large merge. A staged extent
    /// record found here is REHYDRATED (absorbed older-under + removed —
    /// custody moves staging → RAM exactly as the full checkout moves the
    /// staged sibling). Fold-threshold crossings post background fold
    /// hints; the parked BYTE budget is enforced by the shared spill loop.
    #[allow(clippy::too_many_arguments)]
    async fn try_extent_park(
        &self,
        ino: u64,
        b: u32,
        rel_start: usize,
        data: &[u8],
        needs_existing_data: bool,
        cache_key: &str,
        adjacent: bool,
        fencing_token: u64,
    ) -> Result<bool, SqueezefsError> {
        let bs = self.router.block_size.load(Ordering::Relaxed);
        let small = !data.is_empty() && (data.len() as u64) * 4 < bs;
        let ext_key = crate::keys::active_block_ext(ino, b as u64).to_string();

        // Existing parked overlay: merge in place (the entry never leaves
        // the map — readers stay served at every instant).
        if let Some(mut entry) = self.active_block_buffers.get_mut(cache_key) {
            if !entry.value().is_extent_repr() {
                return Ok(false);
            }
            if !small || adjacent {
                // Large/stream merge into an extent overlay: escalate in
                // place (RAM-only) and let the ordinary checkout own it.
                entry.value_mut().escalate_to_full();
                return Ok(false);
            }
            let completed = entry.value_mut().merge_extent(rel_start, data);
            debug_assert!(
                !completed,
                "an extent overlay cannot complete coverage: escalation at \
                 25% strictly precedes any full-coverage transition"
            );
            METRICS.extent_parks.fetch_add(1, Ordering::Relaxed);
            let count = entry.value().extent_count() as u64;
            let bytes = entry.value().extent_payload_bytes();
            if completed || bytes * 4 >= bs {
                entry.value_mut().escalate_to_full();
                drop(entry);
            } else {
                drop(entry);
                let fm = fold_max_extents();
                let fb = fold_max_bytes();
                if (fm != 0 && count >= fm) || (fb != 0 && bytes >= fb) {
                    self.ensure_fold_worker();
                    let _ = self.fold_tx.try_send((ino, b));
                }
            }
            self.spill_parked_toward_cap(fencing_token).await;
            return Ok(true);
        }

        // No RAM entry. A staged FULL sibling owns the block (accumulation
        // in progress): the seeded checkout path owns it.
        if !small || adjacent || self.router.cache.nvme.has_staged_active_block(cache_key) {
            return Ok(false);
        }

        // Create the overlay; rehydrate a staged record first (its extents
        // are OLDER custody — absorbed under, then the record retires).
        let mut overlay =
            crate::cache::active_block::ActiveBlockBuf::extent(bs as usize, needs_existing_data);
        let mut absorbed_record = false;
        if self.router.cache.nvme.has_staged_extent_record(&ext_key) {
            if let Some(rec) = self.read_valid_extent_record(&ext_key) {
                if rec.base_deferred && !overlay.seed_deferred() {
                    // Safe direction: a deferred complement costs at most a
                    // (possibly hole-`None`) seed fetch; a wrongly-fresh one
                    // would codify zeros over old bytes.
                    overlay = crate::cache::active_block::ActiveBlockBuf::extent(bs as usize, true);
                }
                for (s, d) in &rec.extents {
                    if (*s as usize) + d.len() <= bs as usize {
                        overlay.absorb_older_extent(*s as usize, d);
                    }
                }
                absorbed_record = true;
                METRICS
                    .extent_record_absorbs
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        if absorbed_record && overlay.extent_payload_bytes() * 4 >= bs {
            // The rehydrated record alone crosses the escalation edge: park
            // the ESCALATED absorbed state and let the ordinary checkout
            // merge this write (write-through machinery included).
            overlay.escalate_to_full();
            self.park_overlay_entry(cache_key.to_string(), overlay);
            let nvme = self.router.cache.nvme.clone();
            let ek = ext_key.clone();
            tokio::task::spawn_blocking(move || nvme.remove_active_block(&ek))
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            return Ok(false);
        }
        let completed = overlay.merge_extent(rel_start, data);
        debug_assert!(!completed, "small first write cannot complete a block");
        METRICS.extent_parks.fetch_add(1, Ordering::Relaxed);
        let count = overlay.extent_count() as u64;
        let bytes = overlay.extent_payload_bytes();
        // Park FIRST (overlay visible), then retire the absorbed record —
        // at every instant at least one copy serves reads.
        self.park_overlay_entry(cache_key.to_string(), overlay);
        if absorbed_record {
            let nvme = self.router.cache.nvme.clone();
            let ek = ext_key.clone();
            tokio::task::spawn_blocking(move || nvme.remove_active_block(&ek))
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        let fm = fold_max_extents();
        let fb = fold_max_bytes();
        if (fm != 0 && count >= fm) || (fb != 0 && bytes >= fb) {
            self.ensure_fold_worker();
            let _ = self.fold_tx.try_send((ino, b));
        }
        self.spill_parked_toward_cap(fencing_token).await;
        Ok(true)
    }

    /// Staged/striped active-block write path. Safe to call without holding the
    /// per-inode write lock: mutates each block under [`BLOCK_FLUSH_LOCKS`].
    ///
    /// P1-10: the fencing check uses a short-lived meta connection that is dropped
    /// before any active-block / backend I/O (block tasks open their own connections).
    pub async fn write_file_staged(
        &self,
        ino: u64,
        offset: u64,
        data: bytes::Bytes,
        existing_size: u64,
        fencing_token: u64,
    ) -> Result<(), SqueezefsError> {
        {
            let current_fencing = self.dlm.get_fencing_token_ino(ino);
            if fencing_token < current_fencing {
                return Err(SqueezefsError::FencingTokenExpired {
                    token: fencing_token,
                    expected: current_fencing,
                });
            }
        }

        let block_size = self.router.block_size.load(Ordering::Relaxed);
        let start_block = offset / block_size;
        let end_block = (offset + data.len() as u64 - 1) / block_size;

        // W1 sole-owner extent patch — the request-shape half of the §5.1
        // predicate (design-random-small-writes), evaluated once per
        // striped write. Every striped write resolves to exactly one of
        // {patched, one `patch_ineligible_*` bucket} — the FIRST failing
        // predicate in this order — so the decision ledger reconciles
        // against invocations. The stream word is swapped UNCONDITIONALLY
        // (predicate 6 needs the true previous end even while the knob is
        // 0 or the shape is ineligible).
        let prev_write_end = self.note_last_write_end(ino, offset, data.len() as u64);
        // W2 (§5.2): stream-adjacency also gates the extent-overlay park —
        // sequential streams keep the whole-block write-through economy.
        let stream_adjacent = prev_write_end == Some(offset);
        let patch_cap = patch_max_bytes();
        let try_patch = if patch_cap == 0 || data.is_empty() {
            // Knob 0 = the §6 A/B lever: the patch path is OFF and the
            // decision ledger stays silent.
            false
        } else if prev_write_end == Some(offset) {
            // Predicate 6 — stream-adjacent: sequential streams keep the
            // whole-block write-through economy.
            METRICS
                .patch_ineligible_adjacent
                .fetch_add(1, Ordering::Relaxed);
            false
        } else if start_block != end_block
            || data.len() as u64 > patch_cap
            || offset + data.len() as u64 > existing_size
        {
            // Predicate 5 size/window class (one bucket by design, §5.4):
            // spans blocks, exceeds SQUEEZEFS_PATCH_MAX_BYTES, or EXTENDS
            // the file (a grown i_size owes a meta commit).
            METRICS
                .patch_ineligible_oversize
                .fetch_add(1, Ordering::Relaxed);
            false
        } else if offset % 4096 != 0 || data.len() % 4096 != 0 {
            // Predicate 5 alignment (v1 is LBA-aligned-only — the unaligned
            // edge path is phase-2 behind its own torn-edge contract).
            METRICS
                .patch_ineligible_unaligned
                .fetch_add(1, Ordering::Relaxed);
            false
        } else {
            true
        };

        let mut futures = Vec::new();
        let mut data_cursor = 0usize;
        for b in start_block..=end_block {
            let b_start_offset = b * block_size;
            let b_end_offset = b_start_offset + block_size;

            let write_start = std::cmp::max(offset, b_start_offset);
            let write_end = std::cmp::min(offset + data.len() as u64, b_end_offset);
            let slice_len = (write_end - write_start) as usize;

            let file_data_slice = &data[data_cursor..data_cursor + slice_len];
            data_cursor += slice_len;

            let cache_key = crate::keys::active_block(ino, b as u64).to_string();
            let needs_existing_data = Self::block_write_needs_existing_data(
                existing_size,
                b_start_offset,
                b_end_offset,
                write_start,
                write_end,
            );

            let file_path = crate::keys::inode_path(ino);

            futures.push(async move {
                // 0. Acquire Block-level Lock to prevent concurrent modification to the same block
                // (RW1: per-site wait attribution — the write_checkout site;
                // the returned wait keeps feeding the always-on global
                // histogram so the FIND-L1-A baseline series stays
                // comparable.)
                let (block_guard, lock_waited) =
                    block_lock_acquire_timed(ino, b as u32, BlockLockSite::WriteCheckout).await;
                METRICS.block_lock_wait.record(lock_waited);

                // W1 sole-owner extent patch (§5.1): the request-shape half
                // passed above (single block ⇒ this future is the whole
                // request); the state half runs here under the held block
                // lock. `true` = patched — one aligned in-place DMA, tiers
                // purged, ACK: no checkout, no merge, no park, no meta.
                // `false` = its `patch_ineligible_*` bucket counted — fall
                // through to today's accumulation path. `Err` = DMA/verify
                // failure surfaced to exactly this write.
                if try_patch {
                    let rel = write_start - b_start_offset;
                    match self
                        .try_sole_owner_patch(
                            ino,
                            b as u32,
                            rel,
                            file_data_slice,
                            &cache_key,
                            &file_path,
                        )
                        .await
                    {
                        Ok(true) => {
                            std::mem::drop(block_guard);
                            return Ok::<(), SqueezefsError>(());
                        }
                        Ok(false) => {}
                        Err(e) => {
                            std::mem::drop(block_guard);
                            return Err(e);
                        }
                    }
                }

                // W2 §5.2 extent-overlay park — the patch-INELIGIBLE
                // small-write route (compressed/shared/decorated/hole/
                // unaligned shapes): park ~payload bytes instead of a
                // block-size deferred buffer. `true` = absorbed (ACK).
                {
                    let rel = (write_start - b_start_offset) as usize;
                    if self
                        .try_extent_park(
                            ino,
                            b as u32,
                            rel,
                            file_data_slice,
                            needs_existing_data,
                            &cache_key,
                            stream_adjacent,
                            fencing_token,
                        )
                        .await?
                    {
                        std::mem::drop(block_guard);
                        return Ok::<(), SqueezefsError>(());
                    }
                }

                // 1. Ensure this block's overlay entry is PRESENT and
                // seeded — never checked out. OVERLAY NEVER INVISIBLE,
                // applied to the write path itself (fstests generic/209,
                // VL10 release gate): the old remove→mutate→reinsert
                // possession made every previously-ACKED byte living in
                // this buffer invisible to concurrent reads for the whole
                // absorb/sibling-remove/upload await window — readers fell
                // through to the pre-merge ring/backend and served
                // one-write-behind bytes (the aio-dio-invalidate-readahead
                // "old byte" signature; pinned in
                // tests/write_visibility_tests.rs::completed_overwrites_never_serve_the_previous_pass).
                // The entry now stays in the map at every instant; all
                // mutations happen under short-lived map guards (never
                // across an await), and this block's BLOCK_FLUSH_LOCKS
                // guard (held) excludes every other mutator.
                let wp_checkout = write_phase_start();
                if self.active_block_buffers.contains_key(&cache_key) {
                    // RW1 ledger: an overlay already owned this block —
                    // the §1.2 block-revisit discount, quantified.
                    METRICS.write_block_revisits.fetch_add(1, Ordering::Relaxed);
                } else {
                    // Build the seed OUTSIDE the map (no acked bytes live
                    // here yet — awaits are legal), then publish it.
                    // The seed class keys on whether the BLOCK holds any
                    // existing bytes — NOT on `needs_existing_data` (this
                    // write's complement shape). The entry is published
                    // BEFORE this write's coverage is recorded (never
                    // invisible), so a fully-covering overwrite classified
                    // "no complement owed" would sit in the map as a Fresh
                    // (zeros-complement) buffer with an EMPTY union for the
                    // absorb/sibling awaits — and a concurrent read's
                    // Fresh-gap branch would compose ZEROS over real old
                    // bytes (the ranged_read rebind_under_movement foreign-
                    // zeros regression). Deferred costs nothing here: the
                    // covering write completes the union at record_write,
                    // which clears the deferral (`seed_deferred ⇒ union
                    // partial` — structural), so no seed read is ever paid.
                    let block_has_existing_bytes =
                        std::cmp::min(existing_size, b_end_offset) > b_start_offset;
                    let seed = if let Some(d) = self.router.cache.nvme.read_staged(&cache_key) {
                        METRICS.write_block_revisits.fetch_add(1, Ordering::Relaxed);
                        crate::cache::active_block::ActiveBlockBuf::seeded(&d, block_size as usize)
                    } else if !block_has_existing_bytes {
                        // Fresh entry: no existing data for this block, so the
                        // seed-time zero-fill is elided (§5.3) — the written
                        // coverage runs keep recycled pool bytes private, and
                        // the gaps are zeroed lazily at any stage/upload exit
                        // (a coverage-complete block has none).
                        crate::cache::active_block::ActiveBlockBuf::fresh(block_size as usize)
                    } else {
                        // Item B (the overwrite lazy-RMW seed): DEFER the
                        // old-block read. A covering overwrite — in ANY
                        // segment order (RW3b) — completes the block before
                        // its write-through, so the seed read is skipped
                        // entirely when coverage completes. Every escape
                        // path (stage/upload exits, sparse reads)
                        // materializes the seed first via
                        // `fetch_seed_image`. A concurrent read of a
                        // deferred gap takes this block's lock (Item B
                        // materialize) and therefore parks behind THIS
                        // write — fresh serve after, never a stale one.
                        METRICS
                            .overwrite_seed_deferred
                            .fetch_add(1, Ordering::Relaxed);
                        crate::cache::active_block::ActiveBlockBuf::deferred(block_size as usize)
                    };
                    self.park_overlay_entry(cache_key.clone(), seed);
                }
                write_phase_record(WritePhase::Checkout, wp_checkout);

                // W2 one-authority extension: a full-repr buffer supersedes
                // the block's staged extent record from birth — absorb its
                // (older) extents into the coverage gaps and retire it.
                // Zero-cost when no record exists (one latch-free probe).
                // Split form of the old `absorb_extent_record_into`: sync
                // record read → guarded apply (no await under the map
                // guard) → awaited record retirement.
                {
                    let ext_key = crate::keys::active_block_ext(ino, b as u64).to_string();
                    if self.router.cache.nvme.has_staged_extent_record(&ext_key) {
                        if let Some(rec) = self.read_valid_extent_record(&ext_key) {
                            let bs_usize = block_size as usize;
                            if let Some(mut e) = self.active_block_buffers.get_mut(&cache_key) {
                                for (s, d) in &rec.extents {
                                    if (*s as usize) + d.len() <= bs_usize {
                                        e.value_mut().absorb_older_extent(*s as usize, d);
                                    }
                                }
                            }
                            METRICS
                                .extent_record_absorbs
                                .fetch_add(1, Ordering::Relaxed);
                            let nvme = self.router.cache.nvme.clone();
                            tokio::task::spawn_blocking(move || nvme.remove_active_block(&ext_key))
                                .await
                                .map_err(|e| std::io::Error::other(e.to_string()))?;
                        }
                    }
                }

                // ONE-AUTHORITY INVARIANT (generic/075.2), amended by the
                // never-invisible law: per block, the NEWEST content lives
                // in the RAM parked buffer, which now coexists with a
                // (strictly older) staged `active_block:` sibling only for
                // the guarded window below — readers prefer the RAM
                // overlay, and this block's held lock excludes
                // flush_one_active_block from uploading the stale staged
                // copy meanwhile. The sibling is superseded and removed
                // here; any queued WritebackRequest for it becomes a clean
                // no-op; the durability promise transfers to this write's
                // own park/write-through path.
                //
                // spawn_blocking (same rule as every put_active_block call):
                // the staging-shard WRITE lock is a parking_lot lock, and
                // §5.5 DMA sources hold the shard READ lock ACROSS awaits —
                // parking an async worker on the writer side starves the
                // executor until no worker is left to poll the guard-holding
                // tasks (observed live: total daemon wedge under the fsx-075
                // harness, every worker parked in NvmeShard lock_shared/
                // lock_exclusive).
                {
                    // RW1: the H1 probe counter (fires per checkout, staged
                    // sibling or not — the pure-overhead face) + the §1.2
                    // bucket-4 churn ledger (a revisit discarding the staged
                    // image an earlier spill already paid for).
                    let wp_sibling = write_phase_start();
                    METRICS
                        .staging_sibling_probes
                        .fetch_add(1, Ordering::Relaxed);
                    let nvme = self.router.cache.nvme.clone();
                    let key = cache_key.clone();
                    let removed =
                        tokio::task::spawn_blocking(move || nvme.remove_active_block(&key))
                            .await
                            .map_err(|e| std::io::Error::other(e.to_string()))?;
                    if let Some(prev) = removed {
                        METRICS
                            .restage_churn_removes
                            .fetch_add(1, Ordering::Relaxed);
                        METRICS
                            .restage_churn_bytes
                            .fetch_add(prev.len() as u64, Ordering::Relaxed);
                    }
                    write_phase_record(WritePhase::SiblingRemove, wp_sibling);
                }

                // 2. Merge the request slice under a SHORT map guard (no
                // await). `make_mut` mutates only provably-unique memory: a
                // live reader snapshot forces a copy-on-write instead of
                // mutating aliased bytes (P0 fix, zero-copy write-path
                // design §5.2). Coverage bookkeeping first (§5.3 + RW3b):
                // `record_write` merges this range into the buffer's
                // written-coverage union — overlap-safe, order-blind — and
                // returns `true` exactly when the union reaches the whole
                // block. Gap writes into a deferred-seed buffer do not
                // materialize inline (item B holds; FIND-L1-A).
                let rel_start = (write_start - b_start_offset) as usize;
                let wp_merge = write_phase_start();
                let coverage_completed = {
                    let mut entry = self
                        .active_block_buffers
                        .get_mut(&cache_key)
                        .expect("parked entry cannot vanish under the held block lock");
                    let completed = entry
                        .value_mut()
                        .record_write(rel_start, rel_start + slice_len);
                    entry.value_mut().make_mut()[rel_start..rel_start + slice_len]
                        .copy_from_slice(file_data_slice);
                    completed
                };
                write_phase_record(WritePhase::MergeCopy, wp_merge);

                // 3. Write-through when the block is content-complete —
                // RW3b normative trigger: the ACCUMULATED written coverage
                // spans the whole block (FIND-L1-A). Else keep parked:
                // partial coverage stays in the map (it never left), and
                // the item-B deferral keeps holding. The upload runs from a
                // CoW snapshot while the entry stays readable; the entry is
                // removed only after the durable publish succeeds.
                let is_block_complete = coverage_completed;
                if is_block_complete {
                    let block_snapshot = {
                        let entry = self
                            .active_block_buffers
                            .get(&cache_key)
                            .expect("parked entry cannot vanish under the held block lock");
                        debug_assert!(
                            entry.value().is_content_valid() && !entry.value().seed_deferred(),
                            "a coverage-complete buffer must be content-valid with \
                             its deferral cleared (record_write's completion \
                             transition owns both)"
                        );
                        entry.value().snapshot()
                    };
                    match self
                        .upload_full_block(ino, b as u32, block_snapshot.clone(), fencing_token)
                        .await
                    {
                        Ok(()) => {
                            // Durable + published: the RAM entry retires
                            // (reads flow to the block map / read tiers).
                            self.retire_parked_overlay(&cache_key);
                            METRICS.write_through_blocks.fetch_add(1, Ordering::Relaxed);
                            METRICS
                                .write_through_bytes
                                .fetch_add(block_size, Ordering::Relaxed);
                            std::mem::drop(block_guard);
                        }
                        Err(e @ SqueezefsError::FencingTokenExpired { .. }) => {
                            // A fenced-out writer must not publish anywhere —
                            // not even to staging. Drop custody (the old
                            // checked-out buffer was dropped here too) and
                            // propagate; the caller invalidates the lease.
                            self.retire_parked_overlay(&cache_key);
                            return Err(e);
                        }
                        Err(e) => {
                            // Never-lossy fallback (uring backpressure /
                            // allocator / device failure): degrade into
                            // today's staging + writeback path.
                            METRICS
                                .write_through_fallbacks
                                .fetch_add(1, Ordering::Relaxed);
                            warn!(
                                "write-through failed for ino {} block {} ({:?}); \
                                 falling back to staging",
                                ino, b, e
                            );
                            let nvme_clone = self.router.cache.nvme.clone();
                            let cache_key_clone = cache_key.clone();
                            let fencing_token_val = fencing_token;
                            let put_len = block_snapshot.len() as u64;
                            let staging_snapshot = block_snapshot;
                            let wp_put = write_phase_start();
                            let admitted = tokio::task::spawn_blocking(move || {
                                nvme_clone.put_active_block(
                                    &cache_key_clone,
                                    &staging_snapshot,
                                    fencing_token_val,
                                )
                            })
                            .await
                            .map_err(|e| std::io::Error::other(e.to_string()))?;
                            write_phase_record(WritePhase::StagingPut, wp_put);

                            if admitted {
                                // Staged custody landed: the RAM entry
                                // retires (the ring copy is identical).
                                self.retire_parked_overlay(&cache_key);
                            }
                            std::mem::drop(block_guard);

                            if admitted {
                                METRICS
                                    .staging_put_bytes_wt_fallback
                                    .fetch_add(put_len, Ordering::Relaxed);
                                let req = WritebackRequest {
                                    ino,
                                    block_idx: b as u32,
                                    fencing_token,
                                    attempts: 0,
                                };
                                self.enqueue_writeback(req).await?;
                                METRICS
                                    .writeback_enqueued_wt_fallback
                                    .fetch_add(1, Ordering::Relaxed);
                            } else {
                                // Staging refused too (never-lossy
                                // backpressure): the block stays PARKED —
                                // it never left the map — and rides the R5
                                // admission pass like a partial block;
                                // fsync's buffer flush re-attempts staging
                                // or uploads it durably.
                                let wp_park = write_phase_start();
                                self.admit_parked_active_block(&cache_key, fencing_token)
                                    .await;
                                write_phase_record(WritePhase::ParkSpill, wp_park);
                            }
                        }
                    }
                } else {
                    // Partial coverage: already parked (never left the
                    // map); run the R5 byte-budget admission pass.
                    let wp_park = write_phase_start();
                    self.admit_parked_active_block(&cache_key, fencing_token)
                        .await;
                    write_phase_record(WritePhase::ParkSpill, wp_park);
                    std::mem::drop(block_guard);
                }

                Ok::<(), SqueezefsError>(())
            });
        }

        futures::future::try_join_all(futures).await?;

        Ok(())
    }

    /// Punch a hole: make `[offset, offset+length)` read back as zeros WITHOUT
    /// changing the file's logical size (POSIX `FALLOC_FL_PUNCH_HOLE`). A hole
    /// reads as zeros (an unwritten / punched / extended region), so this is the
    /// zero-or-unmap primitive shared by the fallocate handler.
    ///
    /// - **Striped:** whole covered blocks are unmapped + freed (real holes,
    ///   space reclaimed, incarnation-retired so a reused offset never reads
    ///   back through the stale map); partial edges are RMW-zeroed through the
    ///   write path (preserving the un-punched bytes of the edge block).
    /// - **inline / staged:** the covered sub-range is RMW-zeroed in place via
    ///   the router write path (which refreshes the whole-file cache too).
    ///
    /// Caller MUST hold the per-inode write guard (serializes against writes;
    /// makes any in-flight writeback for a dropped block a no-op — the worker
    /// skips a `NotFound` active block and takes the same guard before its map
    /// merge). Size is never grown (`end` is clamped to EOF).
    async fn punch_hole_range(
        &self,
        ino: u64,
        offset: u64,
        length: u64,
        fencing_token: u64,
    ) -> Result<(), SqueezefsError> {
        let file_path = crate::keys::inode_path(ino);
        let meta = self.router.fetch_metadata(&file_path).await?;
        if length == 0 {
            return Ok(());
        }
        // Honor the kernel's punch range [offset, offset+length). Do NOT clamp
        // `end` to our own `meta.size`: under the FUSE writeback cache a
        // deferred write flush leaves `meta.size` (what fetch_metadata reports)
        // lagging the kernel `i_size`, so clamping would skip zeroing a tail the
        // kernel considers in-bounds — the punched range then reads its stale
        // prior bytes (the generic/616 residual; same laggable-size trap the
        // copy_file_range fix hit). The kernel only punches within `i_size`, so
        // zeroing the full range is safe and reconciles the lagging size.
        let end = offset + length;
        // Size floor for map/layout saves: never regress below the freshest
        // known size, and let the punched range extend it if our size lagged.
        let size_floor = meta.size.max(end);

        if meta.file_type == "striped" {
            let bs = self.router.block_size.load(Ordering::Relaxed);
            let start_b = offset / bs;
            let end_b = (end - 1) / bs;
            let mut whole_idxs: Vec<u32> = Vec::new();
            for b in start_b..=end_b {
                let b_start = b * bs;
                let b_end = b_start + bs;
                let cov_start = std::cmp::max(offset, b_start);
                let cov_end = std::cmp::min(end, b_end);
                if cov_start == b_start && cov_end == b_end {
                    // Whole block → hole. Drop the RAM + staged active-block
                    // copy under the block lock (serialize against a racing
                    // flush) before it is unmapped + freed below.
                    let block_guard = block_lock_acquire(ino, b as u32, BlockLockSite::Punch).await;
                    let key = crate::keys::active_block(ino, b).to_string();
                    let ext_key = crate::keys::active_block_ext(ino, b).to_string();
                    self.retire_parked_overlay(&key);
                    // Blocking-pool hop: shard WRITE lock (invariant rule 2).
                    // W2: the staged extent record dies with the block too.
                    self.router
                        .cache
                        .nvme
                        .remove_active_blocks_async(vec![key, ext_key])
                        .await?;
                    drop(block_guard);
                    whole_idxs.push(b as u32);
                } else {
                    // Partial edge → RMW-zero exactly the covered sub-range via
                    // the striped write path: it consumes the active buffer /
                    // device block as the RMW base, overlays zeros in the
                    // covered range (recorded as covered), and preserves the
                    // un-punched bytes.
                    let zeros = bytes::Bytes::from(vec![0u8; (cov_end - cov_start) as usize]);
                    self.write_file_staged(ino, cov_start, zeros, size_floor, fencing_token)
                        .await?;
                }
            }
            // Unmap + free the whole blocks (holes); the size floor keeps the
            // logical size at least the freshest known / punched extent.
            self.router
                .punch_striped_blocks(ino, &whole_idxs, size_floor, fencing_token)
                .await?;
        } else {
            // inline / staged: RMW-zero the covered range in place through the
            // router write path. It reads the authoritative base (staging for
            // staged, data_key for inline) and rewrites the range as zeros.
            let zeros = bytes::Bytes::from(vec![0u8; (end - offset) as usize]);
            self.router
                .write_file(&file_path, offset, zeros, fencing_token)
                .await?;
        }
        Ok(())
    }

    /// Drop every active-block overlay — parked RAM `ActiveBlockBuf` and
    /// staged `active_block:` ring entry — whose block lies entirely at/after
    /// `new_size`. Truncate prunes those blocks from the map; an overlay that
    /// survives the prune is stale beyond-EOF content that would serve
    /// single-block reads, seed the next partial write's RMW, or re-merge
    /// into the map on the next flush (the generic/075.2 stale-data
    /// resurrection). Caller MUST hold the per-inode write guard; each
    /// removal runs under that block's `BLOCK_FLUSH_LOCKS` (lock order 1→3),
    /// making any in-flight writeback for the dropped block a NotFound no-op.
    async fn drop_active_block_overlays_beyond(&self, ino: u64, new_size: u64) {
        let bs = self.router.block_size.load(Ordering::Relaxed);
        if bs == 0 {
            return;
        }
        let first_dead_block = new_size.div_ceil(bs);
        let prefix = crate::keys::active_block_ino_prefix(ino);
        let mut dead: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        for r in self.active_block_buffers.iter() {
            if let Some((i, b)) = Self::parse_active_block_key(r.key()) {
                if i == ino && (b as u64) >= first_dead_block {
                    dead.insert(b);
                }
            }
        }
        // Staged overlays can exist without a RAM buffer (RAM-cap spill,
        // write-through staging fallback awaiting writeback): sweep the
        // staging key space too. Truncate is a cold path; the key list is
        // bounded by the staging budget. W2: staged extent records are
        // overlays of the same class — beyond-EOF records must die with
        // the map prune or their extents resurface through the next fold.
        for key in self.router.cache.nvme.list_staged_files() {
            if !key.starts_with(prefix.as_str()) {
                continue;
            }
            if let Some((i, b)) = Self::parse_active_block_key(&key) {
                if i == ino && (b as u64) >= first_dead_block {
                    dead.insert(b);
                }
            }
        }
        let ext_prefix = crate::keys::active_block_ext_ino_prefix(ino);
        for key in self
            .router
            .cache
            .nvme
            .extent_record_keys(ext_prefix.as_str())
        {
            if let Some((i, b)) = Self::parse_extent_record_key(&key) {
                if i == ino && (b as u64) >= first_dead_block {
                    dead.insert(b);
                }
            }
        }
        for b in dead {
            let _block_guard = block_lock_acquire(ino, b, BlockLockSite::OverlayPrune).await;
            let key = crate::keys::active_block(ino, b as u64).to_string();
            let ext_key = crate::keys::active_block_ext(ino, b as u64).to_string();
            self.retire_parked_overlay(&key);
            // spawn_blocking: the staging-shard WRITE lock must never park
            // an async worker (§5.5 read guards are held across DMA awaits;
            // see flush_one_active_block).
            let nvme = self.router.cache.nvme.clone();
            if tokio::task::spawn_blocking(move || {
                nvme.remove_active_block(&key);
                nvme.remove_active_block(&ext_key);
            })
            .await
            .is_err()
            {
                continue;
            }
        }
    }

    /// The freshest known logical size of `ino`: the maximum of the durable
    /// inode size and the RAM-side caches the write path updates synchronously
    /// (`metadata_cache`, `attr_cache`). The durable size LAGS the truth —
    /// staged/inline writes defer their layout+size persist to the fsync/flush
    /// cadence — so any size-classifying decision (grow-vs-shrink, extend
    /// no-op) taken against the durable size alone misclassifies whenever
    /// unflushed writes grew the file (the generic/091 lost-write family).
    /// Taking the max never regresses: each source only ever runs behind the
    /// kernel-observed size, never ahead of it.
    async fn freshest_size(&self, ino: u64) -> Result<u64, SqueezefsError> {
        let backend = self.meta_backend.as_ref().ok_or_else(|| {
            SqueezefsError::InvalidOperation("Metadata backend not initialized".to_string())
        })?;
        let mut size = backend.getattr(ino).await?.size;
        if let Some(m) = self.router.metadata_cache.get(&ino) {
            size = size.max(m.size);
        }
        if let Some((attr, _)) = self.attr_cache.get(&ino) {
            size = size.max(attr.size);
        }
        Ok(size)
    }

    /// Grow a file's logical size to `target_size` (a no-op if already at least
    /// that large). The newly exposed region [old_size, target_size) is a hole
    /// (reads zeros). Shared by the fallocate preallocate/extend and ZERO_RANGE
    /// paths.
    ///
    /// MUST never shrink: the no-op gate compares against the FRESHEST size
    /// (durable + RAM caches), not the laggable durable inode size, and the
    /// layout-size stores below only ever move the size up. Gating on the
    /// durable size alone let an in-bounds (interior) fallocate issued while
    /// staged writes were still unflushed clobber the logical size DOWN to
    /// `offset + length`, hiding all data beyond it — the generic/091
    /// "written data reads back zeros" corruption.
    async fn extend_file_size(
        &self,
        ino: u64,
        target_size: u64,
        fencing_token: u64,
    ) -> Result<(), SqueezefsError> {
        let backend = self.meta_backend.as_ref().ok_or_else(|| {
            SqueezefsError::InvalidOperation("Metadata backend not initialized".to_string())
        })?;

        let old_size = self.freshest_size(ino).await?;
        if target_size <= old_size {
            return Ok(());
        }

        // Update in backend
        backend
            .setattr(ino, None, None, None, Some(target_size), None, None, None)
            .await?;

        // Update layout size. Striped files go through the §5.3 degenerate
        // size-only merge: the old whole-meta save of a stale snapshot under NO
        // lock could rewrite the block map "without mutating it", dropping
        // mappings a concurrent write-through just published.
        //
        // Inline/staged files take the SAME discipline for the same reason:
        // the whole-meta save of an unlocked pre-promotion snapshot raced the
        // merge worker's promotion commit — the commit publishes
        // `block_map[0]` (RAM cache + backend) and releases the ring entry,
        // then the stale save (map=None) erased the mapping from BOTH,
        // stranding the sole copy of the payload: reads wedged re-resolving a
        // moving identity (EIO after the retry bound) or served the D0
        // zeros-degrade for LIVE data, and the next RMW codified the zeros
        // durably (the aged fsx GOOD→0x0000 loss;
        // tests/staged_multifile_ring_pressure_tests.rs
        // extend_vs_promotion_never_strands_payload). Re-resolve the FRESHEST
        // entry and grow its size under INODE_META_LOCKS — the promote/spill
        // commit lock — so the save can never carry a pre-commit layout.
        let file_path = crate::keys::inode_path(ino);
        if let Ok(meta) = self.router.fetch_metadata(&file_path).await {
            if meta.file_type == "striped" {
                let _ = self
                    .router
                    .merge_block_mappings(
                        ino,
                        crate::routing::BlockMapOp::Merge(&[]),
                        target_size,
                        crate::routing::LayoutFlip::KeepLayout,
                        fencing_token,
                    )
                    .await;
            } else {
                let _ = self
                    .router
                    .grow_layout_size(ino, target_size, fencing_token)
                    .await;
            }
        }

        // Update cache (never regress a fresher attr size).
        if let Some((mut attr, _)) = self.attr_cache.get(&ino) {
            if attr.size < target_size {
                attr.size = target_size;
                attr.blocks = target_size.div_ceil(512);
                self.attr_cache
                    .insert(ino, (attr, std::time::Instant::now()));
            }
        }
        Ok(())
    }

    /// Upload a content-complete block directly: crypto → allocate → DMA →
    /// block-map merge (zero-copy write-path design §5.3, PR 4). Caller
    /// holds `BLOCK_FLUSH_LOCKS(ino, b)`; MUST NOT hold the inode write
    /// guard (striped scope is MetaPrepOnly, guard already dropped — P1-8)
    /// nor any pooled meta connection (P1-10: the meta connection opens
    /// after the DMA completed and closes before returning). Returns `Err`
    /// to request the caller's never-lossy staging fallback.
    ///
    /// `plaintext` MUST be lease-free (§5.4 severance boundary): always an
    /// `ActiveBlockBuf::snapshot()` — zero-completed, block-sized,
    /// 4096-aligned (guaranteed `WriteData::Aligned` zero-copy submit) —
    /// never a transport-payload `Bytes`.
    async fn upload_full_block(
        &self,
        ino: u64,
        b: u32,
        plaintext: bytes::Bytes,
        fencing_token: u64,
    ) -> Result<(), SqueezefsError> {
        // RW1 H2 hold-time split: the device leg (crypto → allocate → DMA)
        // vs the map-merge leg below — both under the caller's held block
        // lock.
        let wp_dma = write_phase_start();
        // Passthrough returns the same `Bytes` (0 copy); non-passthrough
        // transforms into a fresh buffer (§5.7).
        let processed = self
            .router
            .get_crypto()
            .process_write_async(plaintext)
            .await?;
        let (be_id, block_allocator, nvme_writer) =
            self.router.backend_router.get_active_backend()?;
        crate::block_allocator::ensure_stored_block_image_fits(
            processed.len(),
            block_allocator.chunk_size(),
            "write-through block upload",
        )?;
        // Marks the key's incarnation unstable: racing validated cache fills
        // of a reused key fail their seqlock check instead of caching
        // pre-DMA bytes.
        let offset = block_allocator.allocate_block().await?;
        // PR VL6a: live-owner registration across the allocate→merge
        // window (drops at function end, after the map merge below).
        let _inflight = block_allocator.inflight_register(offset);
        if let Err(e) = nvme_writer.write_block(offset, processed).await {
            let _ = block_allocator.free_block(offset).await;
            return Err(e);
        }
        write_phase_record(WritePhase::UploadDma, wp_dma);
        // Publish after the device write (incarnation ordering). No
        // `read_lru.put` for the striped hot path — deliberately mirroring
        // `flush_single_active_block`'s `!is_striped` gate: a 10 GiB stream
        // would otherwise evict genuinely hot read data with 2,560 plaintext
        // blocks.
        block_allocator.publish_block(offset);
        let new_key = self.router.backend_router.persist_block_key(&be_id, offset);
        // A no-put owner must PURGE the reused key's read tiers instead
        // (PR 6 equivalence with the deleted `write_striped` direct route,
        // whose unconditional fresh-plaintext put overwrote any stale
        // entry): block keys are offset strings, and a validated fill of
        // the key's DYING incarnation may legally publish its bytes in the
        // window between the previous owner's merge-purge and
        // `free_block`'s retire (the word is still stable there). Such a
        // poison publish strictly precedes our `allocate_block` above
        // (which bumped the incarnation word — later fill re-checks fail),
        // so purging here, after the DMA and before the map names the key,
        // leaves no interleaving that can serve the dead incarnation's
        // bytes for this block.
        self.router.cache.purge_block_key(&new_key);

        // Block-map merge via the shared primitive (§5.3 one merge
        // discipline) under INODE_META_LOCKS: current-map RMW, fencing
        // revalidation, RAM cache republish, displaced-key tier purge. The
        // completed write ends exactly at the block end, so the file is at
        // least that large.
        let min_size = (b as u64 + 1) * self.router.block_size.load(Ordering::Relaxed);
        let entries = [(b, new_key)];
        let wp_merge = write_phase_start();
        let displaced = match self
            .router
            .merge_block_mappings(
                ino,
                crate::routing::BlockMapOp::Merge(&entries),
                min_size,
                crate::routing::LayoutFlip::ToStripedKeepStagedIdentity,
                fencing_token,
            )
            .await
        {
            Ok(d) => d,
            Err(e) => {
                // The DMA'd block is unreachable (never published to the
                // map): free it before surfacing the error.
                let _ = block_allocator.free_block(offset).await;
                return Err(e);
            }
        };
        write_phase_record(WritePhase::UploadMapMerge, wp_merge);
        // Free displaced keys only after the new map is published (durable +
        // cached), so no reader can resolve a block to a key we are freeing.
        for bk in displaced {
            let _ = self.router.backend_router.free_block(&bk).await;
        }

        // Invalidate AFTER the meta publish: a read racing between DMA and
        // publish still hits the RAM snapshot (correct); after removal it
        // resolves via the published block map. Any stale queued
        // WritebackRequest for this key becomes a no-op (its staged source
        // is gone). A stale whole-file RAM snapshot would serve pre-write
        // bytes — drop it, as the routing striped merge does.
        let cache_key = crate::keys::active_block(ino, b as u64).to_string();
        self.retire_parked_overlay(&cache_key);
        // Blocking-pool hop: shard WRITE lock (invariant rule 2).
        self.router
            .cache
            .nvme
            .remove_active_block_async(cache_key)
            .await?;
        let file_path = crate::keys::inode_path(ino);
        self.router.cache.write_lru.remove(&file_path);
        self.router.cache.read_lru.remove(&file_path);
        Ok(())
    }

    async fn flush_active_blocks_with_retry(
        &self,
        ino: u64,
        fencing_token: u64,
    ) -> Result<(), SqueezefsError> {
        let prefix = crate::keys::active_block_ino_prefix(ino);

        // Collect block indices from in-RAM partial buffers only. Do NOT scan the
        // entire staging key space (was O(staged_files) per fsync and dominated
        // small-file sync_all benches as n grew).
        let mut block_indices: Vec<u32> = Vec::new();
        for r in self.active_block_buffers.iter() {
            let key = r.key();
            if !key.starts_with(prefix.as_str()) {
                continue;
            }
            let b_str = key
                .trim_start_matches(prefix.as_str())
                .trim_start_matches("block_");
            if let Ok(b) = b_str.parse::<u32>() {
                block_indices.push(b);
            }
        }

        // Also flush complete active blocks already in the mmap staging segment
        // under known keys (without a full list_keys scan): probe block indices
        // that have a staged active_block entry via the in-RAM set above, plus
        // any indices still referenced by a pending writeback for this ino is
        // handled by the writeback worker. For pure file_id staged small files
        // there are no active_block keys — this returns immediately.
        if !block_indices.is_empty() {
            // Spill RAM buffers to staging first so flush_single can see them.
            self.flush_memory_buffers_for_inode(ino, fencing_token)
                .await?;
            flush_due_active_blocks_for_inode(
                ino,
                block_indices,
                &self.router,
                &self.dlm,
                &self.active_inode_locks,
            )
            .await?;
        }

        Ok(())
    }

    /// Public flush of staged active blocks + dirty layout for an inode.
    /// Propagates I/O errors so callers (fsync, tests) can fail the durable op.
    pub async fn flush_inode_to_backend(
        &self,
        ino: u64,
        fencing_token: u64,
    ) -> Result<(), SqueezefsError> {
        self.flush_memory_buffers_for_inode(ino, fencing_token)
            .await?;
        self.flush_active_blocks_with_retry(ino, fencing_token)
            .await?;
        let file_id_opt = self
            .router
            .metadata_cache
            .get(&ino)
            .and_then(|m| m.file_id.clone());

        let sync_data_fut = async {
            if let Some(file_id) = file_id_opt {
                let key_bytes = bytes::Bytes::copy_from_slice(file_id.as_bytes());
                self.router
                    .cache
                    .nvme
                    .staging_nvme_cache
                    .sync_key(&key_bytes)
                    .await?;
            }
            Ok::<(), SqueezefsError>(())
        };

        let sync_meta_fut = async {
            self.router
                .persist_dirty_layout_if_needed(&crate::keys::inode_path(ino), fencing_token)
                .await?;
            if let Some(backend) = self.meta_backend.as_ref() {
                backend.sync_device_for_ino(ino).await?;
            }
            Ok::<(), SqueezefsError>(())
        };

        tokio::try_join!(sync_data_fut, sync_meta_fut)?;
        Ok(())
    }

    /// Enqueue a writeback request; if the bounded queue is full, flush that block
    /// synchronously so work is never silently dropped (P1-2).
    async fn enqueue_writeback(&self, req: WritebackRequest) -> Result<(), SqueezefsError> {
        match self.writeback_tx.try_send(req) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(r)) => {
                Err(SqueezefsError::InvalidOperation(format!(
                    "writeback channel closed for ino {} block {}",
                    r.ino, r.block_idx
                )))
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(r)) => {
                warn!(
                    "Writeback queue full; synchronous flush for ino {} block {}",
                    r.ino, r.block_idx
                );
                // The unit never enters the queue: its custody transfers to
                // this authoritative flush of the block's newest staged
                // state (owner_token = None).
                flush_due_active_blocks_for_inode(
                    r.ino,
                    vec![r.block_idx],
                    &self.router,
                    &self.dlm,
                    &self.active_inode_locks,
                )
                .await
            }
        }
    }

    /// `(ino, block)` of an `active_block:inode_{ino}:block_{b}` key.
    /// Compose the RAM active-block overlays' AUTHORITATIVE runs over a
    /// base-tier read reply (the never-invisible law's read-side
    /// composer — fstests generic/209): for every block the reply
    /// covers, an extent-repr overlay contributes its parked slabs and a
    /// full-repr overlay its written-coverage runs, both strictly newer
    /// than any base tier by the one-authority invariant. Runs are
    /// captured under short per-entry map guards (no await); the reply
    /// is copied only when an overlay actually intersects it.
    /// The read window's custody fingerprint: per covered block, the
    /// durable binding and the block's CUSTODY-TRANSFER EPOCH (see
    /// `BLOCK_CUSTODY_EPOCHS` — bumped at every overlay / staged-sibling /
    /// extent-record retire). A block's acked bytes move overlay ⇄ sibling
    /// / record ⇄ binding; presence booleans cannot see an A→B→A cycle
    /// completing inside the window (sibling retired into a re-seeded
    /// overlay that write-through retired again — the observed 795 tape),
    /// but every transfer's retire bumps the epoch, so an unchanged
    /// fingerprint proves no transfer crossed the reader's probe gaps.
    /// `None` = not a striped layout (no custody chain to fingerprint).
    /// Bindings ride the freshest RAM metadata entry (every block-map
    /// merge republishes it before retiring the superseded tier).
    /// Build the read window's custody fingerprint (defense #3 of the
    /// moving-custody protocol — see [`ReadCustodyFp`]). `meta`: an
    /// in-hand RAM snapshot to build from (the handler's size-coherency
    /// get — saves the second moka get + per-key clones the old builder
    /// paid); `None` probes the cache fresh (the post-read side, which
    /// must observe current authority). The anomalous map-id-without-map
    /// shape falls back to the authoritative async resolve, exactly the
    /// old builder's arm.
    async fn read_custody_fingerprint(
        &self,
        meta: Option<&crate::routing::CachedMetadata>,
        file_path: &str,
        ino: u64,
        offset: u64,
        len: usize,
    ) -> Option<ReadCustodyFp> {
        if len == 0 {
            return None;
        }
        let fresh;
        let m = match meta {
            Some(m) => m,
            None => {
                fresh = self.router.metadata_cache.get(&ino)?;
                &fresh
            }
        };
        if m.file_type != "striped" {
            return None;
        }
        let block_size = self.router.block_size.load(Ordering::Relaxed);
        if let Some(fp) = ReadCustodyFp::build_sync(m, ino, block_size, offset, len) {
            return Some(fp);
        }
        if m.block_map_id.is_none() {
            // No map, no map-id, no prefix: nothing to fingerprint (the
            // old builder's load error → None arm).
            return None;
        }
        let start_block = (offset / block_size) as u32;
        let end_block = ((offset + len as u64 - 1) / block_size) as u32;
        let bindings = self
            .router
            .load_striped_block_keys(file_path, m, start_block, end_block)
            .await
            .ok()?;
        Some(ReadCustodyFp::Owned(
            bindings
                .into_iter()
                .map(|(b, key)| (b, key, block_custody_epoch(ino, b)))
                .collect(),
        ))
    }

    /// Publish a parked overlay into `active_block_buffers` — the mandated
    /// single insert path (mirror of [`Self::retire_parked_overlay`]): the
    /// O(1) gate increments BEFORE the map publish, so a reader observing
    /// `0` holds a proof no overlay exists (a transient over-count is
    /// conservative — the reader pays an ordinary probe). Replacements
    /// re-balance the count. A raw `.insert()` on the map is a protocol
    /// violation — readers gated on `0` would skip acked bytes.
    fn park_overlay_entry(
        &self,
        key: String,
        buf: crate::cache::active_block::ActiveBlockBuf,
    ) -> Option<crate::cache::active_block::ActiveBlockBuf> {
        self.parked_overlay_count
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        let replaced = self.active_block_buffers.insert(key, buf);
        if replaced.is_some() {
            self.parked_overlay_count
                .fetch_sub(1, std::sync::atomic::Ordering::Release);
        }
        replaced
    }

    /// Capture the RAM-overlay byte runs intersecting `[offset, offset+len)`
    /// as absolute-offset runs. Zero-cost when no overlay exists — one
    /// lock-free O(1) gate load, NEVER `DashMap::is_empty()`/`len()` (an
    /// every-shard rwlock scan: 45 % + 12 % of daemon CPU at 4k-randread
    /// saturation, `.benchmarks/2026-07-25-odirect-randread-concurrency.md`);
    /// with overlays live, one map probe per covered block, runs copied out
    /// under short map guards.
    fn capture_parked_runs(&self, ino: u64, offset: u64, len: usize) -> Vec<(u64, Vec<u8>)> {
        if len == 0
            || self
                .parked_overlay_count
                .load(std::sync::atomic::Ordering::Acquire)
                == 0
        {
            return Vec::new();
        }
        let block_size = self.router.block_size.load(Ordering::Relaxed);
        let end = offset + len as u64;
        let start_block = offset / block_size;
        let end_block = (end - 1) / block_size;
        let mut out = Vec::new();
        for b in start_block..=end_block {
            let key = crate::keys::active_block(ino, b).to_string();
            let Some(entry) = self.active_block_buffers.get(&key) else {
                continue;
            };
            let b_start = b * block_size;
            let rel_lo = offset.max(b_start).saturating_sub(b_start) as usize;
            let rel_hi = (end.min(b_start + block_size) - b_start) as usize;
            // Capture the runs under the guard, then drop it before the
            // (allocation-bearing) compose.
            let runs: Vec<(usize, Vec<u8>)> = if entry.value().is_extent_repr() {
                entry.value().extent_runs_in(rel_lo, rel_hi)
            } else {
                let snapshot = entry.value().snapshot();
                entry
                    .value()
                    .covered_runs_in(rel_lo, rel_hi)
                    .into_iter()
                    .map(|(s, e)| (s, snapshot[s..e].to_vec()))
                    .collect()
            };
            drop(entry);
            for (s, d) in runs {
                out.push((b_start + s as u64, d));
            }
        }
        out
    }

    /// Overlay absolute-offset byte runs onto `data` (read reply base at
    /// `offset`). Allocates only when a run actually intersects the range.
    fn apply_parked_runs(offset: u64, data: bytes::Bytes, runs: &[(u64, Vec<u8>)]) -> bytes::Bytes {
        if data.is_empty() || runs.is_empty() {
            return data;
        }
        let mut out: Option<Vec<u8>> = None;
        for (abs, d) in runs {
            let Some(lo) = abs.checked_sub(offset).map(|v| v as usize) else {
                continue;
            };
            if lo >= data.len() {
                continue;
            }
            let out = out.get_or_insert_with(|| data.to_vec());
            let hi = (lo + d.len()).min(out.len());
            out[lo..hi].copy_from_slice(&d[..hi - lo]);
        }
        match out {
            Some(v) => bytes::Bytes::from(v),
            None => data,
        }
    }

    /// Retire a block's RAM overlay — the custody-transfer form of
    /// `active_block_buffers.remove` (generic/795 moving-custody read
    /// protocol): every overlay retire bumps the block's custody epoch so
    /// lock-free readers whose probe window the transfer crossed re-run.
    /// ALL overlay removals must route here (a raw `.remove()` is a
    /// protocol violation — readers could serve zeros for acked bytes).
    fn retire_parked_overlay(
        &self,
        key: &str,
    ) -> Option<(String, crate::cache::active_block::ActiveBlockBuf)> {
        let removed = self.active_block_buffers.remove(key);
        if removed.is_some() {
            // Gate decrement strictly AFTER the map removal: `0` must
            // always prove the map empty (see `park_overlay_entry`).
            self.parked_overlay_count
                .fetch_sub(1, std::sync::atomic::Ordering::Release);
            if let Some((ino, b)) = Self::parse_active_block_key(key) {
                bump_block_custody_epoch(ino, b);
            }
        }
        removed
    }

    fn parse_active_block_key(key: &str) -> Option<(u64, u32)> {
        let rest = key.strip_prefix("active_block:inode_")?;
        let (ino_str, block_str) = rest.split_once(":block_")?;
        Some((ino_str.parse().ok()?, block_str.parse().ok()?))
    }

    /// R5 gauge: bytes parked in RAM as active block buffers — the RAII
    /// byte gauge (full backings + extent slabs; W2 §5.2), replacing the
    /// count × block-size approximation.
    pub fn parked_buffer_bytes(&self) -> u64 {
        Self::parked_gauge_bytes()
    }

    /// The O(1) parked-overlay gate value (see the field doc): `0` proves
    /// `active_block_buffers` empty — the read hot path skips every
    /// per-block overlay probe on that proof. Exposed for the gate's
    /// contract tests (`tests/parked_overlay_gate_tests.rs`).
    pub fn parked_overlay_gate_count(&self) -> usize {
        self.parked_overlay_count
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// §5.7 Red drain — "early `flush_memory_buffers_*`, the existing
    /// never-lossy staging path, just earlier": flush parked buffers
    /// inode-by-inode through the durable upload/staging path until the
    /// parked gauge is ≤ `target` bytes. The row-5 trace proved
    /// cap-halving alone cannot hold the line: it only gates INSERTS,
    /// while the backlog itself kept growing (spill-to-staging ran 42 %
    /// refused — a 4 MiB entry every ~10 MiB shard — and the rand-write
    /// revisit carousel pulled spilled entries straight back to RAM).
    /// Durability semantics untouched: every buffer goes through
    /// `flush_memory_buffers_for_inode` — upload or staged, never
    /// dropped.
    pub async fn drain_parked_toward(&self, target: u64) {
        let mut last_len = usize::MAX;
        while self.parked_buffer_bytes() > target {
            let len = self.active_block_buffers.len();
            if len == 0 || len >= last_len {
                // No forward progress (writers re-parking as fast as the
                // drain flushes): stop — the sampler re-fires next tick.
                break;
            }
            last_len = len;
            let mut inos: Vec<u64> = self
                .active_block_buffers
                .iter()
                .take(64)
                .filter_map(|r| Self::parse_active_block_key(r.key()).map(|(ino, _)| ino))
                .collect();
            inos.sort_unstable();
            inos.dedup();
            for ino in inos {
                let Ok(fencing_token) = self.get_or_acquire_lease(ino).await else {
                    continue;
                };
                let _ = self
                    .flush_memory_buffers_driven(ino, fencing_token, FlushDriver::ParkedDrain)
                    .await;
                // Wake Red-gated writers after every inode flush — space
                // frees incrementally, admission resumes incrementally.
                self.parked_drain_progress.notify_waiters();
                if self.parked_buffer_bytes() <= target {
                    break;
                }
            }
        }
        self.parked_drain_progress.notify_waiters();
    }

    /// Spawn-once worker behind the parked-drain plumbing: the authority's
    /// Red shed closure and the Red admission gate both post a target to
    /// [`Self::parked_drain_target`] and kick; the worker runs the
    /// never-lossy [`Self::drain_parked_toward`] outside every caller's
    /// lock context (P1-9: the drain takes lease locks (2) and OTHER
    /// blocks' flush locks (3) — never legal from under a writer's own
    /// held block lock, which is exactly why gated writers WAIT instead
    /// of draining inline).
    fn ensure_parked_drain_worker(&self) {
        if self
            .parked_drain_worker_started
            .swap(true, Ordering::Relaxed)
        {
            return;
        }
        let fs = self.clone();
        tokio::spawn(async move {
            loop {
                fs.parked_drain_kick.notified().await;
                let target = fs.parked_drain_target.swap(u64::MAX, Ordering::Relaxed);
                if target == u64::MAX {
                    continue;
                }
                fs.drain_parked_toward(target).await;
            }
        });
    }

    /// The parked-write BYTE gauge (W2 §5.2): full-repr backings + extent
    /// payload slabs — RAII-charged by `ActiveBlockBuf`, so checked-out
    /// buffers keep counting (they are still RAM).
    pub fn parked_gauge_bytes() -> u64 {
        METRICS.parked_full_buffer_bytes.load(Ordering::Relaxed)
            + METRICS.parked_extent_bytes.load(Ordering::Relaxed)
    }

    /// The parked-write byte budget: `parked_cap_buffers()` buffers' worth
    /// (Red halves it — §5.7). The retired 256-COUNT cap's byte form: an
    /// extent overlay charges ~its payload, so the small-write shape no
    /// longer saturates the cap at 256 entries (the inline-spill convoy).
    fn parked_cap_bytes(&self) -> u64 {
        let cap = crate::mem_budget::effective_parked_cap(
            parked_cap_buffers() as usize,
            crate::mem_budget::level(),
        );
        (cap as u64).saturating_mul(self.router.block_size.load(Ordering::Relaxed).max(1))
    }

    /// W2 shared spill loop: bring the parked BYTE gauge back under the
    /// budget by spilling victims — extent overlays as staged
    /// `active_block_ext:` records (4 KiB-class puts, **no seed read at
    /// spill, ever**), full buffers through today's seed-materialize +
    /// whole-image staging put. Callers may hold their own block's lock:
    /// victims are taken `try_lock` (a contended victim — including the
    /// caller's own block — is skipped; the cap is soft, §5.3).
    async fn spill_parked_toward_cap(&self, fencing_token: u64) {
        let cap_bytes = self.parked_cap_bytes();
        'spill: while Self::parked_gauge_bytes() > cap_bytes {
            // Candidate keys snapshotted first so the map is never mutated
            // under a live iterator guard.
            let candidates: Vec<String> = self
                .active_block_buffers
                .iter()
                .take(16)
                .map(|r| r.key().clone())
                .collect();
            if candidates.is_empty() {
                break;
            }
            let mut spilled = false;
            for spill_key in candidates {
                let Some((v_ino, v_b)) = Self::parse_active_block_key(&spill_key) else {
                    continue;
                };
                let victim_lock = BLOCK_FLUSH_LOCKS.get_lock(v_ino, v_b);
                let Ok(_victim_guard) = victim_lock.try_lock() else {
                    // Contended (possibly by this very caller's shard): a
                    // writer/flusher owns this block right now — skip it.
                    block_lock_try_note(BlockLockSite::SpillVictim, v_ino, v_b, false);
                    continue;
                };
                block_lock_try_note(BlockLockSite::SpillVictim, v_ino, v_b, true);

                // W2 extent victim: serialize the overlay as a staged
                // extent record — no seed materialize, no whole-image put.
                // The overlay stays PARKED across the put (readers keep
                // serving it); authority transfers stage-then-remove.
                let is_extent = match self.active_block_buffers.get(&spill_key) {
                    Some(entry) => entry.value().is_extent_repr(),
                    None => continue, // checked out by a racing writer
                };
                if is_extent {
                    let (extents, base_deferred) = match self.active_block_buffers.get(&spill_key) {
                        Some(entry) => {
                            (entry.value().extent_table(), entry.value().seed_deferred())
                        }
                        None => continue,
                    };
                    if extents.is_empty() {
                        // Nothing parked (degenerate): drop the empty shell.
                        self.retire_parked_overlay(&spill_key);
                        spilled = true;
                        break;
                    }
                    let ext_key = crate::keys::active_block_ext(v_ino, v_b as u64).to_string();
                    let record = crate::cache::nvme::ExtentRecord {
                        version: crate::cache::nvme::EXTENT_RECORD_VERSION,
                        fencing_token: self.dlm.get_fencing_token_ino(v_ino),
                        block_idx: v_b,
                        base_deferred,
                        extents,
                    };
                    let payload = record.payload_bytes();
                    let nvme = self.router.cache.nvme.clone();
                    let wp_put = write_phase_start();
                    let admitted = tokio::task::spawn_blocking(move || {
                        nvme.put_extent_record(&ext_key, &record)
                    })
                    .await
                    .unwrap_or(false);
                    write_phase_record(WritePhase::StagingPut, wp_put);
                    if !admitted {
                        // Staging refused (never-lossy backpressure): the
                        // extents stay in RAM; the cap is soft.
                        break 'spill;
                    }
                    METRICS.extent_spills.fetch_add(1, Ordering::Relaxed);
                    METRICS
                        .extent_spill_bytes
                        .fetch_add(payload, Ordering::Relaxed);
                    // Authority transferred stage-then-remove (the record
                    // is a superset snapshot of the overlay).
                    self.retire_parked_overlay(&spill_key);
                    spilled = true;
                    break;
                }

                // Full-repr victim: today's path — zero-complete /
                // seed-materialize under the victim's lock, whole-image
                // staging put. OVERLAY NEVER INVISIBLE: the victim stays
                // PARKED across every await; the held victim lock excludes
                // mutators. Item B: a deferred victim owes old bytes first —
                // on a failed fetch it just stays parked (never-lossy).
                let deferred = match self.active_block_buffers.get(&spill_key) {
                    Some(entry) => entry.value().seed_deferred(),
                    None => continue, // checked out by a racing writer meanwhile
                };
                if deferred {
                    let file_path = crate::keys::inode_path(v_ino);
                    let Ok(image) = self.fetch_seed_image(&file_path, v_b).await else {
                        continue;
                    };
                    // RW1 ledger bucket 1, read leg: the foreground writer
                    // materializing a VICTIM's deferred RMW seed — the §1.2
                    // "spill seed" device read.
                    if let Some(img) = image.as_deref() {
                        METRICS.spill_seed_reads.fetch_add(1, Ordering::Relaxed);
                        METRICS
                            .spill_seed_read_bytes
                            .fetch_add(img.len() as u64, Ordering::Relaxed);
                    }
                    if let Some(mut entry) = self.active_block_buffers.get_mut(&spill_key) {
                        entry
                            .value_mut()
                            .fill_complement_from(image.as_deref().unwrap_or(&[]));
                    }
                }
                let snapshot = match self.active_block_buffers.get_mut(&spill_key) {
                    Some(mut entry) => {
                        entry.value_mut().zero_complete();
                        entry.value().snapshot()
                    }
                    None => continue,
                };
                let spill_len = snapshot.len() as u64;
                // Blocking-pool hop: shard WRITE lock (invariant rule 2).
                let wp_put = write_phase_start();
                let admitted = self
                    .router
                    .cache
                    .nvme
                    .put_active_block_async(spill_key.clone(), snapshot, fencing_token)
                    .await
                    .unwrap_or(false);
                write_phase_record(WritePhase::StagingPut, wp_put);
                if admitted {
                    // RW1 ledger bucket 1, write leg: the inline victim
                    // spill's staging put.
                    METRICS.spill_staging_puts.fetch_add(1, Ordering::Relaxed);
                    METRICS
                        .spill_staging_put_bytes
                        .fetch_add(spill_len, Ordering::Relaxed);
                }
                if !admitted {
                    // Staging refused (never-lossy backpressure): keep the
                    // buffer in RAM — exceeding the soft cap beats losing
                    // dirty data. fsync drains it durably.
                    break 'spill;
                }
                // Authority transferred stage-then-remove (identical copy).
                self.retire_parked_overlay(&spill_key);
                spilled = true;
                break;
            }
            if !spilled {
                // Every candidate was contended or vanished: soft cap.
                break;
            }
        }
    }

    /// The R5 byte-budget admission pass for an ALREADY-PARKED active
    /// block (the never-invisible successor of the old
    /// `insert_active_block_buffer`, whose by-value insert was half of
    /// the generic/209 checkout window — entries now stay in the map at
    /// every instant and this pass runs on them in place). Caller holds
    /// this block's `BLOCK_FLUSH_LOCKS` guard.
    async fn admit_parked_active_block(&self, cache_key: &str, fencing_token: u64) {
        // R5 Red (§5.7): the spill threshold halves — parked dirty bytes
        // reach the existing never-lossy staging path at half the byte
        // budget (the fast, guaranteed RSS reducer of the row-5 cage
        // shape). W2: the budget is BYTES (extent overlays charge their
        // payload), enforced by the shared spill loop.
        self.spill_parked_toward_cap(fencing_token).await;
        // R5 Red BLOCKING admission (§5.7, finding #2): below Red the cap
        // stays soft (spill-or-keep, today's semantics). At Red — the
        // authority already shedding — parking past the halved cap is the
        // measured OOM engine (staging refused 100% of spills on the
        // saturation suite; the parked set ballooned 256 → 1,937 buffers
        // = 7.6 GiB anon while the 1 Hz drain lost the race). Two-stage
        // awaited backpressure (the §4.4-pt-5 ring-admission precedent —
        // no spin, no lock acquired while waiting):
        //
        // 1. DRAIN ASSIST (brief, env-tunable): post a target and give
        //    the shared drain worker a moment to free room — the cheap
        //    path when the backlog is other inodes' cold parks.
        // 2. SELF-FLUSH: still over cap ⇒ THIS block goes durable now
        //    through `upload_active_block_bytes` (the fsync
        //    staging-refusal escalation — upload + merge under the block
        //    guard the caller already holds, the established 3 → 3.5
        //    extended order) and retires the parked entry; RAM frees at
        //    the removal. Leg B measured why waiting longer is wrong: a
        //    drain CONVOY (writers of the same hot inode queueing behind
        //    one flush pass) produced 36 ten-second deadline stalls —
        //    the writer paying for its own block's writeback is bounded
        //    by the device, not by the convoy.
        //
        // Never-lossy at every exit: assist parks nothing it does not
        // own; self-flush makes the bytes durable BEFORE the entry
        // retires; self-flush FAILURE keeps the entry parked past the cap
        // loud (parked_gate_timeouts) — RAM is the last-resort custody of
        // dirty data, exactly like the staging-refusal soft-cap leg.
        if crate::mem_budget::level() == crate::mem_budget::Level::Red {
            // W2: the Red admission gate is the BYTE budget's halved form
            // (extent overlays charge payload bytes, full buffers a block).
            let gate_bytes = self.parked_cap_bytes();
            let incoming = match self.active_block_buffers.get(cache_key) {
                Some(e) => e
                    .value()
                    .extent_payload_bytes()
                    .max(if e.value().is_extent_repr() {
                        0
                    } else {
                        self.router.block_size.load(Ordering::Relaxed)
                    }),
                // Entry gone (a concurrent spill of it under our own
                // spill pass): nothing to admit.
                None => return,
            };
            // NOTE: `incoming` is already charged in the parked gauge
            // (RAII on the buffer) — the gate compares against the same
            // total the old by-value path saw at its insert instant.
            if Self::parked_gauge_bytes() > gate_bytes {
                METRICS.parked_gate_waits.fetch_add(1, Ordering::Relaxed);
                let assist_ms = std::env::var("SQUEEZEFS_PARKED_GATE_ASSIST_MS")
                    .ok()
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(200);
                let deadline = std::time::Instant::now() + Duration::from_millis(assist_ms);
                while Self::parked_gauge_bytes() > gate_bytes
                    && crate::mem_budget::level() == crate::mem_budget::Level::Red
                    && std::time::Instant::now() < deadline
                {
                    let cap_bytes = gate_bytes.saturating_sub(incoming);
                    self.parked_drain_target
                        .fetch_min(cap_bytes, Ordering::Relaxed);
                    self.ensure_parked_drain_worker();
                    self.parked_drain_kick.notify_one();
                    let progress = self.parked_drain_progress.notified();
                    tokio::select! {
                        _ = progress => {}
                        _ = tokio::time::sleep(Duration::from_millis(25)) => {}
                    }
                }
                let still_over = Self::parked_gauge_bytes() > gate_bytes
                    && crate::mem_budget::level() == crate::mem_budget::Level::Red;
                if still_over {
                    if let Some((ino, b)) = Self::parse_active_block_key(cache_key) {
                        // W2: an extent overlay reaching the Red self-flush
                        // escalates to full IN THE MAP first — the
                        // whole-image choreography (seed / zero-complete /
                        // upload) then applies to the parked entry.
                        // OVERLAY NEVER INVISIBLE: the entry stays parked
                        // across every await (the caller holds this
                        // block's lock, so no other mutator can race);
                        // a failed fetch/upload leaves it parked
                        // (never-lossy; the gate stays soft for it).
                        let deferred = match self.active_block_buffers.get_mut(cache_key) {
                            Some(mut e) => {
                                e.value_mut().escalate_to_full();
                                e.value().seed_deferred()
                            }
                            None => return,
                        };
                        if deferred {
                            // Item B: deferred buffers owe old bytes first.
                            let file_path = crate::keys::inode_path(ino);
                            let Ok(image) = self.fetch_seed_image(&file_path, b).await else {
                                return;
                            };
                            METRICS.flush_seed_read_bytes.fetch_add(
                                image.as_deref().map(|d| d.len()).unwrap_or(0) as u64,
                                Ordering::Relaxed,
                            );
                            if let Some(mut e) = self.active_block_buffers.get_mut(cache_key) {
                                e.value_mut()
                                    .fill_complement_from(image.as_deref().unwrap_or(&[]));
                            } else {
                                return;
                            }
                        }
                        // §5.3 exit 2: zero-complete Fresh buffers before
                        // their bytes leave RAM (hole intervals materialize
                        // as zeros — recycled pool bytes never escape).
                        let self_flush_snapshot = match self.active_block_buffers.get_mut(cache_key)
                        {
                            Some(mut e) => {
                                e.value_mut().zero_complete();
                                e.value().snapshot()
                            }
                            None => return,
                        };
                        let self_flush_len = self_flush_snapshot.len() as u64;
                        match upload_active_block_bytes(ino, b, self_flush_snapshot, &self.router)
                            .await
                        {
                            Ok(()) => {
                                METRICS
                                    .parked_gate_self_flushes
                                    .fetch_add(1, Ordering::Relaxed);
                                // RW1 ledger bucket 3: the Red parked-gate
                                // self-flush's durable upload.
                                METRICS
                                    .durable_upload_bytes_self_flush
                                    .fetch_add(self_flush_len, Ordering::Relaxed);
                                // Durable + published: the entry retires.
                                self.retire_parked_overlay(cache_key);
                                // A staler STAGED image under this key (an
                                // earlier spill) must not outlive the newer
                                // durable merge — same fencing-checked
                                // remove as flush_one_active_block, on the
                                // blocking pool (shard write lock).
                                let nvme = self.router.cache.nvme.clone();
                                let key = cache_key.to_string();
                                let token = fencing_token;
                                let _ = tokio::task::spawn_blocking(move || {
                                    match nvme.get_staged_fencing_token(&key) {
                                        Some(tok) if tok != token => {}
                                        _ => {
                                            nvme.remove_active_block(&key);
                                        }
                                    }
                                })
                                .await;
                            }
                            Err(e) => {
                                METRICS.parked_gate_timeouts.fetch_add(1, Ordering::Relaxed);
                                warn!(
                                    "Red parked-buffer gate: self-flush of ino {} block {} \
                                     failed ({:?}) — staying parked past the cap ({} buffers) \
                                     to preserve the dirty bytes",
                                    ino,
                                    b,
                                    e,
                                    self.active_block_buffers.len()
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    pub async fn flush_all_memory_buffers_to_staging(&self) -> Result<(), SqueezefsError> {
        info!("FUSE Daemon: Force flushing all in-memory write buffers to local NVMe staging...");
        let keys_to_flush: Vec<String> = self
            .active_block_buffers
            .iter()
            .map(|r| r.key().clone())
            .collect();

        for key in keys_to_flush {
            let Some((ino, b)) = Self::parse_active_block_key(&key) else {
                continue;
            };
            // W2 (§5.2 clean-unmount mandate): extent overlays FOLD
            // durably at teardown — a cleanly-shut-down staging dir
            // contains no extent records (zero clean-downgrade exposure).
            let is_extent = self
                .active_block_buffers
                .get(&key)
                .map(|e| e.value().is_extent_repr())
                .unwrap_or(false);
            if is_extent {
                match self.fold_extent_block(ino, b).await {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(e) => {
                        error!(
                            "dismount: extent fold for ino {ino} block {b} failed ({e:?}); \
                             extents stay parked (D0)"
                        );
                        continue;
                    }
                }
            }
            // Stage/upload exit under the victim's block lock (§5.3 exit 2;
            // normal await — teardown holds no other block locks):
            // zero-complete Fresh buffers before they leave RAM.
            let block_guard = block_lock_acquire(ino, b, BlockLockSite::FlushExit).await;
            // OVERLAY NEVER INVISIBLE: the buffer stays PARKED across every
            // await here too — teardown races the last reads/FORGETs, and
            // the same transparency rules apply (see
            // flush_memory_buffers_for_inode).
            let deferred = match self.active_block_buffers.get(&key) {
                Some(entry) => entry.value().seed_deferred(),
                None => {
                    drop(block_guard);
                    continue;
                }
            };
            if deferred {
                // Item B teardown exit: on a failed fetch SKIP this buffer
                // (unfsynced loss is D0-legal; staging a zeros-codified
                // block is corruption). It stays parked until process end.
                let file_path = crate::keys::inode_path(ino);
                match self.fetch_seed_image(&file_path, b).await {
                    Ok(image) => {
                        METRICS.flush_seed_read_bytes.fetch_add(
                            image.as_deref().map(|d| d.len()).unwrap_or(0) as u64,
                            Ordering::Relaxed,
                        );
                        if let Some(mut entry) = self.active_block_buffers.get_mut(&key) {
                            entry
                                .value_mut()
                                .fill_complement_from(image.as_deref().unwrap_or(&[]));
                        }
                    }
                    Err(_) => {
                        error!(
                            "dismount: deferred RMW seed for ino {ino} block {b} unreadable; \
                             leaving unflushed (D0)"
                        );
                        drop(block_guard);
                        continue;
                    }
                }
            }
            let staging_copy = match self.active_block_buffers.get_mut(&key) {
                Some(mut entry) => {
                    entry.value_mut().zero_complete();
                    entry.value().snapshot()
                }
                None => {
                    drop(block_guard);
                    continue;
                }
            };
            let fencing_token = self.dlm.get_fencing_token_ino(ino);

            let nvme_clone = self.router.cache.nvme.clone();
            let key_clone = key.clone();
            let staging_snapshot = staging_copy.clone();
            let put_len = staging_copy.len() as u64;
            let admitted = match tokio::task::spawn_blocking(move || {
                nvme_clone.put_active_block(&key_clone, &staging_snapshot, fencing_token)
            })
            .await
            {
                Ok(admitted) => admitted,
                Err(e) => {
                    error!(
                        "Failed to write active block to NVMe staging during dismount: {:?}",
                        e
                    );
                    continue;
                }
            };

            if !admitted {
                // Staging refused (never-lossy backpressure): dismount must
                // not strand dirty RAM — upload the block durably right now
                // (the escalation merges via the shared primitive, legal
                // under the block guard per the P1-9 extended order).
                if let Err(e) = upload_active_block_bytes(ino, b, staging_copy, &self.router).await
                {
                    error!(
                        "Dismount durable upload failed for ino {} block {}: {:?}",
                        ino, b, e
                    );
                } else {
                    METRICS
                        .durable_upload_bytes_escalation
                        .fetch_add(put_len, Ordering::Relaxed);
                    self.retire_parked_overlay(&key);
                }
                continue;
            }
            METRICS
                .staging_put_bytes_teardown
                .fetch_add(put_len, Ordering::Relaxed);
            // Authority transferred stage-then-remove (identical copy).
            self.retire_parked_overlay(&key);
            drop(block_guard);

            let req = WritebackRequest {
                ino,
                block_idx: b,
                fencing_token,
                attempts: 0,
            };
            if let Err(e) = self.enqueue_writeback(req).await {
                error!(
                    "Failed to enqueue writeback during dismount for ino {}: {:?}",
                    ino, e
                );
            } else {
                METRICS
                    .writeback_enqueued_teardown
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        info!("FUSE Daemon: All in-memory write buffers flushed to local NVMe staging.");
        Ok(())
    }

    pub async fn flush_all_staged_blocks_to_backend(&self) -> TeardownFlushSummary {
        info!("FUSE Daemon: Force flushing all staged active blocks to NVMe-oF backend...");

        // W2 (§5.2 clean-unmount mandate): fold every staged extent record
        // FIRST — fold consumes any staged-full sibling as its seed base,
        // so the ordinary sweep below never flushes a superseded image.
        for key in self.router.cache.nvme.extent_record_keys("") {
            let Some((ino, b)) = Self::parse_extent_record_key(&key) else {
                continue;
            };
            if let Err(e) = self.fold_extent_block(ino, b).await {
                error!(
                    "dismount: extent-record fold for ino {ino} block {b} failed ({e:?}); \
                     the record stays staged (recovered at next mount)"
                );
            }
        }

        let keys = self.router.cache.nvme.list_staged_files();

        let mut active_keys = Vec::new();
        for key in keys {
            if key.starts_with("active_block:") {
                active_keys.push(key);
            }
        }

        let mut summary = TeardownFlushSummary {
            attempted: active_keys.len(),
            ..Default::default()
        };
        if active_keys.is_empty() {
            info!("FUSE Daemon: No staged active blocks to flush.");
            return summary;
        }

        info!(
            "FUSE Daemon: Found {} staged active blocks to flush.",
            active_keys.len()
        );

        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(
            crate::bg_admit::striped_block_concurrency(),
        ));
        let mut tasks = futures::stream::FuturesUnordered::new();

        for key in active_keys {
            let sem_clone = sem.clone();
            let router_clone = self.router.clone();
            let dlm_clone = self.dlm.clone();
            let locks_clone = self.active_inode_locks.clone();

            tasks.push(tokio::spawn(async move {
                let _permit = sem_clone.acquire().await.ok();

                let parts: Vec<&str> = key.split(":block_").collect();
                if parts.len() != 2 {
                    return Ok(());
                }
                let ino_parts: Vec<&str> = parts[0].split("inode_").collect();
                if ino_parts.len() != 2 {
                    return Ok(());
                }
                let ino = match ino_parts[1].parse::<u64>() {
                    Ok(i) => i,
                    Err(_) => return Ok(()),
                };
                let b = match parts[1].parse::<u32>() {
                    Ok(idx) => idx,
                    Err(_) => return Ok(()),
                };

                let meta = router_clone
                    .fetch_metadata_from_backend(ino)
                    .await?
                    .unwrap_or_default();
                let is_striped = meta.file_type == "striped";

                // Authoritative dismount sweep (owner_token = None): flush
                // whatever is staged regardless of the generation it was
                // stamped under — acked custody bytes must reach the
                // backend, and the entry must DRAIN so destroy's
                // staged-drain wait is bounded (FIND-M11-A).
                flush_single_active_block(
                    ino,
                    b,
                    None,
                    &router_clone,
                    &dlm_clone,
                    &locks_clone,
                    is_striped,
                )
                .await?;

                Ok::<(), SqueezefsError>(())
            }));
        }

        use futures::StreamExt;
        const ERROR_SAMPLES: usize = 3;
        while let Some(res) = tasks.next().await {
            match res {
                Err(join_err) => {
                    summary.failed += 1;
                    if summary.error_samples.len() < ERROR_SAMPLES {
                        summary
                            .error_samples
                            .push(format!("task panicked: {join_err:?}"));
                    }
                }
                Ok(Err(e)) => {
                    summary.failed += 1;
                    if summary.error_samples.len() < ERROR_SAMPLES {
                        summary.error_samples.push(format!("{e:?}"));
                    }
                }
                Ok(Ok(())) => summary.flushed += 1,
            }
        }

        // One aggregated report, never a line per block: an unmount racing
        // deleted files or an offline backend produces thousands of
        // identical failures (orphan active blocks are dropped with the
        // segment either way).
        if summary.failed > 0 {
            warn!(
                "FUSE Daemon: dismount active-block flush: {} flushed, {} failed of {} (first errors: {:?})",
                summary.flushed, summary.failed, summary.attempted, summary.error_samples
            );
        } else {
            info!(
                "FUSE Daemon: Force flush of staged active blocks completed ({} flushed).",
                summary.flushed
            );
        }
        summary
    }

    pub async fn force_flush_all_staged_data(&self) -> Result<(), SqueezefsError> {
        let _ = self.flush_all_memory_buffers_to_staging().await;
        let _ = self.flush_all_staged_blocks_to_backend().await;
        Ok(())
    }

    /// Whether dismount teardown has begun (destroy runs once per unmount;
    /// duplicate per-queue invocations are no-ops).
    pub fn dismount_started(&self) -> bool {
        self.dismount_once.load(Ordering::Acquire)
    }

    /// The largest representable file size: block indices are **u32**
    /// across the striped layout (`block_map` keys, patch/extent records,
    /// prefetch lanes), so content past `block_size × (2^32 − 1)` cannot
    /// be addressed. Every size-growing entry point (write, truncate,
    /// fallocate, copy_file_range dest) refuses EFBIG at this boundary —
    /// the pre-fix paths silently wrapped the index mod 2^32 and read
    /// back zeros (fstests generic/525; pinned in
    /// tests/sparse_write_bounded_tests.rs). Widening the index space is
    /// an on-disk layout-key change; at the default 4 MiB block size the
    /// cap is ~16 EiB−4 MiB, far past the i64 VFS ceiling — only small
    /// custom block sizes ever observe it.
    fn max_file_size(&self) -> u64 {
        let bs = self.router.block_size.load(Ordering::Relaxed);
        (i64::MAX as u64).min(bs.saturating_mul(u32::MAX as u64))
    }

    fn mode_to_file_type(&self, mode: u32) -> FileType {
        match mode & libc::S_IFMT {
            libc::S_IFDIR => FileType::Directory,
            libc::S_IFLNK => FileType::Symlink,
            libc::S_IFIFO => FileType::NamedPipe,
            libc::S_IFCHR => FileType::CharDevice,
            libc::S_IFBLK => FileType::BlockDevice,
            libc::S_IFSOCK => FileType::Socket,
            _ => FileType::RegularFile,
        }
    }

    /// D2.c (design-metadata-throughput §5.2, PR M5): re-seed an inode's
    /// cached attrs from the RAM-authoritative backend instead of
    /// invalidating them after a directory mutation.
    ///
    /// Why: the kernel invalidates its own parent-dir attrs on every
    /// create/unlink/rename (`fuse_dir_changed`), and the next path walk's
    /// permission check forces a GETATTR the daemon cannot suppress
    /// (M2 measured 1.17/create and 1.82/unlink at ~45 µs each). The
    /// daemon-side `attr_cache.invalidate(&parent)` made each of those a
    /// contended backend fetch; refreshing here turns them into ~µs cache
    /// hits with values that are exact (backend-authoritative, monotone
    /// through the M6 pending-times fold — never a hand-rolled clock that
    /// could regress on the next real fetch).
    ///
    /// Failure degrades to the pre-M5 invalidate (next getattr refetches).
    ///
    /// `pub(crate)`: the IPC read handoff (`crate::ipc_service`) re-seeds
    /// a cold attr cache with it so warm workloads return to the §5.5.1
    /// sync fast path after one miss demotion.
    pub(crate) async fn refresh_attr_cache(&self, ino: u64) {
        let Some(backend) = self.meta_backend.as_ref() else {
            self.attr_cache.invalidate(&ino);
            return;
        };
        match backend.getattr(ino).await {
            Ok(inode) => {
                let attr = self.inode_to_file_attr(&inode);
                self.attr_cache
                    .insert(ino, (attr, std::time::Instant::now()));
                METRICS
                    .fuse_attr_cache_refreshes
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => self.attr_cache.invalidate(&ino),
        }
    }

    /// Per-class dentry TTL (survey P1-C, the DAOS dir-vs-file split):
    /// directory dentries get `dir_entry` (their invalidation cost covers
    /// whole subtrees), everything else `entry`.
    fn entry_ttl_for(&self, kind: FileType) -> Duration {
        if kind == FileType::Directory {
            self.kernel_ttls.dir_entry
        } else {
            self.kernel_ttls.entry
        }
    }

    fn inode_to_file_attr(&self, inode: &crate::meta_backend::Inode) -> FileAttr {
        FileAttr {
            ino: inode.ino,
            size: inode.size,
            blocks: inode.size.div_ceil(512),
            atime: as_timestamp(inode.atime),
            mtime: as_timestamp(inode.mtime),
            ctime: as_timestamp(inode.ctime),
            kind: self.mode_to_file_type(inode.mode),
            perm: (inode.mode & 0o7777) as u16,
            nlink: inode.nlink,
            uid: inode.uid,
            gid: inode.gid,
            rdev: inode.rdev,
            blksize: 4096,
        }
    }

    /// PR K7 (§5.1): one streaming-readdir page of
    /// `(resume_cookie, entry)` pairs. Offsets at or above the
    /// virtual-entry cookies never reach the backend: nothing real lives
    /// there, and they do not decode as §5.1 cookies.
    async fn readdir_v3_page(
        &self,
        parent: u64,
        offset: u64,
        max: usize,
    ) -> Result<Vec<(u64, crate::meta_backend::DirEntry)>, SqueezefsError> {
        let backend = self.meta_backend.as_ref().ok_or_else(|| {
            SqueezefsError::InvalidOperation("Metadata backend not initialized".to_string())
        })?;
        if offset >= READDIR_VIRTUAL_CONFIG_COOKIE {
            return Ok(Vec::new());
        }
        backend.readdir_stream(parent, offset, max).await
    }

    /// [`Self::readdir_v3_page`] behind the §4.5 small-directory cache:
    /// directories ≤ [`DIR_ENTRY_CACHE_MAX_ENTRIES`] are snapshotted
    /// cookie-ascending on a listing start and pages are served as cache
    /// slices (the repeat-`readdir` hot shape); larger directories bypass
    /// and stream page-by-page — never OOMing the cache. Cache-served
    /// pages carry the stored §5.1 cookies, so the resume contract is
    /// identical on both paths.
    async fn v3_listing_page(
        &self,
        parent: u64,
        offset: u64,
        max: usize,
    ) -> Result<Vec<(u64, crate::meta_backend::DirEntry)>, SqueezefsError> {
        // PR M4 (D1.c): the snapshot generation is read BEFORE any backend
        // page is fetched — a mutation landing mid-build bumps the parent's
        // generation, so the snapshot below is inserted under a superseded
        // key and can never serve stale (the pre-M4 invalidate-then-insert
        // race). Serving and inserting both key on `(parent, gen)`.
        let gen = self.dir_generation(parent);
        let slice_page = |cached: &std::sync::Arc<[(std::boxed::Box<str>, u64, u64, u32)]>|
         -> Vec<(u64, crate::meta_backend::DirEntry)> {
            // §5.1 resume rule: offsets 0/1/2 ⇒ the start; c ≥ 3 ⇒
            // strictly after cookie c (entries are cookie-ascending).
            let start = if offset < 3 {
                0
            } else {
                cached.partition_point(|e| e.2 <= offset)
            };
            cached[start..]
                .iter()
                .take(max)
                .map(|(name, ino, cookie, ft)| {
                    (
                        *cookie,
                        crate::meta_backend::DirEntry {
                            ino: *ino,
                            name: name.to_string(),
                            file_type: *ft,
                        },
                    )
                })
                .collect()
        };
        if let Some(cached) = self.dir_entry_cache_v3.get(&(parent, gen)) {
            return Ok(slice_page(&cached));
        }
        // Listing start on an uncached directory: probe up to the cache
        // cap + 1; small directories snapshot (with cookies), larger ones
        // hand back the streamed prefix and stay stream-only.
        if offset < 3 {
            let mut all: Vec<(u64, crate::meta_backend::DirEntry)> = Vec::new();
            let mut cursor = 0u64;
            loop {
                let page = self
                    .readdir_v3_page(parent, cursor, V3_READDIR_PAGE)
                    .await?;
                let short = page.len() < V3_READDIR_PAGE;
                if let Some((c, _)) = page.last() {
                    cursor = *c;
                }
                all.extend(page);
                if short || all.len() > DIR_ENTRY_CACHE_MAX_ENTRIES {
                    break;
                }
            }
            if all.len() <= DIR_ENTRY_CACHE_MAX_ENTRIES {
                let snapshot: std::sync::Arc<[(std::boxed::Box<str>, u64, u64, u32)]> = all
                    .iter()
                    .map(|(cookie, d)| {
                        (d.name.clone().into_boxed_str(), d.ino, *cookie, d.file_type)
                    })
                    .collect::<Vec<_>>()
                    .into();
                self.dir_entry_cache_v3.insert((parent, gen), snapshot);
            }
            all.truncate(max);
            return Ok(all);
        }
        self.readdir_v3_page(parent, offset, max).await
    }

    async fn get_attr_internal(&self, ino: u64) -> Result<FileAttr, SqueezefsError> {
        let mut attr = match self.attr_cache.get(&ino) {
            // Daemon-side freshness window follows the attr TTL knob: an
            // operator raising the kernel-facing TTL accepts the same
            // staleness bound daemon-side.
            Some((attr, cached_at)) if cached_at.elapsed() < self.kernel_ttls.attr => attr,
            _ => {
                let backend = self.meta_backend.as_ref().ok_or_else(|| {
                    SqueezefsError::InvalidOperation("Metadata backend not initialized".to_string())
                })?;
                let inode = backend.getattr(ino).await?;
                let attr = self.inode_to_file_attr(&inode);
                self.attr_cache
                    .insert(ino, (attr, std::time::Instant::now()));
                attr
            }
        };

        // Size coherency: `write_file` updates `router.metadata_cache` size
        // synchronously, but the durable inode/layout size only catches up on
        // the deferred flush. Trust the hot metadata cache as the authoritative
        // logical size for regular files (both write and truncate keep it
        // current) so a stat/read/mmap issued between a write and its flush
        // never observes a stale size (e.g. 0 on a freshly written file) — which
        // otherwise truncates reads to zero and SIGBUSes mmap under the FUSE
        // writeback cache.
        if attr.kind == FileType::RegularFile {
            if let Some(m) = self.router.metadata_cache.get(&ino) {
                if m.size != attr.size {
                    attr.size = m.size;
                    attr.blocks = m.size.div_ceil(512);
                    self.attr_cache
                        .insert(ino, (attr, std::time::Instant::now()));
                }
            }
        }
        debug!("get_attr_internal returning: {:?}", attr);
        Ok(attr)
    }

    pub async fn complete_active_multipart_upload_if_any(
        &self,
        _ino: u64,
    ) -> Result<(), SqueezefsError> {
        Ok(())
    }

    /// Reclaim a BATCH of orphaned inos (`nlink == 0`, FORGET'd) with one
    /// group-committed destroy transaction per volume (design §4.5, PR 5).
    ///
    /// Preserves today's per-ino split around the destroy:
    /// - admission re-checks (reserved / open / getattr / nlink) per ino —
    ///   the drain-time complement of `queue_reclaim_inode`'s enqueue check;
    /// - `router.delete_file` (data-path teardown) runs BEFORE admission,
    ///   log-and-proceed on failure exactly as today (its result was always
    ///   discarded; gating on it would leak the slot forever under a
    ///   persistently failing data teardown — the slot zero is the
    ///   authoritative reclaim, blocks are refcount-recoverable);
    /// - lease / POSIX-lock / cache teardown runs per ino AFTER the batch's
    ///   commit, on success AND failure alike (invalidating before a
    ///   now-deferred zero would let a straggling getattr repopulate
    ///   attr_cache from the still-valid slot and survive as a ghost).
    ///
    /// `pub` because it is the reclaim worker's unit of work and the
    /// integration seam the bisect tests drive directly.
    pub async fn reclaim_orphaned_batch(&self, inos: Vec<u64>) {
        let _permit = self.reclaim_semaphore.acquire().await.ok();
        let Some(backend) = self.meta_backend.as_ref() else {
            return;
        };

        let mut admitted = Vec::with_capacity(inos.len());
        for ino in inos {
            if ino <= 1 || ino == CONFIG_INODE || ino == STATS_INODE {
                continue;
            }
            // OPEN/RECLAIM HANDSHAKE (fstests generic/795 — the destroy-
            // under-a-live-fd race): claim the in-flight slot FIRST, then
            // re-check the open count. The open path takes the mirrored
            // order (add_open, THEN refuse if the slot is claimed), so every
            // interleaving resolves one way: either this admission sees the
            // open and backs out, or the open sees the claim and fails
            // ENOENT — a destroyed inode can never be readable through a
            // just-granted handle. The old order admitted at count == 0 and
            // a racing OPEN landed after the check; reads on that live fd
            // then found the destroyed ino (size 0 / NotFound) and the
            // KERNEL zero-extended the short replies to its cached i_size
            // (fuse_short_read) — full-length foreign zeros, sticky in the
            // page cache (the generic/795 cmp signature, tape-proven:
            // OPEN → 5 good chunks → RECLAIM-ADMIT → EMPTY fsize 0).
            //
            // Single-drive guard (FIND-RW5-A face 4) unchanged: only ONE
            // reclaim may ever run an ino's data teardown — duplicate
            // drives (release+forget enqueues, racing batches) double-freed
            // the mapped blocks.
            // Cheap gates FIRST, without claiming: a live (nlink > 0) or
            // erroring ino must never even transiently enter the in-flight
            // set — FORGET storms (drop_caches) enqueue live inos
            // constantly, and a transient claim turned racing OPENs of
            // healthy files into spurious ENOENT.
            if self.is_open(ino) {
                debug!("RECLAIM: ino = {} is currently open, skipping reclaim", ino);
                continue;
            }
            match backend.getattr(ino).await {
                Ok(inode) if inode.nlink > 0 => {
                    debug!(
                        "RECLAIM: ino = {} has nlink = {}, skipping reclaim",
                        ino, inode.nlink
                    );
                    continue;
                }
                Ok(_) => {}
                Err(e) => {
                    debug!("RECLAIM: getattr({}) failed: {:?}", ino, e);
                    continue;
                }
            }
            // Claim, then RE-CHECK the open count (the handshake's load-
            // bearing order): the open path counts itself first and then
            // refuses on a visible claim, so an OPEN racing this admission
            // either lands its count before our re-check (we back out) or
            // sees the claim and fails ENOENT — correct either way, because
            // nlink is already 0 here (the file IS unlinked; ENOENT is the
            // honest outcome for an open that lost the race to rm).
            //
            // Single-drive guard (FIND-RW5-A face 4) unchanged: only ONE
            // reclaim may ever run an ino's data teardown — duplicate
            // drives (release+forget enqueues, racing batches) double-freed
            // the mapped blocks.
            if self.reclaim_inflight.insert_sync(ino).is_err() {
                debug!("RECLAIM: ino = {ino} already in flight, skipping");
                continue;
            }
            if self.is_open(ino) {
                debug!(
                    "RECLAIM: ino = {} opened during admission, backing out",
                    ino
                );
                self.reclaim_inflight.remove_sync(&ino);
                continue;
            }
            admitted.push(ino);
        }
        if admitted.is_empty() {
            return;
        }

        // Data-path teardown per ino, before admission — log-and-proceed.
        for &ino in &admitted {
            let file_path = crate::keys::inode_path(ino);
            match self.dlm.get_connection().await {
                Ok(mut con) => {
                    if let Err(e) = self.router.delete_file(&file_path, &mut con).await {
                        debug!(
                            "RECLAIM: delete_file({}) failed (proceeding to destroy): {:?}",
                            ino, e
                        );
                    }
                }
                Err(e) => {
                    debug!("RECLAIM: no DLM connection for delete_file({ino}): {e:?}");
                }
            }
        }

        self.destroy_batch_bisect(backend, &admitted).await;
    }

    /// Destroy `inos` as one batch; on commit failure bisect and retry the
    /// halves, terminating at size-1 sub-batches whose behavior is
    /// byte-for-byte today's per-ino path (§4.5: one persistently bad
    /// sector must wedge only its own ino, never 63 innocents). The per-ino
    /// teardown runs on BOTH edges — only `free()` (inside
    /// `destroy_inodes`) is withheld on failure.
    async fn destroy_batch_bisect(
        &self,
        backend: &std::sync::Arc<crate::meta_backend::RoutedMetaBackend>,
        inos: &[u64],
    ) {
        if inos.is_empty() {
            return;
        }
        match backend.destroy_inodes(inos).await {
            Ok(()) => {
                for &ino in inos {
                    self.reclaim_teardown(ino).await;
                }
            }
            Err(e) if inos.len() == 1 => {
                // Today's per-ino path discarded this error silently; the
                // batched path logs it (a strict logging improvement) and
                // still runs the teardown — leaving leases to TTL expiry and
                // stale cache entries would diverge from today's behavior.
                warn!(
                    "RECLAIM: destroy failed for ino {} (slot retained, free() withheld): {:?}",
                    inos[0], e
                );
                self.reclaim_teardown(inos[0]).await;
            }
            Err(_) => {
                let mid = inos.len() / 2;
                Box::pin(self.destroy_batch_bisect(backend, &inos[..mid])).await;
                Box::pin(self.destroy_batch_bisect(backend, &inos[mid..])).await;
            }
        }
    }

    /// Per-ino post-destroy teardown, exactly today's tail: lease release,
    /// POSIX-lock cleanup, metadata/attr cache invalidation (+ the W1
    /// stream-adjacency word, so the map's growth is bounded by live inos).
    async fn reclaim_teardown(&self, ino: u64) {
        // The single-drive guard clears only here — after the destroy
        // committed (a failed destroy leaves it set: leak-safe, see the
        // field doc).
        self.reclaim_inflight.remove_sync(&ino);
        if let Some((_, lease)) = self.active_leases.remove(&ino) {
            let _ = lease.release().await;
        }
        self.router.metadata_cache.remove(&ino);
        self.attr_cache.invalidate(&ino);
        self.last_write_end.remove_sync(&ino);
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GdsReadArgs {
    pub vram_address: u64,
    pub offset: u64,
    pub size: u64,
}

pub const SQUEEZEFS_IOC_GDS_READ: u32 = 0x80186601;

// Implement fuse3 Raw Filesystem interface
impl SqueezefsFilesystem {
    /// The single-run dismount teardown body (extracted from `destroy`,
    /// VL8 item 4): flush staged/memory data, deregister the `client:{id}`
    /// heartbeat record, and shut each meta volume down (which deletes its
    /// `writer_claim` and releases the D0 guard). Runs exactly once per
    /// unmount on a cancellation-immune spawned task.
    async fn run_dismount_teardown(&self) {
        info!("FUSE Daemon: Destroying mount. Force flushing all staged and memory data...");

        // Phase 1 & 2: Force flush memory buffers to staging, then staged blocks to NVMe-oF backend
        let _ = self.force_flush_all_staged_data().await;

        // Gracefully wait up to self.dismount_wait seconds for background workers to drain staged writes and active writes to NVMe-oF backend
        let start_wait = std::time::Instant::now();
        let max_wait = std::time::Duration::from_secs(self.dismount_wait);
        loop {
            let n = self
                .router
                .cache
                .nvme
                .staged_writes_in_flight
                .load(Ordering::Acquire);
            if n == 0 || start_wait.elapsed() >= max_wait {
                break;
            }
            let remaining = max_wait.saturating_sub(start_wait.elapsed());
            if remaining.is_zero() {
                break;
            }
            let notify = self.router.cache.nvme.staged_drained_notify.clone();
            let _ = tokio::time::timeout(remaining, notify.notified()).await;
        }

        // Async block-reclaim conservation (contract 3,
        // tests/async_block_reclaim_tests.rs): a clean unmount returns
        // every queued device range before declaring the dismount clean —
        // nothing is lost on clean unmount. Runs AFTER the flush/drain
        // waits above (they are what produce the final displaced-block
        // frees).
        self.router.backend_router.reclaim_drain().await;

        // Gather final count for warnings/statistics
        let keys = self.router.cache.nvme.list_staged_files();
        let mut staged_count = 0;
        let mut active_writes_count = 0;
        for key in keys {
            if key.starts_with("active_block:") {
                active_writes_count += 1;
            } else {
                staged_count += 1;
            }
        }

        if staged_count > 0 || active_writes_count > 0 {
            warn!(
                "WARNING: SqueezeFS dismounted with unflushed data! Remaining local staged files: {}, active write directories: {}. Other nodes may see inconsistent filesystem state until these are recovered or flushed.",
                staged_count, active_writes_count
            );
        } else {
            info!("FUSE Daemon: Dismount clean. All write staged blocks successfully flushed to backend.");
        }

        // Unregister this client on dismount
        let client_id_str = self.client_id.lock().unwrap().clone();
        if !client_id_str.is_empty() {
            if let Some(ref backend) = self.meta_backend {
                let attr_name = format!("client:{}", client_id_str);
                let _ = backend.removexattr(1, &attr_name).await;
            }
        }

        // Clean-unmount teardown (K6b): each volume runs a final
        // checkpoint and JOINS its checkpoint task (tail == head ⇒ empty
        // replay window; no leaked tasks — design §4.6 /
        // tests/dismount_teardown_tests.rs).
        if let Some(ref backend) = self.meta_backend {
            for vol in &backend.volumes {
                if let Err(e) = vol.shutdown().await {
                    warn!("Meta volume unmount teardown failed: {:?}", e);
                }
            }
        }
    }

    /// Wait until the dismount teardown (spawned by the winning `destroy`
    /// invocation) has completed. Returns immediately when no teardown has
    /// started. VL8 item 4: the daemon awaits this (bounded) before exit so
    /// heartbeat records are deregistered, not left to the staleness TTL.
    pub async fn wait_dismount_teardown(&self) {
        if !self.dismount_started() {
            return;
        }
        loop {
            // Arm the notification BEFORE re-checking the flag (the
            // standard Notify race-closure order).
            let notified = self.dismount_done.notified();
            if self.dismount_complete.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

impl Filesystem for SqueezefsFilesystem {
    type DirEntryStream<'a> = futures::stream::BoxStream<'a, FuseResult<DirectoryEntry>>;
    type DirEntryPlusStream<'a> = futures::stream::BoxStream<'a, FuseResult<DirectoryEntryPlus>>;

    async fn init(&self, _req: Request) -> FuseResult<ReplyInit> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        info!(
            "FUSE Daemon: Initialized Squeezefs Filesystem mount (version {}).",
            crate::version::version_line()
        );
        // D1.b: the per-daemon deadline watchdog replaces the retired
        // per-op `timeout()` wrappers (see the await-disposition audit at
        // the top of this module).
        spawn_op_watchdog();
        // Also print to stderr so operators can confirm which binary is live
        // without enabling full logging (PATH often pointed at a stale install).
        eprintln!(
            "squeezefs: mount ready (version {}, exe hint: rebuild target/release and reinstall)",
            crate::version::version_line()
        );

        // D2.a probe + D2.d atomic-open probe (measurement only — design
        // §5.2, PR M5): log what the kernel advertised at FUSE_INIT.
        // fuse3 publishes the negotiated init word before calling us.
        if let Some(ki) = fuse3::raw::kernel_init_info() {
            let noflush_capable = ki.major > 7 || (ki.major == 7 && ki.minor >= 35);
            // Every init capability bit named by this box's uapi
            // (include/uapi/linux/fuse.h through protocol 7.45) sits in
            // bits 0..=41; none is an atomic-open-class capability (the
            // patch lineage folding the pre-create LOOKUP into CREATE is
            // out of tree). Bits above the known set are surfaced so a
            // future kernel offering new capabilities is *seen*, not
            // silently ignored — that is the D2.d measurement.
            const KNOWN_INIT_BITS: u64 = (1u64 << 42) - 1;
            let unknown_bits = ki.flags & !KNOWN_INIT_BITS;
            let atomic_open_msg = if unknown_bits != 0 {
                format!(
                    "UNKNOWN init capability bits {unknown_bits:#x} advertised — \
                     investigate whether an atomic-open-class capability is among \
                     them (D2.d adoption follow-up)"
                )
            } else {
                "atomic-open-class capability: not advertised (no such bit through \
                 uapi 7.45; D2.d records absence)"
                    .to_string()
            };
            info!(
                "FUSE kernel protocol {}.{} (init flags {:#x}): FOPEN_NOFLUSH {} \
                 (D2.a; under writeback cache the elision switch is the clean-FLUSH \
                 ENOSYS latch — uapi: NOFLUSH applies 'unless FUSE_WRITEBACK_CACHE'); {}",
                ki.major,
                ki.minor,
                ki.flags,
                if noflush_capable {
                    "advertised + honored on non-writeback opens (kernel ≥ 7.35)"
                } else {
                    "IGNORED by this kernel (< 7.35) — the D1.d fast path + ENOSYS \
                     latch still apply"
                },
                atomic_open_msg
            );
            eprintln!(
                "squeezefs: FUSE kernel {}.{} — FOPEN_NOFLUSH {}; {}",
                ki.major,
                ki.minor,
                if noflush_capable {
                    "advertised (wb-cache elision = clean-FLUSH ENOSYS latch)"
                } else {
                    "ignored (<7.35)"
                },
                atomic_open_msg
            );
        } else {
            info!(
                "FUSE kernel INIT info not published by transport — capability \
                 probes (D2.a/D2.d) unavailable this session"
            );
        }

        if let Some(ref backend) = self.meta_backend {
            if let Ok(Some(val)) = backend.getxattr(1, "user.squeezefs.format_config").await {
                if let Ok(config) = serde_json::from_slice::<crate::FormatConfig>(&val) {
                    self.router.set_block_size(config.block_size);
                    let crypto_state = crate::crypto_compress::CryptoCompressState::new(
                        config.compression.clone(),
                        config.encrypt_algo.clone(),
                        config.encrypt_key.as_deref(),
                    );
                    // FIND-RW4-A geometry gate (forward-only): a transformed
                    // volume whose block_size leaves no chunk headroom for
                    // the worst-case stored image — every pre-fix
                    // compressed/encrypted format — cannot hold
                    // incompressible blocks. Refuse the mount LOUD; reformat
                    // is the remedy (new formats clamp block_size).
                    if let Err(msg) = crypto_state.transform_geometry_check(
                        config.block_size,
                        self.router.backend_router.default_allocator.chunk_size(),
                    ) {
                        error!("{msg}");
                        eprintln!("squeezefs: {msg}");
                        return Err(libc::EINVAL.into());
                    }
                    self.router.set_crypto(crypto_state);
                    let _ = self.inodes_limit.set(config.inodes);
                    let _ = self.capacity_limit.set(config.capacity);
                }
            }

            // Register this client as an active mount (heartbeat-timestamped so a
            // crashed client's entry expires instead of blocking format forever).
            self.refresh_client_registration().await;
        }

        // R5 (§5.7): register the RAM consumers with the joint memory
        // authority and start its 1 Hz sampler. Registration is guarded
        // (one registry per process) — remounts in-process must not
        // duplicate components.
        {
            use crate::mem_budget::{Component, MEM_BUDGET};
            use std::sync::Arc;
            static REGISTERED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !REGISTERED.swap(true, Ordering::Relaxed) {
                const MIB: u64 = 1024 * 1024;
                let bufs = self.active_block_buffers.clone();
                let bs_atomic = self.router.block_size.clone();
                // §5.7 Red parked shed: cap-halving at admission gates NEW
                // parks; the DRAIN flushes the existing backlog through
                // the durable path (the "early flush_memory_buffers_*"
                // mechanism — measured necessary: the row-5 trace grew
                // 250 → 1,750 parked buffers with the cap alone). Shed
                // closures are sync, the flush is async: the closure posts
                // the target and kicks the shared drain worker — the SAME
                // plumbing the Red admission gate uses
                // (`insert_active_block_buffer`), so the shed and the
                // gate never race two competing drains.
                self.ensure_parked_drain_worker();
                let shed_target = self.parked_drain_target.clone();
                let shed_kick = self.parked_drain_kick.clone();
                MEM_BUDGET.register(Component::new(
                    "parked_write_buffers",
                    32 * 4 * MIB, // 32 parked blocks at the default 4 MiB
                    4,
                    // W2: the RAII byte gauge (full-repr backings), exact
                    // even for checked-out buffers.
                    Arc::new(move || {
                        let _ = (&bufs, &bs_atomic);
                        METRICS.parked_full_buffer_bytes.load(Ordering::Relaxed)
                    }),
                    Arc::new(move |target| {
                        shed_target.fetch_min(target, Ordering::Relaxed);
                        shed_kick.notify_one();
                    }),
                ));
                // W2 (§5.2): extent-overlay payload slabs — a sheddable R5
                // component of their own; the shed is the same parked
                // drain, whose flush pass FOLDS extent overlays.
                let ext_shed_target = self.parked_drain_target.clone();
                let ext_shed_kick = self.parked_drain_kick.clone();
                MEM_BUDGET.register(Component::new(
                    "parked_extent_bytes",
                    0,
                    2,
                    Arc::new(|| METRICS.parked_extent_bytes.load(Ordering::Relaxed)),
                    Arc::new(move |target| {
                        ext_shed_target.fetch_min(target, Ordering::Relaxed);
                        ext_shed_kick.notify_one();
                    }),
                ));
                let hot = self.router.cache.hot_block.clone();
                let hot_shed = self.router.cache.hot_block.clone();
                MEM_BUDGET.register(Component::new(
                    "hot_block_tier",
                    64 * MIB,
                    4,
                    Arc::new(move || hot.current_bytes()),
                    Arc::new(move |target| hot_shed.shed_to(target)),
                ));
                let rl = self.router.cache.read_lru.clone();
                let rl_shed = self.router.cache.read_lru.clone();
                MEM_BUDGET.register(Component::new(
                    "read_lru",
                    32 * MIB,
                    2,
                    Arc::new(move || rl.current_bytes()),
                    Arc::new(move |target| rl_shed.shed_to(target)),
                ));
                let wl = self.router.cache.write_lru.clone();
                let wl_shed = self.router.cache.write_lru.clone();
                MEM_BUDGET.register(Component::new(
                    "write_lru",
                    32 * MIB,
                    2,
                    Arc::new(move || wl.current_bytes()),
                    Arc::new(move |target| wl_shed.shed_to(target)),
                ));
                let lanes = self.router.stream_lanes.clone();
                MEM_BUDGET.register(Component::new(
                    "prefetch_inflight",
                    0,
                    1,
                    Arc::new(|| METRICS.prefetch_inflight_bytes.load(Ordering::Relaxed)),
                    // Red: plans cleared — lane invalidation is the existing
                    // abandonment semantics (in-flight fills settle wasted).
                    Arc::new(move |_| lanes.invalidate_all()),
                ));
                // The two mmap tiers are attribution-only (§5.7): their
                // logical bytes sit over kernel-reclaimable page cache —
                // counting them as pressure pinned phantom Red through the
                // f1 rand passes (5 GiB of already-reclaimed tier bytes)
                // while the REAL kill-relevant residue (dirty/writeback)
                // is measured by the unreclaimable arm.
                let staging = self.router.cache.nvme.clone();
                MEM_BUDGET.register(
                    Component::new(
                        "staging_mmap",
                        0,
                        0, // weight 0: staging keeps its existing refusal behavior
                        Arc::new(move || staging.current_staged_write_bytes()),
                        Arc::new(|_| {}),
                    )
                    .kernel_reclaimable(),
                );
                let tier = self.router.cache.nvme.clone();
                MEM_BUDGET.register(
                    Component::new(
                        "read_tier_mmap",
                        0,
                        0, // mmap residency is kernel-owned; reclaim_extent is churn-driven (§5.7)
                        Arc::new(move || tier.current_read_cache_bytes()),
                        Arc::new(|_| {}),
                    )
                    .kernel_reclaimable(),
                );
                // RAM metadata caches (refill-from-backend caches — a Red
                // clear is always correctness-safe). Gauges are
                // conservative per-entry estimates: the VALUE here is the
                // shed (an aged daemon's metadata churn was the QUICK
                // cage class's live-set driver), not byte-exact billing.
                let meta_cache = self.router.metadata_cache.clone();
                let meta_cache_shed = self.router.metadata_cache.clone();
                MEM_BUDGET.register(Component::new(
                    "metadata_cache",
                    0,
                    1,
                    Arc::new(move || meta_cache.entry_count() * 1024),
                    Arc::new(move |_| meta_cache_shed.invalidate_all()),
                ));
                // KV metadata node cache (follow-up C defense-in-depth):
                // gauge at the budget-accounting basis (nodes × node
                // size); the shed is a checkpoint KICK — an early run of
                // the exact drain the cadence performs anyway (never-lossy
                // by construction; dirty state is flushed, never dropped).
                if let Some(ref routed) = self.meta_backend {
                    let vols = routed.volumes.clone();
                    let vols_shed = routed.volumes.clone();
                    MEM_BUDGET.register(Component::new(
                        "kv_node_cache",
                        64 * MIB,
                        1,
                        Arc::new(move || vols.iter().map(|v| v.node_cache_gauge().0).sum()),
                        Arc::new(move |_| {
                            for v in &vols_shed {
                                v.kick_checkpoint();
                            }
                        }),
                    ));
                }
                // L1: the FUSE-over-io_uring payload arenas — registered,
                // session-lifetime buffers the kernel copies payloads
                // through. Sized AT MOUNT by `transport_buffer_cap`
                // (budget/8, ≤ 2 GiB) and never resized, so they cannot
                // shed: weight 0 + no-op shed = attribution only, but the
                // bytes are anon (pressure-carrying), unlike the mmap
                // tiers. Gauge reads 0 until the session arms.
                #[cfg(target_os = "linux")]
                MEM_BUDGET.register(Component::new(
                    "transport_payload_buffers",
                    0,
                    0,
                    Arc::new(|| fuse3::over_uring_geometry().3),
                    Arc::new(|_| {}),
                ));
                MEM_BUDGET.register(Component::new(
                    "buffer_pool",
                    16 * 4 * MIB,
                    2,
                    Arc::new(|| crate::cache::pool::BUFFER_POOL.allocated_bytes()),
                    Arc::new(|target| crate::cache::pool::BUFFER_POOL.trim_to(target)),
                ));
                MEM_BUDGET.register(Component::new(
                    "aligned_buf_pool",
                    16 * 4 * MIB,
                    2,
                    Arc::new(|| crate::cache::pool::ALIGNED_BUF_POOL.allocated_bytes()),
                    Arc::new(|target| crate::cache::pool::ALIGNED_BUF_POOL.trim_to(target)),
                ));
                MEM_BUDGET.register(Component::new(
                    "ranged_buf_pool",
                    4 * MIB,
                    2,
                    Arc::new(|| crate::cache::pool::RANGED_BUF_POOL.allocated_bytes()),
                    Arc::new(|target| crate::cache::pool::RANGED_BUF_POOL.trim_to(target)),
                ));
            }
            crate::mem_budget::spawn_sampler();
        }

        // Start background active writes flusher task
        {
            let mut rx_guard = self.writeback_rx.lock().unwrap();
            if let Some(writeback_rx) = rx_guard.take() {
                let router = self.router.clone();
                let dlm = self.dlm.clone();
                let active_inode_locks = self.active_inode_locks.clone();
                let max_uploads = self.max_background_uploads;
                let requeue_tx = self.writeback_tx.clone();
                tokio::spawn(async move {
                    run_constant_writeback_worker(
                        writeback_rx,
                        requeue_tx,
                        router,
                        dlm,
                        active_inode_locks,
                        max_uploads,
                    )
                    .await;
                });
            }
        }

        // W2 (§5.2): mount-time extent-record sweep — validate + loudly
        // report kill-9 residue (clean shutdowns drain every record), and
        // arm the background fold worker.
        self.recover_extent_records().await;
        self.ensure_fold_worker();

        // Start background GC/reclaim worker pool
        let mut reclaim_rx_guard = self.reclaim_rx.lock().unwrap();
        if let Some(reclaim_rx) = reclaim_rx_guard.take() {
            let self_clone = self.clone();
            let reclaim_concurrency = self.reclaim_semaphore.available_permits();
            tokio::spawn(async move {
                run_reclaim_worker_pool(reclaim_rx, self_clone, reclaim_concurrency).await;
            });
        }

        Ok(ReplyInit {
            max_write: std::num::NonZeroU32::new(1048576).unwrap(), // 1MB absolute maximum write buffer size
        })
    }

    async fn destroy(&self, _req: Request) {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        // FUSE-over-io_uring: every queue loop observes connection teardown
        // and calls destroy — only the first runs the teardown (the repeats
        // used to re-attempt thousands of orphan flushes, starve the uring
        // worker into failing health probes, and spam ~3k ERROR lines).
        if self.dismount_once.swap(true, Ordering::SeqCst) {
            debug!("FUSE Daemon: duplicate destroy (per-queue) ignored");
            return;
        }
        // VL8 item 4: the winning destroy invocation may live on a session
        // task that dies mid-teardown on external unmount (its future is
        // dropped by the reply-task select, a detached queue worker's
        // shutdown, or daemon exit) — which stranded `client:`/`writer_claim`
        // heartbeat records until the 45 s staleness TTL. Run the real
        // teardown on a spawned task that survives that cancellation and
        // signal completion for `wait_dismount_teardown`. NOTE: no await
        // point between the `dismount_once` claim above and this spawn —
        // the claim can never be taken without the teardown being scheduled.
        let this = self.clone();
        let teardown = tokio::spawn(async move {
            this.run_dismount_teardown().await;
            this.dismount_complete.store(true, Ordering::Release);
            this.dismount_done.notify_waiters();
        });
        // Normal (non-cancelled) flow still completes the teardown before
        // replying to DESTROY; if THIS await is dropped, the task runs on.
        let _ = teardown.await;
    }
    async fn lookup(&self, _req: Request, parent: u64, name: &OsStr) -> FuseResult<ReplyEntry> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        crate::coz_progress!("fuse_lookup");
        check_component_name_len(name)?;
        let name_str = osstr_to_cow(name);
        debug!("FUSE Lookup: parent = {}, name = {}", parent, name_str);

        // FUSE_EXPORT_SUPPORT contract (fstests generic/426; pinned in
        // tests/attr_refresh_tests.rs): the kernel revives an evicted
        // nodeid — the open_by_handle_at decode path — with
        // `LOOKUP(nodeid, ".")`, and reconnects directory handles with
        // `LOOKUP(nodeid, "..")`. "." is the nodeid itself; ".." at the
        // root is the root (elsewhere it falls through to the backend,
        // which resolves stored parent links where they exist and stays
        // loud where they do not — never a fabricated parent).
        if name_str == "." || (parent == 1 && name_str == "..") {
            let target = if name_str == "." { parent } else { 1 };
            let attr = self
                .get_attr_internal(target)
                .await
                .map_err(map_squeezefs_err)?;
            return Ok(ReplyEntry {
                ttl: self.entry_ttl_for(attr.kind),
                attr,
                generation: 1,
            });
        }

        if parent == 1 && name_str == ".config" {
            let config_data = self.generate_config_json().await;
            let bytes = config_data.into_bytes();
            let size = bytes.len() as u64;
            self.latest_config_json
                .store(std::sync::Arc::new(Some(std::sync::Arc::new(bytes))));
            self.latest_config_size.store(size, Ordering::Release);
            let attr = self.get_config_attr(size);
            return Ok(ReplyEntry {
                // Zero TTL (like `.stats`): with exact-size payloads
                // (no floor padding) every fstat must reach the daemon
                // so the kernel's copy bound is never a stale size.
                ttl: Duration::from_secs(0),
                attr,
                generation: 1,
            });
        }

        if parent == 1 && name_str == ".stats" {
            let stats_data = self.generate_stats_json().await;
            let bytes = stats_data.into_bytes();
            let size = bytes.len() as u64;
            self.latest_stats_json
                .store(std::sync::Arc::new(Some(std::sync::Arc::new(bytes))));
            self.latest_stats_size.store(size, Ordering::Release);
            let attr = self.get_stats_attr(size);
            return Ok(ReplyEntry {
                ttl: Duration::from_secs(0), // dynamic stats shouldn't be cached long
                attr,
                generation: 1,
            });
        }

        let prof = OpProf::begin(FuseOpKind::Lookup, parent);
        let lookup_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .ok_or_else(|| {
                    SqueezefsError::InvalidOperation("Metadata backend not initialized".to_string())
                })
                .map_err(map_squeezefs_err)?;
            prof.mark_backend_start();
            let backend_res = backend.lookup(parent, &name_str).await;
            prof.mark_backend_done();
            let inode = backend_res.map_err(map_squeezefs_err)?;
            let attr = self.inode_to_file_attr(&inode);
            self.attr_cache
                .insert(inode.ino, (attr, std::time::Instant::now()));
            Ok(ReplyEntry {
                ttl: self.entry_ttl_for(attr.kind),
                attr,
                generation: 1,
            })
        };

        // D2.b (§5.2, PR M5): a miss becomes a CACHEABLE negative entry
        // (nodeid 0 + negative TTL) instead of a bare ENOENT, so the
        // kernel's dcache absorbs repeated-miss round trips (PATH walks,
        // stat retries, rename-dest probes of recurring names). Honest
        // scope, per the design: unique-name create storms look up each
        // name once — this moves the trailing-op mix and real-workload
        // miss traffic, not the mdstorm create row. `negative_timeout=0`
        // restores the bare-errno reply. Only the ENOENT class converts;
        // every other error keeps its shape.
        let res = match lookup_future.await {
            Err(errno)
                if errno == Errno::from(libc::ENOENT) && !self.kernel_ttls.negative.is_zero() =>
            {
                METRICS
                    .fuse_lookup_negative_replies
                    .fetch_add(1, Ordering::Relaxed);
                Ok(ReplyEntry::negative(self.kernel_ttls.negative))
            }
            other => other,
        };
        // Under-`i_rwsem` estimator: this LOOKUP (hit or ENOENT probe) may
        // be the one the kernel holds the parent lock across into CREATE.
        prof.note_lookup_arrival(parent, &name_str);
        res
    }

    async fn getattr(
        &self,
        _req: Request,
        ino: u64,
        _fh: Option<u64>,
        _flags: u32,
    ) -> FuseResult<ReplyAttr> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE GetAttr: ino = {}", ino);

        // Virtual inodes: GETATTR reports the PUBLISHED generation's size
        // and must never regenerate-and-republish — the kernel copies
        // exactly `i_size` bytes out of these files (`cat` →
        // `copy_file_range`), so an fstat between open and read that
        // republished a *different* size than the open-pinned generation
        // tore every snapshot read under counter churn (the M2 acceptance
        // session measured 9/10 torn phase snapshots with the rig's ~40 KB
        // payload). First touch (never generated) generates once and
        // publishes, so a bare `stat` keeps working. Pinned by
        // `metrics_tests::stats_snapshot_getattr_size_matches_served_bytes_under_churn`.
        if ino == CONFIG_INODE {
            let published = self.latest_config_size.load(Ordering::Acquire);
            let size = if published > 0 {
                published
            } else {
                let bytes = self.generate_config_json().await.into_bytes();
                let size = bytes.len() as u64;
                self.latest_config_json
                    .store(std::sync::Arc::new(Some(std::sync::Arc::new(bytes))));
                self.latest_config_size.store(size, Ordering::Release);
                size
            };
            let attr = self.get_config_attr(size);
            return Ok(ReplyAttr {
                // Zero TTL (like `.stats`): exact-size payloads need
                // every fstat served fresh from the published pin.
                ttl: Duration::from_secs(0),
                attr,
            });
        }

        if ino == STATS_INODE {
            let published = self.latest_stats_size.load(Ordering::Acquire);
            let size = if published > 0 {
                published
            } else {
                let bytes = self.generate_stats_json().await.into_bytes();
                let size = bytes.len() as u64;
                self.latest_stats_json
                    .store(std::sync::Arc::new(Some(std::sync::Arc::new(bytes))));
                self.latest_stats_size.store(size, Ordering::Release);
                size
            };
            let attr = self.get_stats_attr(size);
            return Ok(ReplyAttr {
                ttl: Duration::from_secs(0),
                attr,
            });
        }

        let prof = OpProf::begin(FuseOpKind::Getattr, ino);
        let getattr_future = async {
            prof.mark_backend_start();
            let attr_res = self.get_attr_internal(ino).await;
            prof.mark_backend_done();
            let attr = attr_res.map_err(map_squeezefs_err)?;

            Ok(ReplyAttr {
                ttl: self.kernel_ttls.attr,
                attr,
            })
        };

        getattr_future.await
    }

    async fn mknod(
        &self,
        req: Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        rdev: u32,
    ) -> FuseResult<ReplyEntry> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        check_component_name_len(name)?;
        let name_str = osstr_to_cow(name);
        debug!(
            "FUSE mknod: parent = {}, name = {}, mode = {:o}, rdev = {}",
            parent, name_str, mode, rdev
        );

        // Persist rdev only for the node kinds it means something on
        // (char/block devices — the kernel's 32-bit new_encode_dev word);
        // POSIX says mknod ignores dev for FIFOs/sockets/regular files.
        let stored_rdev = match mode & libc::S_IFMT {
            libc::S_IFCHR | libc::S_IFBLK => rdev,
            _ => 0,
        };
        let prof = OpProf::begin(FuseOpKind::Mknod, parent);
        let mknod_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .expect("meta_backend must be configured");
            prof.mark_backend_start();
            let backend_res = backend
                .create_with_rdev(parent, &name_str, mode, req.uid, req.gid, stored_rdev)
                .await;
            prof.mark_backend_done();
            let inode = backend_res.map_err(map_squeezefs_err)?;
            let attr = self.inode_to_file_attr(&inode);
            self.attr_cache
                .insert(inode.ino, (attr, std::time::Instant::now()));
            self.bump_dir_generation(parent);
            self.attr_cache.invalidate(&parent);
            Ok(ReplyEntry {
                ttl: self.entry_ttl_for(attr.kind),
                attr,
                generation: 1,
            })
        };

        mknod_future.await
    }

    async fn create(
        &self,
        req: Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        flags: u32,
    ) -> FuseResult<ReplyCreated> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        crate::coz_progress!("fuse_create");
        check_component_name_len(name)?;
        let name_str = osstr_to_cow(name);
        debug!(
            "FUSE Create: parent = {}, name = {}, mode = {:o}, flags = {}",
            parent, name_str, mode, flags
        );

        let prof = OpProf::begin(FuseOpKind::Create, parent);
        let create_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .expect("meta_backend must be configured");
            prof.mark_backend_start();
            let backend_res = backend
                .create(parent, &name_str, mode, req.uid, req.gid)
                .await;
            prof.mark_backend_done();
            let inode = backend_res.map_err(map_squeezefs_err)?;
            let attr = self.inode_to_file_attr(&inode);
            self.attr_cache
                .insert(inode.ino, (attr, std::time::Instant::now()));
            // Seed layout cache so the first write skips a cold meta
            // backend fetch. Ino-keyed (D1.c): the pre-M4 shape allocated
            // an `inode_{ino}` String per create just to key this insert.
            self.router.metadata_cache.insert(
                inode.ino,
                crate::routing::CachedMetadata {
                    file_type: "inline".to_string(),
                    size: 0,
                    block_map_id: None,
                    block_prefix: None,
                    file_id: None,
                    cached_at: std::time::Instant::now(),
                    data_key: None,
                    block_map: None,
                    layout_dirty: false,
                },
            );
            self.bump_dir_generation(parent);
            // D2.c: refresh (not invalidate, and never keep-stale) — the
            // backend just bumped the parent's mtime/ctime, and the kernel
            // re-GETATTRs the parent on its next path walk
            // (fuse_dir_changed). Keeping the pre-create cache entry served
            // pre-bump parent times for a full attr-TTL window (pjdfstest
            // open/00.t 33-34, found by the VL10 release gate; pinned in
            // tests/attr_refresh_tests.rs).
            self.refresh_attr_cache(parent).await;
            self.add_open(inode.ino);
            Ok(ReplyCreated {
                ttl: self.entry_ttl_for(attr.kind),
                attr,
                generation: 1,
                fh: inode.ino,
                flags: regular_open_reply_flags(),
            })
        };

        let res = create_future.await;
        // Under-`i_rwsem` estimator: pair this CREATE's reply with its
        // preceding LOOKUP of the same (parent, name) —
        // `fuse_create_under_lock_ns` (design §5.1 artifact 1).
        prof.pair_create_reply(parent, &name_str);
        res
    }

    async fn open(&self, req: Request, inode: Inode, flags: u32) -> FuseResult<ReplyOpen> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        // VL8 item 2: register (the live wedge held 4 opens invisibly).
        let _prof = OpProf::begin(FuseOpKind::Open, inode);
        debug!("FUSE Open: inode = {}, flags = {:#o}", inode, flags);

        if inode == STATS_INODE || inode == CONFIG_INODE {
            let content = if inode == STATS_INODE {
                let old_val = self.latest_stats_json.swap(std::sync::Arc::new(None));
                if let Some(bytes_arc) = &*old_val {
                    (**bytes_arc).clone()
                } else {
                    self.generate_stats_json().await.into_bytes()
                }
            } else {
                let old_val = self.latest_config_json.swap(std::sync::Arc::new(None));
                if let Some(bytes_arc) = &*old_val {
                    (**bytes_arc).clone()
                } else {
                    self.generate_config_json().await.into_bytes()
                }
            };
            // PIN point: this open's fh serves exactly `content` — publish
            // its size so a subsequent fstat (GETATTR, which never
            // regenerates) reports the bound the kernel will copy to.
            // Single-reader snapshots are exact by construction; concurrent
            // readers race last-open-wins (bounded, documented residual).
            let pinned_size = content.len() as u64;
            if inode == STATS_INODE {
                self.latest_stats_size.store(pinned_size, Ordering::Release);
            } else {
                self.latest_config_size
                    .store(pinned_size, Ordering::Release);
            }
            let fh = self.next_virtual_fh.fetch_add(1, Ordering::Relaxed);
            debug!("FUSE Open virtual: ino = {inode}, fh = {fh}, pinned {pinned_size} bytes");
            self.open_virtual_files.insert(fh, content);
            // FOPEN_DIRECT_IO: the payload is regenerated per open, but the
            // kernel clamps buffered reads to i_size from a PREVIOUS
            // generation's lookup — serving truncated (unparseable) JSON
            // once the stats payload grows between generations. Direct I/O
            // makes the kernel trust our read replies (short read = EOF)
            // instead of the stale size.
            const FOPEN_DIRECT_IO: u32 = 1 << 0;
            return Ok(ReplyOpen {
                fh,
                flags: FOPEN_DIRECT_IO,
            });
        }

        // O_TRUNC (VL8 catalog item 1 — the generic/074 fstest.4 stale-unit
        // corruption): fuse3 negotiates FUSE_ATOMIC_O_TRUNC, so the kernel
        // NEVER sends the SETATTR(size=0) fallback for open(O_TRUNC) — it
        // truncates its own page cache/i_size and trusts THIS handler to
        // truncate daemon state. Ignoring the flag left the previous
        // generation's entire state alive (size authority, block map,
        // staged ring images, parked overlays, staged extent records):
        // mmap store faults and RMW seeds then read pre-truncate bytes,
        // and the un-truncated durable size can even resurrect after the
        // kernel attr TTL. Route through the setattr size path — same
        // guard order, same overlay/record prune, same backend commit as
        // an explicit truncate-to-zero.
        if flags & (libc::O_TRUNC as u32) != 0 {
            self.setattr(
                req,
                inode,
                None,
                SetAttr {
                    size: Some(0),
                    ..Default::default()
                },
            )
            .await?;
        }

        // OPEN/RECLAIM HANDSHAKE (fstests generic/795): count the open
        // FIRST, then refuse if a reclaim already claimed the destroy slot.
        // Mirrored order on the reclaim side (claim slot, then re-check the
        // open count) makes every interleaving safe: either the reclaim
        // backs out on our count, or we see its claim here and fail ENOENT
        // (the open lost the race to rm — the file is unlinked with no
        // surviving opens). Without this, an OPEN landing after admission
        // was granted a handle onto the inode being destroyed.
        self.add_open(inode);
        if self.reclaim_inflight.contains_sync(&inode) {
            self.remove_open(inode);
            return Err(Errno::from(libc::ENOENT));
        }
        // File handle is just the inode number for simplicity in this design
        Ok(ReplyOpen {
            fh: inode,
            flags: regular_open_reply_flags(),
        })
    }

    async fn opendir(&self, _req: Request, inode: Inode, _flags: u32) -> FuseResult<ReplyOpen> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Opendir: inode = {}", inode);

        self.add_open(inode);

        // PR K7 (§5.1): directories stream by key cookie — a per-fh
        // whole-directory snapshot is both wasted work and an OOM hazard
        // at 1 M entries. The fh is still minted so releasedir
        // bookkeeping stays uniform.
        let fh = self.next_dir_fh.fetch_add(1, Ordering::Relaxed);
        Ok(ReplyOpen { fh, flags: 0 })
    }

    async fn releasedir(&self, _req: Request, ino: u64, fh: u64, _flags: u32) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Releasedir: ino = {}, fh = {}", ino, fh);

        self.remove_open(ino);
        // Do not reclaim on releasedir: the directory is typically still linked.
        // Destruction runs from forget (and release of unlinked files) only, and
        // only after nlink==0 — avoids racing live parents under concurrent mkdir.
        Ok(())
    }

    async fn read(
        &self,
        _req: Request,
        ino: u64,
        fh: u64,
        offset: u64,
        size: u32,
        flags: u32,
    ) -> FuseResult<ReplyData> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        crate::coz_progress!("fuse_read");
        debug!(
            "FUSE Read: ino = {}, fh = {}, offset = {}, size = {}, flags = {:#x}",
            ino, fh, offset, size, flags
        );
        // R1b classifier inputs (§5.3) + hybrid I/O (user directive
        // 2026-07-15): the kernel sends the file's open flags on every
        // READ; O_DIRECT is counted here and handed to the routing layer
        // as the per-request `ReadClassHint` — under the DEFAULT hybrid
        // policy it only labels observability (O_DIRECT serves/admits
        // exactly like buffered), and combined with the mount-scoped
        // `direct_device_true` escape it selects the strictly device-true
        // diagnostic path.
        let odirect = flags & (libc::O_DIRECT as u32) != 0;
        if odirect {
            METRICS
                .read_odirect_requests
                .fetch_add(1, Ordering::Relaxed);
        }
        let read_hint = crate::routing::ReadClassHint { odirect };

        if ino == CONFIG_INODE {
            let bytes = if let Some(cached) = self.open_virtual_files.get(&fh) {
                cached.clone()
            } else {
                self.generate_config_json().await.into_bytes()
            };
            if offset >= bytes.len() as u64 {
                return Ok(ReplyData {
                    data: Vec::new().into(),
                    backing: None,
                });
            }
            let start = offset as usize;
            let end = std::cmp::min(bytes.len(), start + size as usize);
            // SAFETY: start < bytes.len() checked on line 2144, and end is clamped to bytes.len()
            let slice = unsafe { bytes.get_unchecked(start..end) };
            return Ok(ReplyData {
                data: slice.to_vec().into(),
                backing: None,
            });
        }

        if ino == STATS_INODE {
            let cached_hit = self.open_virtual_files.get(&fh);
            debug!(
                "FUSE Read virtual: ino = {ino}, fh = {fh}, hit = {}, len = {:?}",
                cached_hit.is_some(),
                cached_hit.as_ref().map(|c| c.len())
            );
            let bytes = if let Some(cached) = cached_hit {
                cached.clone()
            } else {
                self.generate_stats_json().await.into_bytes()
            };
            if offset >= bytes.len() as u64 {
                return Ok(ReplyData {
                    data: Vec::new().into(),
                    backing: None,
                });
            }
            let start = offset as usize;
            let end = std::cmp::min(bytes.len(), start + size as usize);
            // SAFETY: start < bytes.len() checked on line 2166, and end is clamped to bytes.len()
            let slice = unsafe { bytes.get_unchecked(start..end) };
            return Ok(ReplyData {
                data: slice.to_vec().into(),
                backing: None,
            });
        }

        let prof = OpProf::begin(FuseOpKind::Read, ino);
        let file_path = crate::keys::inode_path(ino);
        let lock = self.get_inode_lock_ref(ino);

        // Short critical section only: size bound + active-buffer hit.
        // Must NOT hold the inode read lock across flush or backend I/O —
        // flush_single_active_block upgrades to write() and tokio RwLock is
        // not re-entrant (self-deadlock under multi-block / active-block reads).
        let (file_size, active_hit, guard_meta) = {
            let _guard = lock.read().await;

            let mut file_size = if let Some((attr, _)) = self.attr_cache.get(&ino) {
                attr.size
            } else {
                let backend = self
                    .meta_backend
                    .as_ref()
                    .ok_or_else(|| {
                        SqueezefsError::InvalidOperation(
                            "Metadata backend not initialized".to_string(),
                        )
                    })
                    .map_err(map_squeezefs_err)?;
                // Fail LOUD on a dead/erroring ino — the old `.unwrap_or(0)`
                // fabricated size 0, and the kernel zero-extends a short
                // read up to its cached i_size (fuse_short_read): a
                // destroyed-under-fd or transiently-erroring inode read as
                // silent full-length zeros instead of an error
                // (generic/795).
                backend
                    .getattr(ino)
                    .await
                    .map(|inode| inode.size)
                    .map_err(map_squeezefs_err)?
            };

            // Size coherency: prefer the router metadata cache, which the write
            // path updates synchronously. The durable inode/attr caches can lag
            // a just-committed write until its deferred flush, so without this a
            // read racing a write (kernel readahead under the writeback cache)
            // would observe a stale size (0 on a fresh file), return a short
            // read, and let the kernel cache zero pages — silent read-after-
            // write corruption.
            // Captured once and reused below (P2 per-op economy): the
            // pre-read custody fingerprint and the router descent's first
            // attempt both build from this snapshot instead of re-probing
            // the cache — an older-than-instant snapshot is CONSERVATIVE
            // for the fingerprint (movement since capture shows up as a
            // post-side mismatch → one bounded retry; epoch monotonicity
            // forbids ABA), and the router re-validates freshness before
            // trusting the hint.
            let guard_meta = self.router.metadata_cache.get(&ino);
            if let Some(m) = &guard_meta {
                file_size = m.size;
            }

            if offset >= file_size {
                return Ok(ReplyData {
                    data: Vec::new().into(),
                    backing: None,
                });
            }

            let read_len = std::cmp::min(size as u64, file_size - offset) as usize;
            let block_size = self.router.block_size.load(Ordering::Relaxed);
            let start_block = offset / block_size;
            let end_block = (offset + read_len as u64 - 1) / block_size;

            if start_block == end_block {
                let cache_key = crate::keys::active_block(ino, start_block).to_string();
                if let Some(buf) = self.active_block_buffers.get(&cache_key) {
                    let block_start = start_block * block_size;
                    let rel_offset = (offset - block_start) as usize;
                    let rel_end = rel_offset + read_len;
                    // W2 extent overlay (§5.2 — "overlay never invisible"):
                    // serve covered ∩ range from the parked slabs and the
                    // complement from the base tiers/ranged read (zeros for
                    // a hole-backed overlay) — never materialize a
                    // block-size image on the read path. Runs + deferral
                    // are captured under the entry guard; the guard drops
                    // before the base read (a racing fold publishes the
                    // SAME bytes into the base — idempotent overlay).
                    if buf.value().is_extent_repr() {
                        let deferred = buf.value().seed_deferred();
                        let covered = buf.value().covered_contains(rel_offset, rel_end);
                        let runs = buf.value().extent_runs_in(rel_offset, rel_end);
                        drop(buf);
                        let mut out = vec![0u8; read_len];
                        if deferred && !covered {
                            let (base, _backing) = self
                                .router
                                .read_file_range_zero_copy(
                                    &file_path,
                                    offset,
                                    read_len as u32,
                                    None,
                                    read_hint,
                                )
                                .await
                                .map_err(map_squeezefs_err)?;
                            let n = base.len().min(read_len);
                            out[..n].copy_from_slice(&base[..n]);
                        }
                        for (s, d) in runs {
                            let lo = s - rel_offset;
                            out[lo..lo + d.len()].copy_from_slice(&d);
                        }
                        return Ok(ReplyData {
                            data: bytes::Bytes::from(out),
                            backing: None,
                        });
                    }
                    // Zero-copy CoW-stable snapshot: immutable for the
                    // reply's whole lifetime — a later write to this block
                    // copies instead of mutating these bytes (P0 fix).
                    // Coverage-aware (§5.3 + RW3b multi-run): snapshot +
                    // coverage queries are read under the same entry guard,
                    // so they are mutually consistent; memset elision means
                    // the gaps of a Fresh buffer hold recycled pool bytes
                    // that must NEVER be served through the kernel — a read
                    // inside the covered runs serves the zero-copy slice, a
                    // Fresh read overlapping a gap composes zeros + runs,
                    // and a deferred read overlapping a gap materializes
                    // (the gap owes OLD bytes, item B).
                    let contained = buf.value().covered_contains(rel_offset, rel_end);
                    let deferred = buf.value().seed_deferred();
                    if !contained && !deferred {
                        // Rare sparse read overlapping the gaps of a Fresh
                        // buffer: build the reply in a fresh buffer — zeros
                        // plus written runs ∩ range — WITHOUT mutating the
                        // shared buffer (zeroing in place here would be a
                        // mutation outside BLOCK_FLUSH_LOCKS). Runs +
                        // snapshot captured under the same guard.
                        let snapshot = buf.value().snapshot();
                        let runs = buf.value().covered_runs_in(rel_offset, rel_end);
                        drop(buf);
                        let mut out = vec![0u8; read_len];
                        for (s, e) in runs {
                            out[s - rel_offset..e - rel_offset].copy_from_slice(&snapshot[s..e]);
                        }
                        return Ok(ReplyData {
                            data: bytes::Bytes::from(out),
                            backing: None,
                        });
                    }
                    if contained {
                        // Common case (every content-valid entry and every
                        // sequential read): zero-copy slice.
                        let snapshot = buf.value().snapshot();
                        drop(buf);
                        return Ok(ReplyData {
                            data: snapshot.slice(rel_offset..rel_end),
                            backing: None,
                        });
                    }
                    drop(buf);
                    {
                        // Item B: the uncovered complement owes the OLD
                        // block's bytes (not zeros). Materialize under the
                        // block lock — the reader pays the read the writer
                        // deferred — then serve the merged content.
                        // OVERLAY NEVER INVISIBLE (the QUICK 075/112
                        // transient): the buffer stays PARKED across the
                        // fetch await — checking it out here made every
                        // concurrent single-block read (kernel readahead,
                        // AIO) fall to the backend and serve pre-merge
                        // bytes. The held block lock excludes mutators, so
                        // the fill applies synchronously via a short-lived
                        // map guard afterwards (never a guard across an
                        // await — the §5.5 executor-starvation wedge
                        // class). On a failed fetch the buffer is parked
                        // unchanged (never-lossy) and the read fails loud.
                        let _block_guard =
                            block_lock_acquire(ino, start_block as u32, BlockLockSite::ReadSeed)
                                .await;
                        let still_deferred = self
                            .active_block_buffers
                            .get(&cache_key)
                            .map(|e| e.value().seed_deferred());
                        match still_deferred {
                            Some(true) => {
                                let image = self
                                    .fetch_seed_image(&file_path, start_block as u32)
                                    .await
                                    .map_err(map_squeezefs_err)?;
                                if let Some(mut entry) =
                                    self.active_block_buffers.get_mut(&cache_key)
                                {
                                    entry
                                        .value_mut()
                                        .fill_complement_from(image.as_deref().unwrap_or(&[]));
                                    let snap = entry.value().snapshot();
                                    drop(entry);
                                    return Ok(ReplyData {
                                        data: snap.slice(rel_offset..rel_end),
                                        backing: None,
                                    });
                                }
                                // Vanished between fetch and fill — the lock
                                // forbids it; fall through to a backend read
                                // (now authoritative) rather than abort a
                                // read path.
                            }
                            Some(false) => {
                                // A racing flush/write materialized it first:
                                // serve the now content-valid snapshot.
                                if let Some(entry) = self.active_block_buffers.get(&cache_key) {
                                    let snap = entry.value().snapshot();
                                    drop(entry);
                                    return Ok(ReplyData {
                                        data: snap.slice(rel_offset..rel_end),
                                        backing: None,
                                    });
                                }
                            }
                            None => {}
                        }
                        // Buffer vanished (flushed meanwhile): fall through
                        // to the normal backend read below.
                    }
                }
                (file_size, false, guard_meta)
            } else {
                (file_size, true, guard_meta) // need flush of dirty active blocks first
            }
        };

        let read_len = std::cmp::min(size as u64, file_size - offset) as usize;

        let guard_meta = if active_hit {
            let fencing_token = self.dlm.get_fencing_token_ino(ino);
            let _ = self
                .flush_active_blocks_with_retry(ino, fencing_token)
                .await;
            // The flush may have published bindings / retired overlays:
            // re-snapshot so the pre-fingerprint and router hint see the
            // post-flush layout (multi-block windows only — the hot
            // single-block path never flushes here).
            self.router.metadata_cache.get(&ino)
        } else {
            guard_meta
        };

        let conn_guard = self.session_connection.load();
        let dest_addr = conn_guard
            .as_ref()
            .as_ref()
            .and_then(|conn| conn.get_payload_buffer(_req.unique))
            .map(|(ptr, _sz)| ptr);

        // OVERLAY NEVER INVISIBLE — the moving-custody read protocol
        // (fstests generic/795, VL10 release gate). A block's acked bytes
        // live in exactly one live authority at a time — RAM overlay →
        // staged sibling → durable binding — and every transfer publishes
        // its destination strictly BEFORE retiring its source (under the
        // block's flush lock). This read is deliberately lock-free, so a
        // transfer landing INSIDE the read window can invert the reader's
        // probe order: the router resolved the block map before the
        // publish, while the overlay/sibling probes ran after the retire —
        // acked bytes invisible to every tier this pass touched (zeros
        // served at the head of a just-completed block, the 795 cmp
        // signature). Three layered defenses, all load-bearing:
        //
        //  1. pre-captured overlay runs (before the router read) close the
        //     overlay-retired-during-read face;
        //  2. the post-read capture (strictly newer where both exist)
        //     closes the park-created-during-read face;
        //  3. the binding fingerprint below detects a block-map publish
        //     that landed mid-read — the one transfer the two captures
        //     cannot see (sibling/overlay → durable binding) — and re-runs
        //     the read; the fresh pass resolves the published map. Bounded:
        //     each retry needs another publish inside the ever-smaller
        //     window; exhaustion serves the last compose (a racing read may
        //     legally serve any value current within its window).
        let mut bindings_before = self
            .read_custody_fingerprint(guard_meta.as_ref(), &file_path, ino, offset, read_len)
            .await;
        // The router's first attempt reuses the handler snapshot (one moka
        // get + clone saved per read); retries re-resolve fresh — a retry
        // IS the movement signal.
        let mut meta_hint = guard_meta;
        let mut attempts = 0u32;
        let (data, backing) = loop {
            let pre_runs = self.capture_parked_runs(ino, offset, read_len);

            // Backend / cache read without holding the inode lock (readers
            // scale).
            let read_future = self.router.read_file_range_zero_copy_with_meta(
                &file_path,
                offset,
                read_len as u32,
                dest_addr,
                read_hint,
                meta_hint.take(),
            );
            prof.mark_backend_start();
            let read_res = read_future.await;
            prof.mark_backend_done();
            let (data, backing) = match read_res {
                Ok(res) => res,
                Err(e) => {
                    error!("FUSE Read error: {:?}", e);
                    return Err(map_squeezefs_err(e));
                }
            };

            let post_runs = self.capture_parked_runs(ino, offset, data.len());
            let data = Self::apply_parked_runs(offset, data, &pre_runs);
            let data = Self::apply_parked_runs(offset, data, &post_runs);

            let bindings_after = self
                .read_custody_fingerprint(None, &file_path, ino, offset, read_len)
                .await;
            if read_custody_fp_matches(&bindings_after, &bindings_before) || attempts >= 4 {
                break (data, backing);
            }
            attempts += 1;
            bindings_before = bindings_after;
        };
        Ok(ReplyData { data, backing })
    }

    async fn write(
        &self,
        _req: Request,
        ino: u64,
        _fh: u64,
        offset: u64,
        data: bytes::Bytes,
        _write_flags: u32,
        _flags: u32,
    ) -> FuseResult<ReplyWrite> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        crate::coz_progress!("fuse_write");

        if ino == CONFIG_INODE || ino == STATS_INODE {
            return Err(Errno::from(libc::EACCES));
        }

        // The representable maximum: block indices are u32 across the
        // striped layout, so files cap at block_size × (2^32 − 1). Beyond
        // it the pre-fix path silently wrapped the block index mod 2^32
        // and read back zeros (fstests generic/525) — refuse EFBIG loud
        // (pinned in tests/sparse_write_bounded_tests.rs).
        if offset.saturating_add(data.len() as u64) > self.max_file_size() {
            return Err(Errno::from(libc::EFBIG));
        }

        // D1.d: this open generation now has flushable state.
        self.mark_handle_dirty(ino);

        // Record writeback queue depth
        let queue_depth = self.writeback_queue_cap - self.writeback_tx.capacity();
        METRICS.writeback_queue_depth.record(queue_depth);

        // RW1: sample the concurrent WRITE-handler depth (rig-armed only) —
        // the §12 OQ2 instrument. RAII: released at handler return.
        let _write_inflight = WriteInflight::enter();

        let prof = OpProf::begin(FuseOpKind::Write, ino);
        let write_future = async {
            prof.mark_backend_start();
            // RW1 route-classify stamp: backend entry → striped dispatch
            // (recorded only on the striped branch below).
            let wp_route = write_phase_start();
            let lock = self.get_inode_lock_ref(ino);
            let start_wait = std::time::Instant::now();
            let guard = lock.write().await;
            METRICS.write_lock_wait.record(start_wait.elapsed());

            // VL8 item 9 — the syncfs transient-ENOENT vector: with the
            // writeback cache + clean-handle FLUSH elision the kernel can
            // flush dirty pages AFTER close+unlink+FORGET destroyed the
            // ino. Downstream, `fetch_metadata` would FABRICATE a fresh
            // inline layout (silently resurrecting the destroyed ino as a
            // zombie record) and deeper commit paths can surface ENOENT —
            // which lands in the superblock errseq and fails the NEXT
            // `syncfs` on a healthy mount (the VL7-rig leg-12 transient,
            // reproduced live 2026-07-21). Unlink IS the discard
            // authority for this data: when nothing local knows the ino
            // (no open handle — writes with a live handle never probe, so
            // the hot path pays nothing — no cached meta/attr) verify
            // against the backend; a verified-NotFound ino gets a COUNTED
            // discard ack (`writeback_orphan_discards`, the FIND-M11-A
            // remount-law analogue), never a resurrect, never an errno.
            if !self.is_open(ino)
                && self.router.metadata_cache.get(&ino).is_none()
                && self.attr_cache.get(&ino).is_none()
            {
                if let Some(backend) = self.meta_backend.as_ref() {
                    if matches!(
                        backend.getattr(ino).await,
                        Err(ref e) if e.to_errno() == libc::ENOENT
                    ) {
                        METRICS
                            .writeback_orphan_discards
                            .fetch_add(1, Ordering::Relaxed);
                        debug!(
                            "FUSE Write: ino {ino} is verified-reclaimed — \
                             counted writeback-orphan discard ({} bytes)",
                            data.len()
                        );
                        drop(guard);
                        return Ok(ReplyWrite {
                            written: data.len() as u32,
                        });
                    }
                }
            }

            // 1. Get or acquire lease (fencing token)
            let fencing_token = self
                .get_or_acquire_lease(ino)
                .await
                .map_err(map_squeezefs_err)?;

            let file_path = crate::keys::inode_path(ino);
            let block_size = self.router.block_size.load(Ordering::Relaxed);
            // Prefer hot caches for path selection (avoids meta RTT on every small write).
            // write_file still loads authoritative layout when it mutates data.
            let (old_size, file_type) = if let Some(m) = self.router.metadata_cache.get(&ino) {
                (m.size, m.file_type.clone())
            } else if let Some((attr, cached_at)) = self.attr_cache.get(&ino) {
                if cached_at.elapsed() < Duration::from_secs(1) {
                    let ft = if attr.size > block_size {
                        "striped".to_string()
                    } else if attr.size > MAX_INLINE_SIZE {
                        "staged".to_string()
                    } else {
                        "inline".to_string()
                    };
                    (attr.size, ft)
                } else {
                    let meta = self
                        .router
                        .fetch_metadata(&file_path)
                        .await
                        .map_err(map_squeezefs_err)?;
                    (meta.size, meta.file_type.clone())
                }
            } else {
                let meta = self
                    .router
                    .fetch_metadata(&file_path)
                    .await
                    .map_err(map_squeezefs_err)?;
                (meta.size, meta.file_type.clone())
            };
            let is_striped = file_type == "striped";

            let bytes_written = data.len() as u32;
            let expected_new_size = std::cmp::max(old_size, offset + bytes_written as u64);

            let fits_inline = expected_new_size <= MAX_INLINE_SIZE
                && file_type != "staged"
                && file_type != "striped";
            let fits_staged = expected_new_size <= block_size
                && !self.router.cache.nvme.staging_dirs().is_empty()
                && file_type != "striped";

            let use_router_write =
                fits_inline || fits_staged || file_type == "inline" || file_type == "staged";
            let lock_scope = if file_type == "staged" && expected_new_size <= block_size {
                InodeWriteLockScope::MetaPrepOnly
            } else {
                inode_write_lock_scope(use_router_write, is_striped)
            };

            // Kernel clock domain (fstests generic/423) — an attr-cache
            // time publish is a daemon-authored inode stamp like any
            // other; see `crate::coarse_realtime_ns`.
            let now_ns = crate::coarse_realtime_ns() as i64;
            let sec = now_ns.div_euclid(1_000_000_000);
            let nsec = now_ns.rem_euclid(1_000_000_000) as u32;

            // (fstests generic/795: the attr size/mtime publish moved to
            // AFTER the dispatch below — SIZE MUST NEVER LEAD DATA. The
            // pre-dispatch publish let a concurrent reader observe
            // size = expected_new_size while this write's bytes were not
            // yet in any overlay: the read clamps to the published size
            // and legitimately composes ZEROS for the not-yet-landed
            // range — acked-looking zeros at stable offsets, the 795
            // cmp-mismatch signature.)

            if use_router_write {
                // §5.4 lease-severance boundary — the single sever route:
                // the router's inline/staged commits retain the payload
                // `Bytes` unboundedly (`data_key`, `write_lru`, `read_lru`)
                // and its internal promotions/striped section slice it
                // across device writes (and, post-PR 6, retain full-coverage
                // slices in the read LRU) — a transport payload lease here
                // would park the ring ent's COMMIT_AND_FETCH for as long as
                // the cache holds it (deterministic mount hang at
                // Q_DEPTH=4). Materialize a private copy before anything
                // reaches `DataRouter::write_file`.
                let data_bytes = sever_payload(&data);
                // FIND-RW5-A face 3: one fresh-lease retry on a transient
                // adjacent-bump fence (see the striped arm below).
                let held_guard = if lock_scope == InodeWriteLockScope::MetaPrepOnly {
                    drop(guard);
                    None
                } else {
                    Some(guard)
                };
                let mut token = fencing_token;
                let mut attempt = 0u32;
                loop {
                    match self
                        .router
                        .write_file(&file_path, offset, data_bytes.clone(), token)
                        .await
                    {
                        Ok(()) => break,
                        Err(e @ SqueezefsError::FencingTokenExpired { .. }) => {
                            self.invalidate_local_lease(ino);
                            if attempt >= 1 {
                                return Err(map_squeezefs_err(e));
                            }
                            attempt += 1;
                            token = self
                                .get_or_acquire_lease(ino)
                                .await
                                .map_err(map_squeezefs_err)?;
                        }
                        Err(e) => return Err(map_squeezefs_err(e)),
                    }
                }
                drop(held_guard);
            } else {
                // `use_router_write` is unconditionally true for
                // `file_type == "inline" || "staged"` (the condition names
                // those values verbatim), so this branch only ever sees
                // striped files — inline/staged→striped promotions resolve
                // inside `DataRouter::write_file` on the severed route
                // above. Every striped shape (aligned or not, complete
                // blocks or partial) funnels through `write_file_staged`,
                // whose per-block write-through (§5.3) uploads
                // content-complete blocks directly: the one striped write
                // path. PR 6 deleted the in-handler promotion block (dead:
                // its `file_type` guard could never hold here) and the
                // subsumed `is_aligned` direct leg.
                //
                // Striped: drop inode write lock before long active-block
                // I/O (P1-8); per-block BLOCK_FLUSH_LOCKS serialize the
                // data path.
                drop(guard);
                write_phase_record(WritePhase::RouteClassify, wp_route);
                // FIND-RW5-A face 3: a transient adjacent-bump fence
                // (our own lease churn — single-writer mount) retries once
                // with a fresh lease instead of surfacing EIO; a second
                // fence is genuine and stays loud.
                let mut token = fencing_token;
                let mut attempt = 0u32;
                loop {
                    match self
                        .write_file_staged(ino, offset, data.clone(), old_size, token)
                        .await
                    {
                        Ok(()) => break,
                        Err(e @ SqueezefsError::FencingTokenExpired { .. }) => {
                            self.invalidate_local_lease(ino);
                            if attempt >= 1 {
                                return Err(map_squeezefs_err(e));
                            }
                            attempt += 1;
                            token = self
                                .get_or_acquire_lease(ino)
                                .await
                                .map_err(map_squeezefs_err)?;
                        }
                        Err(e) => return Err(map_squeezefs_err(e)),
                    }
                }
            }

            prof.mark_backend_done();
            // Size/mtime publish — strictly AFTER the data landed (either
            // router commit or the striped overlays; both are readable
            // now), so size never leads data (generic/795). The striped
            // path defers its durable size persist to the flush cadence:
            // the RAM metadata_cache floor bump here is its ONLY size
            // publish, and it must trail `write_file_staged` for the same
            // reason the attr publish below does — the read path clamps
            // `read_len` to this entry's size, and a size the overlays
            // cannot back yet composes zeros for the gap.
            if !use_router_write && expected_new_size > old_size {
                self.router
                    .update_metadata_cache_size(&file_path, expected_new_size)
                    .await;
            }
            if let Some((mut attr, _)) = self.attr_cache.get(&ino) {
                attr.size = attr.size.max(expected_new_size);
                attr.blocks = attr.size.div_ceil(512);
                attr.mtime = Timestamp::new(sec, nsec);
                attr.ctime = Timestamp::new(sec, nsec);
                self.attr_cache
                    .insert(ino, (attr, std::time::Instant::now()));
            }
            Ok(ReplyWrite {
                written: bytes_written,
            })
        };

        write_future.await
    }

    async fn mkdir(
        &self,
        req: Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
    ) -> FuseResult<ReplyEntry> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        check_component_name_len(name)?;
        let name_str = osstr_to_cow(name);
        debug!(
            "FUSE mkdir: parent = {}, name = {}, mode = {:o}, umask = {:o}",
            parent, name_str, mode, umask
        );

        let prof = OpProf::begin(FuseOpKind::Mkdir, parent);
        let mkdir_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .expect("meta_backend must be configured");
            let final_mode = ((mode & !umask) & 0o7777) | libc::S_IFDIR;
            prof.mark_backend_start();
            let backend_res = backend
                .create(parent, &name_str, final_mode, req.uid, req.gid)
                .await;
            prof.mark_backend_done();
            let inode = backend_res.map_err(map_squeezefs_err)?;
            let attr = self.inode_to_file_attr(&inode);
            self.attr_cache
                .insert(inode.ino, (attr, std::time::Instant::now()));
            self.bump_dir_generation(parent);
            self.attr_cache.invalidate(&parent);
            Ok(ReplyEntry {
                ttl: self.entry_ttl_for(attr.kind),
                attr,
                generation: 1,
            })
        };

        mkdir_future.await
    }

    async fn rmdir(&self, _req: Request, parent: u64, name: &OsStr) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        check_component_name_len(name)?;
        let name_str = osstr_to_cow(name);
        debug!("FUSE rmdir: parent = {}, name = {}", parent, name_str);

        let prof = OpProf::begin(FuseOpKind::Rmdir, parent);
        let rmdir_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .expect("meta_backend must be configured");
            prof.mark_backend_start();
            let current_inode = backend
                .lookup(parent, &name_str)
                .await
                .map_err(map_squeezefs_err)?;
            if current_inode.mode & libc::S_IFMT == libc::S_IFDIR {
                let list = backend
                    .readdir(current_inode.ino, 0, 100)
                    .await
                    .map_err(map_squeezefs_err)?;
                let has_other_entries = list.iter().any(|d| d.name != "." && d.name != "..");
                if has_other_entries {
                    return Err(Errno::from(libc::ENOTEMPTY));
                }
            } else {
                return Err(Errno::from(libc::ENOTDIR));
            }
            let backend_res = backend.unlink(parent, &name_str).await;
            prof.mark_backend_done();
            backend_res.map_err(map_squeezefs_err)?;
            self.bump_dir_generation(parent);
            self.bump_dir_generation(current_inode.ino);
            self.attr_cache.invalidate(&parent);
            self.attr_cache.invalidate(&current_inode.ino);
            // Reclaim only after FUSE forget (or last release if unlinked-open).
            // Destroying before forget reuses ino numbers while the kernel still
            // holds the nodeid (generation always 1) → ESTALE under load.
            Ok(())
        };

        rmdir_future.await
    }

    async fn setattr(
        &self,
        _req: Request,
        ino: u64,
        _fh: Option<u64>,
        set_attr: SetAttr,
    ) -> FuseResult<ReplyAttr> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);

        if ino == CONFIG_INODE {
            return Err(Errno::from(libc::EACCES));
        }

        // D1.d: truncate mutates data/size — dirty the open generation.
        if set_attr.size.is_some() {
            self.mark_handle_dirty(ino);
        }

        let prof = OpProf::begin(FuseOpKind::Setattr, ino);
        let setattr_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .expect("meta_backend must be configured");
            prof.mark_backend_start();
            let current_inode = backend.getattr(ino).await.map_err(map_squeezefs_err)?;
            let mut size_to_set = None;
            let mut mode_to_set = None;
            if let Some(size) = set_attr.size {
                // The representable maximum is block_size × (2^32 − 1)
                // (u32 block indices — see `max_file_size`); beyond it the
                // pre-fix path wrapped indices mod 2^32 (fstests
                // generic/525). The old i64::MAX guard was never the real
                // boundary.
                if size > self.max_file_size() {
                    return Err(Errno::from(libc::EFBIG));
                }
                size_to_set = Some(size);
            }
            let mut uid_to_set = None;
            let mut gid_to_set = None;
            let mut atime_to_set = None;
            let mut mtime_to_set = None;
            let mut ctime_to_set = None;
            if let Some(mode) = set_attr.mode {
                let new_mode = (current_inode.mode & libc::S_IFMT) | (mode & 0o7777);
                mode_to_set = Some(new_mode);
            }
            if let Some(uid) = set_attr.uid {
                uid_to_set = Some(uid);
            }
            if let Some(gid) = set_attr.gid {
                gid_to_set = Some(gid);
            }
            if let Some(atime) = set_attr.atime {
                atime_to_set = Some(timestamp_to_ns_word(atime));
            }
            if let Some(mtime) = set_attr.mtime {
                mtime_to_set = Some(timestamp_to_ns_word(mtime));
            }
            if let Some(ctime) = set_attr.ctime {
                ctime_to_set = Some(timestamp_to_ns_word(ctime));
            }
            let _guard = if size_to_set.is_some() {
                Some(self.active_inode_locks.get_inode_lock(ino).write().await)
            } else {
                None
            };

            if let Some(new_size) = size_to_set {
                // Truncate is a MUTATION: hold the shared op lease exactly
                // like the write path (get_or_acquire_lease — cached-lease
                // reuse), never a bare token snapshot. The snapshot raced
                // any transient background acquirer (drain/clone-class)
                // re-acquiring after release dropped the cached lease: the
                // token bumped between the snapshot and the layout save's
                // fence, and the truncate — since the O_TRUNC fix, run on
                // every open(O_TRUNC) — surfaced the transient as EIO to
                // open(2) (the aborted first generic/074 ×20, run 2).
                // With the shared cache, either both sides reuse one lease
                // (no bump) or the acquisition serializes behind the
                // transient holder — no stale-token window exists.
                let fencing_token = self
                    .get_or_acquire_lease(ino)
                    .await
                    .map_err(map_squeezefs_err)?;
                // Classify grow-vs-shrink against the FRESHEST size, never the
                // durable inode size alone: staged/inline writes defer their
                // layout+size persist, so `current_inode.size` lags and a real
                // shrink would be misclassified as a grow (skipping the
                // straddle-zero below — the generic/075 stale-tail trap).
                let old_size = self
                    .freshest_size(ino)
                    .await
                    .map_err(map_squeezefs_err)?
                    .max(current_inode.size);

                // Striped shrink to a non-block-aligned size: the block that
                // straddles new_size survives the map removal below with stale
                // bytes in [new_size, block_end). A later re-extend would read
                // those instead of zeros (a hole must read zeros). RMW-zero that
                // tail now, while the file is still at its old size so the write
                // never grows it — the block is then stored clean. This consumes
                // (and thereby clips) any parked/staged overlay of the straddling
                // block as its RMW base.
                if new_size < old_size {
                    let bs = self.router.block_size.load(Ordering::Relaxed);
                    if bs > 0 && new_size % bs != 0 {
                        let file_path = crate::keys::inode_path(ino);
                        if let Ok(meta) = self.router.fetch_metadata(&file_path).await {
                            if meta.file_type == "striped" {
                                let block_end = (new_size / bs + 1) * bs;
                                let zero_to = std::cmp::min(old_size, block_end);
                                if zero_to > new_size {
                                    let zeros = bytes::Bytes::from(vec![
                                        0u8;
                                        (zero_to - new_size)
                                            as usize
                                    ]);
                                    self.write_file_staged(
                                        ino,
                                        new_size,
                                        zeros,
                                        old_size,
                                        fencing_token,
                                    )
                                    .await
                                    .map_err(map_squeezefs_err)?;
                                }
                            }
                        }
                    }
                }

                // Blocks entirely at/after new_size are GONE: their parked RAM
                // buffers and staged `active_block:` overlays must die with
                // them, or the stale overlay outlives the map prune — serving
                // pre-truncate bytes to single-block reads, seeding the next
                // partial write's RMW, and re-merging the whole stale block on
                // the next flush (the generic/075.2 resurrection). Purge BEFORE
                // the map prune so a racing writeback upload NotFound-skips;
                // one that already merged is pruned by truncate_layout below.
                self.drop_active_block_overlays_beyond(ino, new_size).await;

                if let Err(e) = self
                    .router
                    .truncate_layout(ino, new_size, fencing_token)
                    .await
                {
                    // Same hygiene as the write path: a fenced lease is
                    // stale — drop the local cache so the retry (kernel or
                    // app) re-acquires fresh.
                    if matches!(e, SqueezefsError::FencingTokenExpired { .. }) {
                        self.invalidate_local_lease(ino);
                    }
                    return Err(map_squeezefs_err(e));
                }
            }

            let backend_res = backend
                .setattr(
                    ino,
                    mode_to_set,
                    uid_to_set,
                    gid_to_set,
                    size_to_set,
                    atime_to_set,
                    mtime_to_set,
                    ctime_to_set,
                )
                .await;
            prof.mark_backend_done();
            let inode = backend_res.map_err(map_squeezefs_err)?;
            let mut attr = self.inode_to_file_attr(&inode);
            // A metadata-only setattr (chmod/chown/utimes — no `size` in the
            // request) must never change the file size. The durable inode can
            // lag a deferred (cached-but-not-yet-committed) write, so reconcile
            // against the freshest cached size and never regress it — otherwise
            // a chmod right after a write truncates the file to the stale
            // durable size (0), zeroing reads and causing SIGBUS on mmap
            // (LTP mmap02).
            if size_to_set.is_none() {
                if let Some((cached, _)) = self.attr_cache.get(&ino) {
                    if cached.size > attr.size {
                        attr.size = cached.size;
                        attr.blocks = cached.blocks;
                    }
                }
            }
            self.attr_cache
                .insert(ino, (attr, std::time::Instant::now()));
            Ok(ReplyAttr {
                ttl: self.kernel_ttls.attr,
                attr,
            })
        };

        setattr_future.await
    }

    async fn symlink(
        &self,
        req: Request,
        parent: u64,
        name: &OsStr,
        link: &OsStr,
    ) -> FuseResult<ReplyEntry> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        check_component_name_len(name)?;
        if link.len() > 4096 {
            return Err(Errno::from(libc::ENAMETOOLONG));
        }
        let name_str = osstr_to_cow(name);
        let link_str = osstr_to_cow(link);
        debug!(
            "FUSE symlink: parent = {}, name = {}, link = {}",
            parent, name_str, link_str
        );

        let prof = OpProf::begin(FuseOpKind::Symlink, parent);
        let symlink_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .ok_or_else(|| {
                    SqueezefsError::InvalidOperation("Metadata backend not initialized".to_string())
                })
                .map_err(map_squeezefs_err)?;

            let final_mode = 0o777 | libc::S_IFLNK;
            prof.mark_backend_start();
            let inode = backend
                .create(parent, &name_str, final_mode, req.uid, req.gid)
                .await
                .map_err(map_squeezefs_err)?;
            backend
                .setxattr(inode.ino, "system.symlink", link_str.as_bytes())
                .await
                .map_err(map_squeezefs_err)?;
            prof.mark_backend_done();
            let mut attr = self.inode_to_file_attr(&inode);
            attr.size = link_str.len() as u64;
            attr.blocks = 1;
            self.attr_cache
                .insert(inode.ino, (attr, std::time::Instant::now()));
            self.bump_dir_generation(parent);
            self.attr_cache.invalidate(&parent);
            Ok(ReplyEntry {
                ttl: self.entry_ttl_for(attr.kind),
                attr,
                generation: 1,
            })
        };

        symlink_future.await
    }

    async fn readlink(&self, _req: Request, ino: u64) -> FuseResult<ReplyData> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        let backend = self
            .meta_backend
            .as_ref()
            .ok_or_else(|| {
                SqueezefsError::InvalidOperation("Metadata backend not initialized".to_string())
            })
            .map_err(map_squeezefs_err)?;
        let target = backend
            .getxattr(ino, "system.symlink")
            .await
            .map_err(map_squeezefs_err)?;
        let target_bytes = target.ok_or_else(|| Errno::from(libc::ENOENT))?;
        debug!(
            "FUSE readlink: ino = {}, target = {}",
            ino,
            String::from_utf8_lossy(&target_bytes)
        );
        Ok(ReplyData {
            data: target_bytes.into(),
            backing: None,
        })
    }

    async fn link(
        &self,
        _req: Request,
        ino: u64,
        new_parent: u64,
        new_name: &OsStr,
    ) -> FuseResult<ReplyEntry> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        check_component_name_len(new_name)?;
        let new_name_str = osstr_to_cow(new_name);
        debug!(
            "FUSE link: ino = {}, new_parent = {}, new_name = {}",
            ino, new_parent, new_name_str
        );

        let prof = OpProf::begin(FuseOpKind::Link, new_parent);
        let link_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .ok_or_else(|| {
                    SqueezefsError::InvalidOperation("Metadata backend not initialized".to_string())
                })
                .map_err(map_squeezefs_err)?;

            prof.mark_backend_start();
            let inode = backend
                .link(ino, new_parent, &new_name_str)
                .await
                .map_err(map_squeezefs_err)?;
            prof.mark_backend_done();
            let attr = self.inode_to_file_attr(&inode);
            self.attr_cache
                .insert(ino, (attr, std::time::Instant::now()));
            self.attr_cache.invalidate(&new_parent);
            self.bump_dir_generation(new_parent);
            return Ok(ReplyEntry {
                ttl: self.entry_ttl_for(attr.kind),
                attr,
                generation: 1,
            });
        };

        link_future.await
    }

    async fn unlink(&self, _req: Request, parent: u64, name: &OsStr) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        crate::coz_progress!("fuse_unlink");
        check_component_name_len(name)?;
        let name_str = osstr_to_cow(name);
        debug!("FUSE unlink: parent = {}, name = {}", parent, name_str);

        if parent == 1 && name_str == ".config" {
            return Err(Errno::from(libc::EPERM));
        }

        let prof = OpProf::begin(FuseOpKind::Unlink, parent);
        let unlink_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .expect("meta_backend must be configured");
            prof.mark_backend_start();
            let backend_res = backend.unlink(parent, &name_str).await;
            prof.mark_backend_done();
            let child_ino = backend_res.map_err(map_squeezefs_err)?;
            self.bump_dir_generation(parent);
            // D2.c: refresh (not invalidate) — the kernel re-GETATTRs the
            // parent on its next path walk (fuse_dir_changed) and the
            // child for its post-unlink ctime bookkeeping; serve both
            // from cache at ~µs instead of a contended backend fetch
            // (M2: getattr 1.82/unlink @ 45 µs).
            self.refresh_attr_cache(parent).await;
            self.refresh_attr_cache(child_ino).await;
            // Defer destroy_inode until forget/release (see rmdir comment).
            Ok(())
        };

        unlink_future.await
    }

    async fn rename(
        &self,
        _req: Request,
        parent: u64,
        name: &OsStr,
        new_parent: u64,
        new_name: &OsStr,
    ) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        check_component_name_len(name)?;
        check_component_name_len(new_name)?;
        let name_str = osstr_to_cow(name);
        let new_name_str = osstr_to_cow(new_name);
        debug!(
            "FUSE rename: parent = {}, name = {}, new_parent = {}, new_name = {}",
            parent, name_str, new_parent, new_name_str
        );

        if (parent == 1 && name_str == ".config") || (new_parent == 1 && new_name_str == ".config")
        {
            return Err(Errno::from(libc::EPERM));
        }

        let prof = OpProf::begin(FuseOpKind::Rename, parent);
        let rename_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .expect("meta_backend must be configured");
            prof.mark_backend_start();
            let dest_ino = if let Ok(inode) = backend.lookup(new_parent, &new_name_str).await {
                Some(inode.ino)
            } else {
                None
            };
            let backend_res = backend
                .rename(parent, &name_str, new_parent, &new_name_str, 0)
                .await;
            prof.mark_backend_done();
            backend_res.map_err(map_squeezefs_err)?;
            self.bump_dir_generation(parent);
            self.bump_dir_generation(new_parent);
            // D2.c: refresh (not invalidate) — post-M6 renames really move
            // both parents' times, so the kernel revalidates them (+0.21
            // fuse_ops/rename); keep those GETATTRs on the cache.
            self.refresh_attr_cache(parent).await;
            if new_parent != parent {
                self.refresh_attr_cache(new_parent).await;
            }
            if let Some(d_ino) = dest_ino {
                self.refresh_attr_cache(d_ino).await;
                // Reclaim overwritten target via forget, not here.
            }
            Ok(())
        };

        rename_future.await
    }

    async fn rename2(
        &self,
        _req: Request,
        parent: u64,
        name: &OsStr,
        new_parent: u64,
        new_name: &OsStr,
        flags: u32,
    ) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        check_component_name_len(name)?;
        check_component_name_len(new_name)?;
        let name_str = osstr_to_cow(name);
        let new_name_str = osstr_to_cow(new_name);
        debug!(
            "FUSE rename2: parent = {}, name = {}, new_parent = {}, new_name = {}, flags = {}",
            parent, name_str, new_parent, new_name_str, flags
        );

        // Refuse flags this filesystem does not implement — LOUD, never a
        // silent plain rename (fstests generic/078; pinned in
        // tests/rename_semantics_tests.rs). RENAME_WHITEOUT is
        // IMPLEMENTED (fstests generic/631 — the overlayfs-upper
        // contract: the backend mints the char-0:0 whiteout atomically
        // inside the rename tx); WHITEOUT|EXCHANGE stays refused (the
        // VFS forbids the combination — defensive here, load-bearing in
        // the backend).
        if flags & !(libc::RENAME_NOREPLACE | libc::RENAME_EXCHANGE | libc::RENAME_WHITEOUT) != 0 {
            return Err(Errno::from(libc::EINVAL));
        }
        if flags & libc::RENAME_WHITEOUT != 0 && flags & libc::RENAME_EXCHANGE != 0 {
            return Err(Errno::from(libc::EINVAL));
        }

        if (parent == 1 && name_str == ".config") || (new_parent == 1 && new_name_str == ".config")
        {
            return Err(Errno::from(libc::EPERM));
        }

        let prof = OpProf::begin(FuseOpKind::Rename, parent);
        let rename_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .expect("meta_backend must be configured");

            prof.mark_backend_start();
            let src_ino = if let Ok(inode) = backend.lookup(parent, &name_str).await {
                Some(inode.ino)
            } else {
                None
            };
            let dest_ino = if let Ok(inode) = backend.lookup(new_parent, &new_name_str).await {
                Some(inode.ino)
            } else {
                None
            };

            let backend_res = backend
                .rename(parent, &name_str, new_parent, &new_name_str, flags)
                .await;
            prof.mark_backend_done();
            backend_res.map_err(map_squeezefs_err)?;

            self.bump_dir_generation(parent);
            self.bump_dir_generation(new_parent);
            // D2.c: refresh (not invalidate) — see `rename`.
            self.refresh_attr_cache(parent).await;
            if new_parent != parent {
                self.refresh_attr_cache(new_parent).await;
            }
            if let Some(s_ino) = src_ino {
                self.refresh_attr_cache(s_ino).await;
            }
            if let Some(d_ino) = dest_ino {
                self.refresh_attr_cache(d_ino).await;
                // Overwritten target reclaimed on forget only (not RENAME_EXCHANGE).
            }
            Ok(())
        };

        rename_future.await
    }

    async fn readdir<'a>(
        &'a self,
        _req: Request,
        parent: u64,
        fh: u64,
        offset: i64,
    ) -> FuseResult<ReplyDirectory<Self::DirEntryStream<'a>>> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        crate::coz_progress!("fuse_readdir");
        debug!(
            "FUSE readdir: parent = {}, fh = {}, offset = {}",
            parent, fh, offset
        );

        let prof = OpProf::begin(FuseOpKind::Readdir, parent);
        let readdir_future = async {
            // PR K7 (§5.1): directories stream by key cookies.
            let off_u = offset.max(0) as u64;
            prof.mark_backend_start();
            let page = self
                .v3_listing_page(parent, off_u, V3_READDIR_PAGE)
                .await
                .map_err(map_squeezefs_err)?;
            prof.mark_backend_done();
            let mut entries = Vec::with_capacity(page.len() + 2);
            if off_u < 1 {
                entries.push(DirectoryEntry {
                    name: ".".into(),
                    kind: FileType::Directory,
                    inode: parent,
                    offset: 1,
                });
            }
            if off_u < 2 {
                let parent_parent = if parent == 1 {
                    1
                } else if let Some(ref backend) = self.meta_backend {
                    backend
                        .lookup(parent, "..")
                        .await
                        .map(|inode| inode.ino)
                        .unwrap_or(1)
                } else {
                    1
                };
                entries.push(DirectoryEntry {
                    name: "..".into(),
                    kind: FileType::Directory,
                    inode: parent_parent,
                    offset: 2,
                });
            }
            for (cookie, d) in page {
                entries.push(DirectoryEntry {
                    name: d.name.into(),
                    kind: self.mode_to_file_type(d.file_type),
                    inode: d.ino,
                    offset: cookie as i64,
                });
            }
            // The root virtuals (.config/.stats) are LOOKUP-ONLY — never
            // listed (the .zfs/.lustre hidden control-file pattern;
            // fstests generic/062: recursive walks must not see
            // fabricated files). Their reserved cookies remain honored on
            // resume (a stale pre-hide cookie terminates the stream).
            use futures::stream::{self, StreamExt};
            let stream = stream::iter(entries.into_iter().map(Ok)).boxed();
            Ok(ReplyDirectory { entries: stream })
        };

        readdir_future.await
    }

    async fn readdirplus<'a>(
        &'a self,
        _req: Request,
        parent: u64,
        fh: u64,
        offset: u64,
        _lock_owner: u64,
    ) -> FuseResult<ReplyDirectoryPlus<Self::DirEntryPlusStream<'a>>> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!(
            "FUSE readdirplus: parent = {}, fh = {}, offset = {}",
            parent, fh, offset
        );

        let prof = OpProf::begin(FuseOpKind::Readdirplus, parent);
        let readdirplus_future = async {
            // PR K7 (§5.1): directories stream by key cookies.
            {
                prof.mark_backend_start();
                let page = self
                    .v3_listing_page(parent, offset, V3_READDIRPLUS_PAGE)
                    .await
                    .map_err(map_squeezefs_err)?;
                prof.mark_backend_done();
                let mut entries = Vec::with_capacity(page.len() + 2);
                if offset < 1 {
                    let attr = self
                        .get_attr_internal(parent)
                        .await
                        .map_err(map_squeezefs_err)?;
                    entries.push(DirectoryEntryPlus {
                        name: ".".into(),
                        kind: FileType::Directory,
                        inode: parent,
                        generation: 1,
                        attr,
                        entry_ttl: self.kernel_ttls.dir_entry,
                        attr_ttl: self.kernel_ttls.attr,
                        offset: 1,
                    });
                }
                if offset < 2 {
                    let parent_parent = if parent == 1 {
                        1
                    } else if let Some(ref backend) = self.meta_backend {
                        backend
                            .lookup(parent, "..")
                            .await
                            .map(|inode| inode.ino)
                            .unwrap_or(1)
                    } else {
                        1
                    };
                    let attr = self
                        .get_attr_internal(parent_parent)
                        .await
                        .map_err(map_squeezefs_err)?;
                    entries.push(DirectoryEntryPlus {
                        name: "..".into(),
                        kind: FileType::Directory,
                        inode: parent_parent,
                        generation: 1,
                        attr,
                        entry_ttl: self.kernel_ttls.dir_entry,
                        attr_ttl: self.kernel_ttls.attr,
                        offset: 2,
                    });
                }
                for (cookie, d) in page {
                    let attr = match self.get_attr_internal(d.ino).await {
                        Ok(a) => a,
                        Err(e) => {
                            error!(
                                "readdirplus failed to get attr for child {}: {:?}",
                                d.ino, e
                            );
                            continue;
                        }
                    };
                    entries.push(DirectoryEntryPlus {
                        name: d.name.into(),
                        kind: attr.kind,
                        inode: d.ino,
                        generation: 1,
                        attr,
                        entry_ttl: self.entry_ttl_for(attr.kind),
                        attr_ttl: self.kernel_ttls.attr,
                        offset: cookie as i64,
                    });
                }
                // Root virtuals are LOOKUP-ONLY — see the readdir twin.
                use futures::stream::{self, StreamExt};
                let stream = stream::iter(entries.into_iter().map(Ok)).boxed();
                Ok(ReplyDirectoryPlus { entries: stream })
            }
        };

        readdirplus_future.await
    }

    #[allow(clippy::too_many_arguments)]
    async fn copy_file_range(
        &self,
        _req: Request,
        inode: u64,
        _fh_in: u64,
        off_in: u64,
        inode_out: u64,
        _fh_out: u64,
        off_out: u64,
        length: u64,
        _flags: u64,
    ) -> FuseResult<ReplyCopyFileRange> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        // VL8 item 2: register BEFORE the guards — the live wedge's stuck
        // cfr handlers were watchdog-invisible exactly while guard-blocked.
        let _prof = OpProf::begin(FuseOpKind::CopyFileRange, inode);
        debug!(
            "FUSE copy_file_range: src_ino = {}, off_in = {}, dest_ino = {}, off_out = {}, length = {}",
            inode, off_in, inode_out, off_out, length
        );

        // D1.d: the DESTINATION gains flushable state (the source is
        // read-only here).
        self.mark_handle_dirty(inode_out);

        // u32 block-index representability (see `max_file_size`) — EFBIG
        // past the cap, like write/truncate (generic/525).
        if off_out.saturating_add(length) > self.max_file_size() {
            return Err(Errno::from(libc::EFBIG));
        }

        let src_path = format!("inode_{}", inode);
        let dest_path = format!("inode_{}", inode_out);

        // 0. Make the SOURCE fully visible to the router-level reads below.
        // The copy reads the source through `DataRouter::read_file` (and the
        // whole-file clone through the block map), which cannot see the
        // FUSE-layer parked `ActiveBlockBuf`s / staged `active_block:`
        // overlays a partial striped write leaves behind — the copy would
        // silently source pre-write bytes (zeros for a hole-extending write):
        // the generic/075.2 lost-copy corruption. Flush-merges them into the
        // map BEFORE taking the inode guards (the flush takes the write guard
        // internally when it has work; a no-overlay scan is one prefix pass).
        {
            let src_flush_token = self.dlm.get_fencing_token_ino(inode);
            let _ = self
                .flush_active_blocks_with_retry(inode, src_flush_token)
                .await;
            // generic/795, the whole-file-clone face: the scan above finds
            // RAM overlays only — custody parked one station DOWN the chain
            // (the staged `active_block:` sibling a reader-triggered or
            // memory-pressure flush leaves behind, durable merge still
            // queued) is invisible to it, and the clone fast path below
            // would snapshot a block map with those blocks MISSING: a
            // full-size clone whose unmerged blocks read ZEROS, durably
            // (the fstests cmp signature at the first parked-block
            // boundary). Drain any below-size unbound block that has a
            // staged sibling; bounded — each pass merges what it found.
            let bs = self.router.block_size.load(Ordering::Relaxed);
            for _ in 0..4 {
                let Ok(meta) = self.router.fetch_metadata(&src_path).await else {
                    break;
                };
                if meta.file_type != "striped" || meta.size == 0 {
                    break;
                }
                let blocks = meta.size.div_ceil(bs) as u32;
                let mut missing: Vec<u32> = Vec::new();
                for b in 0..blocks {
                    // Bound OR unbound: any staged sibling is undrained
                    // acked custody (a sibling on a BOUND block supersedes
                    // the bound image — the durable copy is stale).
                    let key = crate::keys::active_block(inode, b as u64).to_string();
                    if self.router.cache.nvme.has_staged_active_block(&key) {
                        missing.push(b);
                    }
                }
                if missing.is_empty() {
                    break;
                }
                let _ = flush_due_active_blocks_for_inode(
                    inode,
                    missing,
                    &self.router,
                    &self.dlm,
                    &self.active_inode_locks,
                )
                .await;
            }
        }

        // 1. Acquire local locks on both inodes to ensure consistency and prevent deadlocks
        let src_lock_arc = self.get_inode_lock(inode);
        let dest_lock_arc = if inode != inode_out {
            Some(self.get_inode_lock(inode_out))
        } else {
            None
        };

        let same_lock = if let Some(dest_lock) = dest_lock_arc {
            std::ptr::eq(src_lock_arc, dest_lock)
        } else {
            true
        };

        let _src_read_guard;
        let _src_write_guard;
        let _dest_write_guard;
        if same_lock {
            _src_write_guard = Some(src_lock_arc.write().await);
            _src_read_guard = None;
            _dest_write_guard = None;
        } else if self.inode_pair_lock_order(inode, inode_out).0 == inode {
            _src_read_guard = Some(src_lock_arc.read().await);
            _dest_write_guard = Some(dest_lock_arc.as_ref().unwrap().write().await);
            _src_write_guard = None;
        } else {
            _dest_write_guard = Some(dest_lock_arc.as_ref().unwrap().write().await);
            _src_read_guard = Some(src_lock_arc.read().await);
            _src_write_guard = None;
        }

        // Per-ino CACHED leases — the write path's discipline
        // (`get_or_acquire_lease`): once acquired, a lease persists in
        // `active_leases` until release/reclaim, so cfr storms stop paying
        // (and stop LOSING) a fresh 5s DLM wait per copy. Acquire in
        // ascending-ino order (a stable total order; the old path-sorted
        // TEMPORARY leases still returned EAGAIN to userspace whenever a
        // conveyor tx co-owned the ino's 4a guard across a >5s batch stall
        // — cp aborted the copy, fstests generic/795's
        // "Resource temporarily unavailable"). One brief bounded retry
        // absorbs the stall; a persistent holder still fails loud.
        let mut lease_tokens: [(u64, u64); 2] = [(inode, 0), (inode_out, 0)];
        {
            let mut order = [inode.min(inode_out), inode.max(inode_out)];
            if order[0] == order[1] {
                order[1] = 0; // dedup sentinel (ino 0 never occurs)
            }
            for &i in order.iter().filter(|&&i| i != 0) {
                let mut attempt = 0u32;
                let tok = loop {
                    match self.get_or_acquire_lease(i).await {
                        Ok(t) => break t,
                        Err(e) if attempt < 2 => {
                            attempt += 1;
                            debug!(
                                "copy_file_range: lease wait on ino {i} lost (attempt {attempt}): {e:?}; retrying"
                            );
                            tokio::time::sleep(Duration::from_millis(250)).await;
                        }
                        Err(e) => {
                            error!("copy_file_range: failed to acquire lease on ino {i}: {e:?}");
                            return Err(Errno::from(libc::EAGAIN));
                        }
                    }
                };
                for slot in lease_tokens.iter_mut() {
                    if slot.0 == i {
                        slot.1 = tok;
                    }
                }
            }
        }
        let src_token = lease_tokens[0].1;
        let dest_token = lease_tokens[1].1;

        // 2. Read sizes to check if we can perform metadata clone
        let src_size = self
            .router
            .get_file_size(&src_path)
            .await
            .map_err(map_squeezefs_err)?;
        let dest_size = self.router.get_file_size(&dest_path).await.unwrap_or(0);

        // Whole-file clone fast path — ONLY when the source's acked custody
        // is fully merged (freeze-clean): under the held guards, every
        // below-size block must be either map-bound or genuinely hole
        // (no RAM overlay, no staged sibling). A write that re-parked
        // custody between the pre-guard drain and the guard acquisition
        // demotes this copy to the chunked path below, which composes
        // parked runs correctly (generic/795).
        let clone_freeze_clean =
            if off_in == 0 && off_out == 0 && length >= src_size && dest_size == 0 {
                match self.router.fetch_metadata(&src_path).await {
                    Ok(meta) if meta.file_type == "striped" && meta.size > 0 => {
                        let bs = self.router.block_size.load(Ordering::Relaxed);
                        let blocks = meta.size.div_ceil(bs) as u32;
                        (0..blocks).all(|b| {
                            // BOUND blocks are not automatically clean: a
                            // revisit overlay / staged sibling on a bound block
                            // means the durable image is a GAP-BAKED stale copy
                            // whose truth is parked (the clone would share the
                            // stale key while the source serves the overlay).
                            let key = crate::keys::active_block(inode, b as u64).to_string();
                            let ext = crate::keys::active_block_ext(inode, b as u64).to_string();
                            let unbound = !meta
                                .block_map
                                .as_ref()
                                .map(|m| m.contains_key(&b))
                                .unwrap_or(false);
                            let parked = self.active_block_buffers.contains_key(&key)
                                || self.router.cache.nvme.has_staged_active_block(&key)
                                || self.router.cache.nvme.has_staged_extent_record(&ext);
                            !(parked || (unbound && b as u64 * bs < meta.size))
                        })
                    }
                    Ok(_) => true,
                    Err(_) => false,
                }
            } else {
                false
            };
        if off_in == 0 && off_out == 0 && length >= src_size && dest_size == 0 && clone_freeze_clean
        {
            self.router
                .clone_file(&src_path, &dest_path, Some(src_token), Some(dest_token))
                .await
                .map_err(map_squeezefs_err)?;

            // Update destination attributes size and times in metadata backend
            if let Some(ref backend) = self.meta_backend {
                let _ = backend
                    .setattr(
                        inode_out,
                        None,
                        None,
                        None,
                        Some(src_size),
                        None,
                        None,
                        None,
                    )
                    .await;
            }

            self.attr_cache.invalidate(&inode_out);

            return Ok(ReplyCopyFileRange { copied: src_size });
        }

        // 3. General partial-range copy.
        //
        // The kernel (vfs_copy_file_range -> generic_copy_file_checks) has
        // already clamped `length` to the source's EOF (its cached `i_size`)
        // before dispatching, so a nonzero `length` here denotes bytes the
        // caller is entitled to copy. The handler must therefore ALWAYS make
        // forward progress: returning copied == 0 for a nonzero request wedges
        // a copy_file_range caller loop (fsx advances only on nr > 0 and never
        // breaks on nr == 0) — the live generic/616 (v3) / generic/112 (v2)
        // CRAWL. Honor the kernel's `length` directly; do NOT re-clamp against
        // our own `meta.size`. Under the FUSE writeback cache a deferred write
        // flush can race a truncate-extend and leave `meta.size` (what
        // get_file_size reports) lagging the kernel `i_size`, so clamping to it
        // would still return 0 for an off_in the kernel considers in-bounds
        // (reproduced live). A staged/inline file whose logical size outran its
        // physical data reads back short; treat that gap as a hole (zeros).
        if length == 0 {
            return Ok(ReplyCopyFileRange { copied: 0 });
        }

        // Bound per-call work/allocation; a short copy is legal and the caller
        // loops, so this keeps forward progress without an unbounded buffer.
        const CFR_MAX_CHUNK: u64 = 16 * 1024 * 1024;
        let effective_len = length.min(CFR_MAX_CHUNK) as usize;

        // O(chunk) source read — NEVER a whole-file materialization. The old
        // `read_file(&src_path)` assembled the entire source in RAM, which for
        // a huge sparse file (generic/285-scale: 8 GiB–16 TiB logical) meant
        // an O(logical size) allocation to serve a 64 KiB copy. The range
        // read serves interior holes as zeros at their correct positions;
        // `_src_backing` keeps any zero-copy mmap segment alive until the
        // chunk has been consumed below.
        //
        // OVERLAY NEVER INVISIBLE, the cfr face (fstests generic/795): the
        // router serves the BASE tiers only — a source block whose newest
        // acked bytes still sit in a RAM overlay (a buffered `head -c` +
        // `sync` leaves the last partial block parked; the writeback merge
        // lands later) would copy as ZEROS/stale into the destination and
        // every subsequent cmp of the copy differs at that block until the
        // source's overlay flushes. Compose exactly like the READ handler:
        // capture the parked runs before AND after the base read (the
        // moving-custody protocol's two sandwich halves).
        let src_pre_runs = self.capture_parked_runs(inode, off_in, effective_len);
        let (src_data, _src_backing) = self
            .router
            .read_file_range_zero_copy(
                &src_path,
                off_in,
                effective_len as u32,
                None,
                crate::routing::ReadClassHint::default(),
            )
            .await
            .map_err(map_squeezefs_err)?;
        let src_post_runs = self.capture_parked_runs(inode, off_in, effective_len);
        let phys = src_data.len();
        // `effective_len > 0` here (length > 0), so the chunk is always
        // non-empty regardless of the physical/logical gap.
        let chunk: bytes::Bytes =
            if phys >= effective_len && src_pre_runs.is_empty() && src_post_runs.is_empty() {
                // Fully backed by physical data, no overlays: zero-copy slice.
                src_data.slice(0..effective_len)
            } else {
                // Short read = the range runs past the physical tail into the
                // hole/EOF gap (a parked source overlay can also hold acked
                // bytes PAST the base's physical tail): assemble exactly
                // effective_len bytes — base data, zeros for the gap, then the
                // overlay runs composed over the top (pre first, post wins).
                let mut buf = vec![0u8; effective_len];
                let n = phys.min(effective_len);
                buf[..n].copy_from_slice(&src_data[..n]);
                let composed =
                    Self::apply_parked_runs(off_in, bytes::Bytes::from(buf), &src_pre_runs);
                Self::apply_parked_runs(off_in, composed, &src_post_runs)
            };

        // Perform write to destination
        let target_fencing_token = dest_token;

        let copied_len = effective_len as u64;
        let new_dest_size = std::cmp::max(dest_size, off_out + copied_len);
        // Route the destination write the same way the WRITE handler does:
        // a STRIPED destination goes through `write_file_staged` — the one
        // striped write path — whose per-block RMW consumes the parked
        // `ActiveBlockBuf` overlays. `DataRouter::write_file`'s striped leg
        // RMWs from the map/read tiers only, so a copy into a block with a
        // parked partial write would base itself on stale bytes AND be
        // shadowed by the parked overlay on the next read/flush (the
        // generic/075.2 family). Inline/staged destinations keep the router
        // write (their authoritative bases live router-side, and layout
        // promotion happens there).
        let dest_is_striped = self
            .router
            .metadata_cache
            .get(&inode_out)
            .map(|m| m.file_type == "striped")
            .unwrap_or_else(|| {
                // Cold cache: classify by the freshest known size, exactly
                // like the WRITE handler's attr fallback.
                dest_size > self.router.block_size.load(Ordering::Relaxed)
            });
        if dest_is_striped {
            self.write_file_staged(inode_out, off_out, chunk, dest_size, target_fencing_token)
                .await
                .map_err(map_squeezefs_err)?;
            // Size publish strictly AFTER the data landed — size must
            // never lead data (generic/795; same law as the WRITE
            // handler's post-dispatch publish).
            if new_dest_size > dest_size {
                self.router
                    .update_metadata_cache_size(&dest_path, new_dest_size)
                    .await;
            }
        } else {
            self.router
                .write_file(&dest_path, off_out, chunk, target_fencing_token)
                .await
                .map_err(map_squeezefs_err)?;
        }

        // Update destination size and times in metadata backend

        if let Some(ref backend) = self.meta_backend {
            let _ = backend
                .setattr(
                    inode_out,
                    None,
                    None,
                    None,
                    Some(new_dest_size),
                    None,
                    None,
                    None,
                )
                .await;
        }

        self.attr_cache.invalidate(&inode_out);

        Ok(ReplyCopyFileRange { copied: copied_len })
    }

    async fn statfs(&self, _req: Request, _ino: u64) -> FuseResult<ReplyStatFs> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        // Honest numbers, served entirely from maintained in-RAM state —
        // statfs is called constantly, so no metadata transactions and no
        // device I/O here.
        //
        // total  = the formatted capacity (FormatConfig.capacity: the
        //          summed data-backend size, or the lower explicit
        //          --capacity quota — the effective limit the user
        //          experiences). Set once at FUSE init.
        // free   = total − striped-block bytes allocated right now
        //          (allocator high-water atomic minus the recycled-free
        //          set, per distinct backend allocator). Inline payloads
        //          live in the metadata volume and staged-but-unpromoted
        //          writes in the local staging dirs; both promote into
        //          accounted blocks via writeback, so free converges on
        //          durability rather than tracking transient staging.
        // files  = the format inode quota; ffree = quota minus the v3
        //          monotonic (no-reuse) ino watermark progression.
        let bsize: u32 = 4096;
        let used_bytes = self.router.backend_router.allocated_bytes();
        // The capacity cell is set during FUSE init (the kernel sends
        // INIT before any statfs); if a non-standard harness asks
        // earlier, degrade to "everything used" rather than invent
        // capacity.
        let total_bytes = self.capacity_limit.get().copied().unwrap_or(used_bytes);
        let free_bytes = total_bytes.saturating_sub(used_bytes);

        let total_inodes = self.inodes_limit.get().copied().unwrap_or(0);
        // §4.8 monotonic ino allocation (no reuse): allocated-ever per
        // volume is the watermark minus the reserved base (ino 1 = root,
        // watermark starts at 2 on a fresh volume).
        let used_inodes: u64 = self
            .meta_backend
            .as_ref()
            .map(|backend| {
                backend
                    .volumes
                    .iter()
                    .map(|v| v.next_ino().saturating_sub(2))
                    .sum::<u64>()
                    .saturating_add(1) // the root inode itself
            })
            .unwrap_or(0);

        Ok(ReplyStatFs {
            blocks: total_bytes / bsize as u64,
            bfree: free_bytes / bsize as u64,
            bavail: free_bytes / bsize as u64,
            files: total_inodes,
            ffree: total_inodes.saturating_sub(used_inodes),
            bsize,
            namelen: 255,
            frsize: bsize,
        })
    }

    async fn flush(&self, _req: Request, ino: u64, _fh: u64, _lock_owner: u64) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Flush: ino = {}", ino);

        if ino == STATS_INODE || ino == CONFIG_INODE {
            return Ok(());
        }

        let prof = OpProf::begin(FuseOpKind::Flush, ino);

        // D1.d + D2.a (§5.1/§5.2, PR M5): a never-dirtied open generation
        // has nothing to flush — skip the lease acquire (a DLM map hit +
        // possible acquire) and the memory-buffer scan entirely, and reply
        // **ENOSYS**: under writeback cache (this daemon's default) the
        // kernel ignores FOPEN_NOFLUSH (uapi fuse.h: "don't flush data
        // cache on close (unless FUSE_WRITEBACK_CACHE)" — verified against
        // the running 7.1.3 kernel's own header and a live probe), and its
        // honored elision switch is the ENOSYS latch (`fc->no_flush`):
        // every later close skips the FLUSH round trip connection-wide
        // while dirty-page writeback + error reporting at close are
        // untouched (fuse_flush runs write_inode_now / fuse_sync_writes /
        // filemap_check_errors BEFORE the no_flush check, and converts
        // this ENOSYS itself to success — close(2) never sees it).
        // Dirty handles lose nothing: their FLUSH work was soft (fsync is
        // the durable barrier) and their close-time daemon flush rides
        // RELEASE's background path. The prof still drops → the rig's op
        // count stays exact.
        if !self.handle_dirty(ino) {
            METRICS
                .fuse_flush_clean_fastpath
                .fetch_add(1, Ordering::Relaxed);
            return Err(Errno::from(libc::ENOSYS));
        }

        prof.mark_backend_start();
        let fencing_token = self
            .get_or_acquire_lease(ino)
            .await
            .map_err(map_squeezefs_err)?;

        // Soft flush path: do not block FUSE flush on MetaLV layout persist or
        // full active-block promotion. sync_all/fsync is the durable barrier.
        let _ = self
            .flush_memory_buffers_for_inode(ino, fencing_token)
            .await;
        prof.mark_backend_done();

        Ok(())
    }

    async fn release(
        &self,
        _req: Request,
        ino: u64,
        fh: u64,
        _flags: u32,
        _lock_owner: u64,
        _flush: bool,
    ) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Release: ino = {}", ino);

        if ino == STATS_INODE || ino == CONFIG_INODE {
            self.open_virtual_files.remove(&fh);
            return Ok(());
        }

        let prof = OpProf::begin(FuseOpKind::Release, ino);
        prof.mark_backend_start();

        // D1.d (§5.1, PR M5): a never-dirtied open generation skips the
        // lease acquire and the per-close background flush task — there is
        // nothing to flush. Lease/lock/open-count/reclaim bookkeeping
        // below still runs (cheap map ops, all correctness-bearing).
        let clean = !self.handle_dirty(ino);
        if clean {
            METRICS
                .fuse_release_clean_fastpath
                .fetch_add(1, Ordering::Relaxed);
        } else {
            // Non-blocking: schedule layout/active flush in background so
            // close is cheap. fsync still waits. Staging mmap retains data
            // for same-session reads.
            if let Ok(fencing_token) = self.get_or_acquire_lease(ino).await {
                let fs = self.clone();
                crate::bg_admit::spawn_bg(async move {
                    let _ = fs.flush_memory_buffers_for_inode(ino, fencing_token).await;
                    let _ = fs.flush_active_blocks_with_retry(ino, fencing_token).await;
                    let file_path = crate::keys::inode_path(ino);
                    let _ = fs
                        .router
                        .persist_dirty_layout_if_needed(&file_path, fencing_token)
                        .await;
                });
            }

            if let Err(e) = self.complete_active_multipart_upload_if_any(ino).await {
                error!(
                    "FUSE Release: Failed to complete multipart upload for inode {}: {:?}",
                    ino, e
                );
            }
        }

        // Drop the shared op lease only at the LAST close (FIND-RW5-A face
        // 3): releasing it while other handles were open let the next
        // acquisition bump the fencing token and fence every in-flight op
        // still holding the old snapshot into EIO — the generic/464
        // FencingTokenExpired{N, N+1} storm (16 procs sharing 200 files,
        // every close detonating its siblings' writes). The open count
        // decrements FIRST so "last close" is exact for this release.
        self.remove_open(ino);
        if !self.is_open(ino) {
            if let Some((_, lease)) = self.active_leases.remove(&ino) {
                let _ = lease.release().await;
            }
        }

        // POSIX locks are kernel-local (no daemon table): close-time lock
        // release is the kernel's posix_lock_file bookkeeping.
        prof.mark_backend_done();

        // Static lock array does not need dynamic cleanup

        self.queue_reclaim_inode(ino);

        Ok(())
    }

    async fn fsync(&self, _req: Request, ino: u64, _fh: u64, _datasync: bool) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Fsync: ino = {}, datasync = {}", ino, _datasync);

        if ino == STATS_INODE || ino == CONFIG_INODE {
            return Ok(());
        }

        // P0-3: durable ops must not mask backend write failures.
        let prof = OpProf::begin(FuseOpKind::Fsync, ino);
        prof.mark_backend_start();
        let fencing_token = self
            .get_or_acquire_lease(ino)
            .await
            .map_err(map_squeezefs_err)?;

        // Single path: memory → active flush → dirty layout, then ONE meta barrier.
        // `flush_inode_to_backend` already issues `sync_device_for_ino` for this
        // inode's volume; a second barrier here flushed nothing new (redundant
        // fdatasync = ~2x small-file fsync latency), so it is intentionally gone.
        // (The former FORCE_SYNC_TX scope here was verified inert end-to-end and
        // deleted in design-wal-crash-consistency PR 4 §4.3: no metadata
        // transaction ever executed inside this scope — the layout/size persist
        // is deliberately non-transactional — so its single reader never
        // observed `true`. Zero fsync behavior change; the single-barrier
        // suites are the regression guard.)
        // fsync's OWN synchronous flush of the staged/parked blocks is the
        // durable error surface (never-lossy writeback: a failed background
        // unit keeps its bytes in staging and is retried forever, so a
        // sticky per-ino poison map would report errors for data that is
        // safe — and did: the multi-volume bench-suite EIO cascade).
        if let Err(e) = self.flush_inode_to_backend(ino, fencing_token).await {
            error!("FUSE Fsync failed for ino {}: {:?}", ino, e);
            return Err(map_squeezefs_err(e));
        }
        prof.mark_backend_done();

        Ok(())
    }

    async fn fsyncdir(&self, _req: Request, ino: u64, _fh: u64, _datasync: bool) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Fsyncdir: ino = {}", ino);
        if let Some(backend) = self.meta_backend.as_ref() {
            backend
                .sync_all_devices()
                .await
                .map_err(map_squeezefs_err)?;
        }
        Ok(())
    }

    async fn fallocate(
        &self,
        req: Request,
        ino: u64,
        _fh: u64,
        offset: u64,
        length: u64,
        mode: u32,
    ) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        // VL8 item 2: register BEFORE the inode guard (watchdog visibility).
        let _prof = OpProf::begin(FuseOpKind::Fallocate, ino);
        debug!(
            "FUSE Fallocate: ino = {}, offset = {}, length = {}, mode = {}",
            ino, offset, length, mode
        );
        // D1.d: every supported fallocate mode can mutate data or size —
        // dirty the open generation (conservative for pure KEEP_SIZE
        // preallocation, which our sparse backend treats as a no-op).
        self.mark_handle_dirty(ino);
        const PUNCH_HOLE: u32 = libc::FALLOC_FL_PUNCH_HOLE as u32;
        const KEEP_SIZE: u32 = libc::FALLOC_FL_KEEP_SIZE as u32;
        const ZERO_RANGE: u32 = libc::FALLOC_FL_ZERO_RANGE as u32;
        const COLLAPSE_RANGE: u32 = libc::FALLOC_FL_COLLAPSE_RANGE as u32;
        const INSERT_RANGE: u32 = libc::FALLOC_FL_INSERT_RANGE as u32;

        // COLLAPSE_RANGE / INSERT_RANGE shift file contents (not a hole op).
        // They are not implemented; reject them loudly so callers (and fsx)
        // fall back instead of silently corrupting via the extend path below.
        if mode & (COLLAPSE_RANGE | INSERT_RANGE) != 0 {
            return Err(Errno::from(libc::EOPNOTSUPP));
        }

        // u32 block-index representability (see `max_file_size`) — EFBIG
        // past the cap, like write/truncate (generic/525).
        if offset.saturating_add(length) > self.max_file_size() {
            return Err(Errno::from(libc::EFBIG));
        }

        // PUNCH_HOLE / ZERO_RANGE: the range must read back as zeros (a hole).
        // PUNCH_HOLE always keeps the size; ZERO_RANGE may grow it when
        // KEEP_SIZE is clear. Zero the in-bounds portion durably, then extend.
        if mode & (PUNCH_HOLE | ZERO_RANGE) != 0 {
            if length == 0 {
                return Ok(());
            }
            // Lock order (P1-9): inode write guard (1) BEFORE the lease (2).
            let _guard = self.active_inode_locks.get_inode_lock(ino).write().await;
            let fencing_token = self
                .get_or_acquire_lease(ino)
                .await
                .map_err(map_squeezefs_err)?;

            // Zero the part that overlaps existing data (punch_hole_range clamps
            // to the current EOF); any region past EOF becomes a hole via the
            // size extension below and already reads zeros.
            self.punch_hole_range(ino, offset, length, fencing_token)
                .await
                .map_err(map_squeezefs_err)?;

            if mode & ZERO_RANGE != 0 && mode & KEEP_SIZE == 0 {
                let target_size = offset + length;
                self.extend_file_size(ino, target_size, fencing_token)
                    .await
                    .map_err(map_squeezefs_err)?;
            }
            self.strip_setid_after_unprivileged_datamod(&req, ino)
                .await?;
            return Ok(());
        }

        // Pre-allocation isn't strictly required to reserve physical space in our NVMe-oF backend volume
        // as blocks are sparse/dynamic by nature. We just update the size attribute if we are extending.
        if mode & libc::FALLOC_FL_KEEP_SIZE as u32 == 0 {
            // Serialize against writes/truncates (lock order 1) so the
            // freshest-size gate inside extend_file_size cannot race a
            // concurrent size change.
            let _guard = self.active_inode_locks.get_inode_lock(ino).write().await;
            let fencing_token = self.dlm.get_fencing_token_ino(ino);
            let target_size = offset + length;
            self.extend_file_size(ino, target_size, fencing_token)
                .await
                .map_err(map_squeezefs_err)?;
        }

        self.strip_setid_after_unprivileged_datamod(&req, ino)
            .await?;
        Ok(())
    }

    async fn forget(&self, _req: Request, ino: u64, count: u64) {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Forget: ino = {}, count = {}", ino, count);
        let _prof = OpProf::begin(FuseOpKind::Forget, ino);
        self.attr_cache.invalidate(&ino);
        self.active_inode_locks.remove(&ino);
        // Reclaim inodes that reached nlink==0 while still open (unlink/14.t).
        self.queue_reclaim_inode(ino);
    }

    /// BATCH_FORGET (kernel mass evictions: memory pressure, drop_caches,
    /// pre-umount sweeps) must behave exactly like N FORGETs. fuse3's
    /// default impl is a NO-OP — leaving this unimplemented leaked every
    /// batch-evicted orphan's inode slot until the next mount's
    /// reconciliation.
    async fn batch_forget(&self, _req: Request, inodes: &[u64]) {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE BatchForget: {} inodes", inodes.len());
        let _prof = OpProf::begin(FuseOpKind::Forget, inodes.first().copied().unwrap_or(0));
        for &ino in inodes {
            self.attr_cache.invalidate(&ino);
            self.active_inode_locks.remove(&ino);
            self.queue_reclaim_inode(ino);
        }
    }

    // POSIX byte-range locks are KERNEL-LOCAL: the INIT reply never
    // advertises FUSE_POSIX_LOCKS (see the fuse3 negotiation note +
    // pins), so the kernel's canonical posix_lock_file arbitrates and
    // no GETLK/SETLK ever reaches this daemon. The former
    // daemon-arbitrated table could not satisfy the full surface
    // (unlock-on-close rode the FLUSH lock_owner that the clean-handle
    // ENOSYS latch elides; OFD owners don't cross the wire; /proc/locks
    // shows only kernel-tracked locks — fstests generic/131/478/504).
    // Intra-mount arbitration is the whole requirement under the D0
    // single-writer mount guard.

    async fn ioctl(
        &self,
        _req: Request,
        inode: u64,
        _fh: u64,
        flags: u32,
        cmd: u32,
        _arg: u64,
        _in_size: u32,
        _out_size: u32,
    ) -> FuseResult<ReplyIoctl> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!(
            "FUSE ioctl: inode = {}, cmd = {}, flags = {}",
            inode, cmd, flags
        );

        match cmd {
            SQUEEZEFS_IOC_GDS_READ => {
                // 1. Read GdsReadArgs from client process memory
                let pid = _req.pid;
                let arg = _arg;
                let args_res = tokio::task::spawn_blocking(move || {
                    let mut bytes = [0u8; std::mem::size_of::<GdsReadArgs>()];
                    use std::os::unix::fs::FileExt;
                    let mem_file =
                        std::fs::File::open(format!("/proc/{}/mem", pid)).map_err(|e| {
                            error!(
                                "GDS ioctl: failed to open client memory file for pid {}: {:?}",
                                pid, e
                            );
                            Errno::from(libc::EFAULT)
                        })?;
                    mem_file.read_exact_at(&mut bytes, arg).map_err(|e| {
                        error!(
                            "GDS ioctl: failed to read client memory at 0x{:X}: {:?}",
                            arg, e
                        );
                        Errno::from(libc::EFAULT)
                    })?;
                    let args: GdsReadArgs =
                        unsafe { std::ptr::read(bytes.as_ptr() as *const GdsReadArgs) };
                    Ok::<GdsReadArgs, Errno>(args)
                })
                .await;

                let args = match args_res {
                    Ok(Ok(a)) => a,
                    Ok(Err(e)) => return Err(e),
                    Err(_) => return Err(Errno::from(libc::EIO)),
                };
                debug!("GDS ioctl args: {:?}", args);

                // 2. Lock inode
                let lock = self.get_inode_lock(inode);
                let _guard = lock.read().await;

                // 3. Fetch metadata
                let file_path = format!("inode_{}", inode);
                let meta = match self.router.fetch_metadata(&file_path).await {
                    Ok(m) => m,
                    Err(e) => {
                        error!(
                            "GDS ioctl: failed to fetch metadata for inode {}: {:?}",
                            inode, e
                        );
                        return Err(map_squeezefs_err(e));
                    }
                };

                if meta.file_type != "striped" {
                    error!(
                        "GDS ioctl: only striped files support GDS, got layout: {}",
                        meta.file_type
                    );
                    return Err(Errno::from(libc::EINVAL));
                }

                let block_size = self.router.block_size.load(Ordering::Relaxed);
                let file_size = meta.size;

                if args.offset >= file_size {
                    return Ok(ReplyIoctl {
                        result: 0,
                        flags: 0,
                        in_iovs: 0,
                        out_iovs: 0,
                    });
                }

                let end_offset = std::cmp::min(args.offset + args.size, file_size);
                let start_block = (args.offset / block_size) as u32;
                let end_block = ((end_offset - 1) / block_size) as u32;

                let block_keys = match self
                    .router
                    .load_striped_block_keys(&file_path, &meta, start_block, end_block)
                    .await
                {
                    Ok(keys) => keys,
                    Err(e) => {
                        error!("GDS ioctl: failed to load block keys: {:?}", e);
                        return Err(map_squeezefs_err(e));
                    }
                };

                for (b_idx, key_opt) in block_keys {
                    let b_key = match key_opt {
                        Some(key) => key,
                        None => continue, // Sparse hole
                    };

                    let b_start_offset = b_idx as u64 * block_size;
                    let b_end_offset = b_start_offset + block_size;

                    let read_start = std::cmp::max(args.offset, b_start_offset);
                    let read_end = std::cmp::min(end_offset, b_end_offset);
                    let block_read_offset = read_start - b_start_offset;
                    let block_read_size = (read_end - read_start) as usize;

                    let dest_vram_address = args.vram_address + (read_start - args.offset);

                    if let Err(e) = self
                        .router
                        .cache
                        .gds
                        .read_direct(
                            &b_key,
                            dest_vram_address,
                            block_read_offset,
                            block_read_size,
                            &self.router,
                        )
                        .await
                    {
                        error!(
                            "GDS ioctl: read_direct failed for block {} key {}: {:?}",
                            b_idx, b_key, e
                        );
                        return Err(map_squeezefs_err(e));
                    }
                }

                Ok(ReplyIoctl {
                    result: 0,
                    flags: 0,
                    in_iovs: 0,
                    out_iovs: 0,
                })
            }
            _ => match cmd as u64 {
                libc::FS_IOC_GETFLAGS => Err(Errno::from(libc::ENOTTY)),
                libc::FS_IOC_SETFLAGS => Err(Errno::from(libc::ENOTTY)),
                _ => Err(Errno::from(libc::ENOTTY)),
            },
        }
    }

    async fn setxattr(
        &self,
        _req: Request,
        inode: Inode,
        name: &OsStr,
        value: &[u8],
        _flags: u32,
        _position: u32,
    ) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        let name_str = match name.to_str() {
            Some(s) => s,
            None => return Err(Errno::from(libc::EINVAL)),
        };
        // Reserved-namespace screen (design-volume-lifecycle §5.1.2,
        // generalizing the L4 BOOTSTRAP_XATTR rule): internal records —
        // job fabric bookkeeping, the format config, anything under
        // `user.squeezefs.` — never cross the FUSE boundary. Without
        // this, any user could FORGE or DELETE the durable lease/
        // fencing bookkeeping of an in-flight drain from an
        // unprivileged shell (the format config was already exposed
        // pre-VL2 — a real hole, pinned closed by tests).
        if posix_acl_xattr_name(name_str) {
            return Err(Errno::from(libc::EOPNOTSUPP));
        }
        if reserved_xattr_name(name_str) {
            METRICS
                .fuse_reserved_xattr_refusals
                .fetch_add(1, Ordering::Relaxed);
            return Err(Errno::from(libc::EPERM));
        }
        let backend = self
            .meta_backend
            .as_ref()
            .expect("meta_backend must be configured");
        backend
            .setxattr(inode, name_str, value)
            .await
            .map_err(map_squeezefs_err)?;
        Ok(())
    }

    async fn getxattr(
        &self,
        _req: Request,
        inode: Inode,
        name: &OsStr,
        size: u32,
    ) -> FuseResult<fuse3::raw::reply::ReplyXAttr> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        let name_str = match name.to_str() {
            Some(s) => s,
            None => return Err(Errno::from(libc::EINVAL)),
        };

        // L4 bootstrap virtual xattr (design-preload-interception §5.2):
        // synthesized ONLY when this mount's session host is armed —
        // rides the already-authorized kernel path (read access to the
        // file ⇒ may read it). Disabled mounts answer ENODATA (the cheap
        // negative probe); the reserved name never serves on-disk bytes.
        if name_str == squeezefs_ipc::wire::BOOTSTRAP_XATTR {
            let host = self.ipc_host.load();
            let Some(host) = host.as_ref() else {
                return Err(Errno::from(libc::ENODATA));
            };
            let blob = host.bootstrap_blob();
            if size == 0 {
                return Ok(fuse3::raw::reply::ReplyXAttr::Size(blob.len() as u32));
            }
            if size < blob.len() as u32 {
                return Err(Errno::from(libc::ERANGE));
            }
            return Ok(fuse3::raw::reply::ReplyXAttr::Data(blob.into()));
        }
        // Reserved-namespace screen (§5.1.2): internal record bytes
        // never serve through FUSE (daemon/probe paths read the meta
        // backend directly). The bootstrap name above is the one
        // deliberate exception — synthesized, never on-disk bytes.
        if posix_acl_xattr_name(name_str) {
            return Err(Errno::from(libc::EOPNOTSUPP));
        }
        if reserved_xattr_name(name_str) {
            METRICS
                .fuse_reserved_xattr_refusals
                .fetch_add(1, Ordering::Relaxed);
            return Err(Errno::from(libc::EPERM));
        }

        let backend = self
            .meta_backend
            .as_ref()
            .expect("meta_backend must be configured");
        let value = backend
            .getxattr(inode, name_str)
            .await
            .map_err(map_squeezefs_err)?;
        if let Some(v) = value {
            if size == 0 {
                return Ok(fuse3::raw::reply::ReplyXAttr::Size(v.len() as u32));
            }
            if size < v.len() as u32 {
                return Err(Errno::from(libc::ERANGE));
            }
            Ok(fuse3::raw::reply::ReplyXAttr::Data(v.into()))
        } else {
            #[cfg(target_os = "macos")]
            return Err(Errno::from(libc::ENOATTR));
            #[cfg(not(target_os = "macos"))]
            return Err(Errno::from(libc::ENODATA));
        }
    }

    async fn listxattr(
        &self,
        _req: Request,
        inode: Inode,
        size: u32,
    ) -> FuseResult<fuse3::raw::reply::ReplyXAttr> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        let backend = self
            .meta_backend
            .as_ref()
            .expect("meta_backend must be configured");
        let keys = backend.listxattr(inode).await.map_err(map_squeezefs_err)?;
        let mut data = Vec::new();
        for key in keys {
            // Reserved names are filtered unconditionally (§5.1.2 screen,
            // subsuming the L4 bootstrap rule): internal records are
            // invisible through FUSE.
            if reserved_xattr_name(&key) {
                continue;
            }
            data.extend_from_slice(key.as_bytes());
            data.push(0);
        }
        if size == 0 {
            return Ok(fuse3::raw::reply::ReplyXAttr::Size(data.len() as u32));
        }
        if size < data.len() as u32 {
            return Err(Errno::from(libc::ERANGE));
        }
        Ok(fuse3::raw::reply::ReplyXAttr::Data(data.into()))
    }

    async fn removexattr(&self, _req: Request, inode: Inode, name: &OsStr) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        let name_str = match name.to_str() {
            Some(s) => s,
            None => return Err(Errno::from(libc::EINVAL)),
        };
        // Reserved-namespace screen (§5.1.2). Pre-VL2 this handler had
        // NO screen at all — `removexattr("user.squeezefs.format_config")`
        // deleted the durable format config from any unprivileged shell.
        if posix_acl_xattr_name(name_str) {
            return Err(Errno::from(libc::EOPNOTSUPP));
        }
        if reserved_xattr_name(name_str) {
            METRICS
                .fuse_reserved_xattr_refusals
                .fetch_add(1, Ordering::Relaxed);
            return Err(Errno::from(libc::EPERM));
        }
        let backend = self
            .meta_backend
            .as_ref()
            .expect("meta_backend must be configured");
        backend
            .removexattr(inode, name_str)
            .await
            .map_err(map_squeezefs_err)?;
        Ok(())
    }
}

/// §5.1.2 reserved-namespace predicate (design-volume-lifecycle): names
/// that carry SqueezeFS-internal durable records — the job fabric's
/// `job:` family, everything under `user.squeezefs.` (format config,
/// the L4 bootstrap name, future internal records) — never cross the
/// FUSE boundary in either direction. The daemon, offline probes, and
/// admin verbs read them through the meta backend directly.
fn reserved_xattr_name(name: &str) -> bool {
    name.starts_with(crate::jobs::JOB_XATTR_PREFIX) || name.starts_with("user.squeezefs.")
}

/// POSIX ACL xattrs refuse ENOTSUP (the no-ACL filesystem class —
/// fstests generic/099/319, VL10 release gate): SqueezeFS implements no
/// ACL semantics (no mode↔ACL_USER_OBJ/mask sync, no default-ACL
/// inheritance, no enforcement beyond mode bits), and STORING the
/// xattrs made every tool believe otherwise. setfacl fails loud;
/// fstests' `_require_acls` notruns; `-o default_permissions` mode-bit
/// enforcement is unaffected. Pinned in tests/job_fabric_tests.rs.
fn posix_acl_xattr_name(name: &str) -> bool {
    name == "system.posix_acl_access" || name == "system.posix_acl_default"
}

/// Initialize the multi-threaded work-stealing tokio runtime
/// with threads pinned strictly to physical cores, keeping one core free.
pub fn init_runtime() -> tokio::runtime::Runtime {
    let physical_cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    // Leave at least one core for kernel processing (FUSE filesystem driver, Garnet, networking)
    let worker_threads = std::cmp::max(1, physical_cores - 1);
    info!(
        "FUSE Daemon: Initializing runtime with {} worker threads bound to physical CPU cores.",
        worker_threads
    );

    Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .on_thread_start(|| {
            debug!("Thread started and pinned to physical CPU core.");
        })
        .build()
        .unwrap()
}

/// Historical debug helper — **not** used by production mounts.
///
/// Production FUSE transport is **fuse3** `BlockFuseConnection`: separate read/write
/// `IoUring` rings, optional SQPOLL (`SQUEEZEFS_FUSE_IO_URING_SQPOLL_*`), eventfd
/// completion wakeups, multi-queue `FUSE_DEV_IOC_CLONE` workers, and (P2-8) fixed-file
/// registration of `/dev/fuse` as `types::Fixed(0)` when the kernel allows.
///
/// Prefer that path over this loop, which only demonstrates a bare `Read` on an fd
/// and does not decode FUSE messages.
#[cfg(target_os = "linux")]
#[deprecated(
    note = "Production mounts use fuse3 BlockFuseConnection io_uring; this loop is a no-op debug stub"
)]
pub fn start_io_uring_polling_loop(
    _fuse_fd: std::os::fd::RawFd,
    _runtime: &tokio::runtime::Runtime,
) {
    info!(
        "FUSE Daemon: start_io_uring_polling_loop is deprecated; production I/O uses fuse3 uring rings"
    );
}

pub fn parse_custom_options(opts: &str) -> std::ffi::OsString {
    let mut custom_opts = std::ffi::OsString::new();
    for opt in opts.split(',') {
        let opt_trimmed = opt.trim();
        if !opt_trimmed.is_empty() {
            let key = opt_trimmed.split('=').next().unwrap_or("").trim();
            // TTL options are DAEMON-level (survey P1-C): consumed into
            // `KernelCacheTtls` by `start_mount`, never passed to the
            // kernel mount(2) string (which would reject them). So is the
            // hybrid-I/O `direct_device_true` escape (consumed into the
            // router by `start_mount`).
            if key == "entry_timeout"
                || key == "attr_timeout"
                || key == "negative_timeout"
                || key == "dir_entry_timeout"
                || key == "direct_device_true"
                // L4 daemon-level tokens (KD-11 posture keys): consumed by
                // `resolve_interception_posture`, never kernel options.
                || key == "interception"
                || key == "writeback"
                || key == "writeback_cache"
            {
                continue;
            }
            if !custom_opts.is_empty() {
                custom_opts.push(",");
            }
            custom_opts.push(opt_trimmed);
        }
    }
    custom_opts
}

/// The live mount's `st_dev` from `/proc/self/mountinfo` (field 3
/// `major:minor` of the entry whose mount point matches) — the §5.2 fd
/// screen's device authority, resolved WITHOUT stat'ing our own mount
/// (zero self-FUSE traffic). `None` until the mount is visible.
fn mount_st_dev(mount_path: &Path) -> Option<u64> {
    let want = mount_path.canonicalize().ok()?;
    let data = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    for line in data.lines() {
        let mut fields = line.split_whitespace();
        let _mount_id = fields.next()?;
        let _parent_id = fields.next()?;
        let devs = fields.next()?;
        let _root = fields.next()?;
        let mount_point = fields.next()?;
        // mountinfo octal-escapes spaces/tabs in paths.
        let unescaped = mount_point.replace("\\040", " ").replace("\\011", "\t");
        if std::path::Path::new(&unescaped) == want {
            let (maj, min) = devs.split_once(':')?;
            let (maj, min): (u32, u32) = (maj.parse().ok()?, min.parse().ok()?);
            return Some(libc::makedev(maj, min));
        }
    }
    None
}

/// The POSIX mount security tokens `(nosuid, nodev, noexec)` in a
/// `-o` option string. These are MOUNT FLAGS (`MS_NOSUID`/`MS_NODEV`/
/// `MS_NOEXEC`), not FUSE data options — the pre-fix plumbing dropped
/// them entirely, so `mount -o nosuid` produced a suid-honoring mount
/// (fstests generic/128; pinned in tests/mount_preflight_tests.rs).
pub fn mount_security_flags(opts: &str) -> (bool, bool, bool) {
    let (mut nosuid, mut nodev, mut noexec) = (false, false, false);
    for opt in opts.split(',') {
        match opt.trim() {
            "nosuid" => nosuid = true,
            "nodev" => nodev = true,
            "noexec" => noexec = true,
            _ => {}
        }
    }
    (nosuid, nodev, noexec)
}

pub fn filter_kernel_mount_options(opts: &str) -> String {
    let mut kernel_opts = Vec::new();
    for opt in opts.split(',') {
        let opt_trimmed = opt.trim();
        if !opt_trimmed.is_empty() {
            let key = opt_trimmed.split('=').next().unwrap_or("").trim();
            if key == "max_read"
                || key == "blksize"
                || key == "default_permissions"
                || key == "allow_other"
            {
                kernel_opts.push(opt_trimmed);
            }
        }
    }
    kernel_opts.join(",")
}

/// Resolved interception/writeback posture for one mount (KD-11,
/// design-preload-interception §5.6.2).
#[derive(Debug, Clone, Copy)]
pub struct InterceptionPosture {
    /// The session host arms for this mount.
    pub interception: bool,
    /// The kernel writeback-cache flag actually sent in the INIT reply.
    pub write_back: bool,
}

/// KD-11 (normative): `-o interception` (or the CLI flag / SQUEEZEFS_IPC=1)
/// **forces kernel write-through** on that mount — the default-on kernel
/// writeback cache acks buffered writes the daemon has not seen, and a
/// ring read (direct-to-daemon by construction) would miss them. An
/// explicit writeback request combined with interception is a
/// contradiction and refuses LOUD; explicit writeback without
/// interception is honored over the mount default. Pure function — the
/// mount path feeds it (options, flag, env) and applies the result at the
/// `options.write_back` site.
pub fn resolve_interception_posture(
    custom_opts: Option<&str>,
    cli_flag: bool,
    env_flag: bool,
    writeback_default: bool,
) -> Result<InterceptionPosture, String> {
    let mut opt_interception = false;
    let mut explicit_writeback = false;
    if let Some(opts) = custom_opts {
        for opt in opts.split(',') {
            match opt.trim() {
                "interception" => opt_interception = true,
                // Both historical spellings of an explicit kernel
                // writeback-cache request.
                "writeback" | "writeback_cache" => explicit_writeback = true,
                _ => {}
            }
        }
    }
    let interception = opt_interception || cli_flag || env_flag;
    if interception && explicit_writeback {
        return Err(
            "mount option conflict: `-o interception` forces kernel write-through \
             (writeback cache off — KD-11, docs/design-preload-interception.md §5.6.2); \
             an explicit writeback/writeback_cache request cannot be combined with \
             interception. Drop one of the two options."
                .to_string(),
        );
    }
    let write_back = if interception {
        false
    } else if explicit_writeback {
        true
    } else {
        writeback_default
    };
    Ok(InterceptionPosture {
        interception,
        write_back,
    })
}

/// Start FUSE mount daemon using fuse3.
pub async fn start_mount<P: AsRef<Path>>(
    mountpoint: P,
    mut fs: SqueezefsFilesystem,
    uid: u32,
    gid: u32,
    writeback: bool,
    allow_other: bool,
    custom_opts: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Survey P1-C: consume operator TTL overrides (`-o attr_timeout=…`,
    // `entry_timeout=`, `dir_entry_timeout=`, `negative_timeout=`) into
    // the per-mount TTL config. Pre-M5 these keys were silently DROPPED;
    // they are still stripped from the kernel option string below (they
    // are daemon-level), but their values now take effect.
    if let Some(ref opts) = custom_opts {
        fs.kernel_ttls = fs.kernel_ttls.with_mount_options(opts);
        info!(
            "Kernel cache TTLs for this mount: attr {:?}, entry {:?}, dir-entry {:?}, negative {:?}",
            fs.kernel_ttls.attr,
            fs.kernel_ttls.entry,
            fs.kernel_ttls.dir_entry,
            fs.kernel_ttls.negative
        );
    }

    // KD-11: resolve the interception/writeback posture BEFORE any option
    // is applied — `-o interception` / SQUEEZEFS_IPC=1 forces kernel
    // write-through; explicit writeback + interception refuses loud.
    // (The CLI `--interception` flag arrives merged into the option
    // string by `main.rs`.)
    let env_ipc = std::env::var("SQUEEZEFS_IPC").is_ok_and(|v| v == "1");
    let posture = resolve_interception_posture(custom_opts.as_deref(), false, env_ipc, writeback)
        .map_err(|e| -> Box<dyn std::error::Error> {
        error!("{e}");
        e.into()
    })?;
    if posture.interception && writeback && !posture.write_back {
        info!(
            "Interception mount: kernel writeback cache forced OFF (write-through — \
             KD-11, docs/design-preload-interception.md §5.6.2)"
        );
    }

    let mut options = MountOptions::default();
    let is_root = unsafe { libc::getuid() } == 0;
    if is_root {
        options.uid(uid);
        options.gid(gid);
    }
    options.allow_other(allow_other);
    options.write_back(posture.write_back);
    options.default_permissions(true);
    if let Some(ref opts) = custom_opts {
        // MS_NOSUID/MS_NODEV/MS_NOEXEC ride the mount(2) flags (root
        // path) / the fusermount option string (unprivileged path) —
        // fstests generic/128.
        let (nosuid, nodev, noexec) = mount_security_flags(opts);
        options.nosuid(nosuid);
        options.nodev(nodev);
        options.noexec(noexec);
    }

    if let Some(ref opts) = custom_opts {
        for opt in opts.split(',') {
            let opt_trimmed = opt.trim();
            if !opt_trimmed.is_empty() {
                // Hybrid I/O diagnostic escape (user directive 2026-07-15):
                // `-o direct_device_true` restores strictly device-true
                // O_DIRECT reads (no tier serve, no admission) — the
                // measurement/diagnostic posture for the `.benchmarks`
                // amplification methodology. Daemon-level: stripped from
                // the kernel option strings like the TTL keys.
                if opt_trimmed == "direct_device_true" {
                    fs.router.set_direct_device_true(true);
                    info!(
                        "Hybrid I/O escape armed (-o direct_device_true): O_DIRECT reads \
                         bypass the read tiers — no serve, no admission (device-true \
                         diagnostic/measurement mode)"
                    );
                }
                let parts: Vec<&str> = opt_trimmed.splitn(2, '=').collect();
                if parts.len() == 2 {
                    let key = parts[0].trim();
                    let val = parts[1].trim();
                    if key == "fsname" {
                        options.fs_name(val);
                    }
                    // L1 (IOPS-parity program): `-o max_background=` /
                    // `-o congestion_threshold=` are DAEMON-level INIT-reply
                    // overrides (pre-L1 they were dead letters — filtered
                    // from the kernel mount string and never reaching the
                    // INIT reply, which hardcoded 12/9). Unset, the policy
                    // default is clamp(queues × depth, 64, 256) and ¾ of it.
                    if key == "max_background" {
                        match val.parse::<u16>() {
                            Ok(v) if v > 0 => {
                                options.max_background(v);
                            }
                            _ => warn!("ignoring invalid -o max_background={val}"),
                        }
                    }
                    if key == "congestion_threshold" {
                        match val.parse::<u16>() {
                            Ok(v) if v > 0 => {
                                options.congestion_threshold(v);
                            }
                            _ => warn!("ignoring invalid -o congestion_threshold={val}"),
                        }
                    }
                }
            }
        }
    }

    // L1 payload-arena budget: an eighth of the §5.7-resolved memory
    // budget, ceilinged at 2 GiB. The FUSE-over-io_uring geometry resolver
    // degrades per-queue ring depth from the desired 32 toward the pre-L1
    // floor of 4 to fit under this cap — small-RAM boxes keep yesterday's
    // footprint, everything else ships the measured 316k-IOPS geometry by
    // default.
    let mem_budget = crate::mem_budget::MEM_BUDGET.resolve_budget_now();
    let transport_cap = crate::mem_budget::transport_buffer_cap(mem_budget);
    options.transport_buffer_cap_bytes(transport_cap);
    info!(
        "Transport payload-buffer cap: {} MiB (memory budget {} MiB)",
        transport_cap / (1024 * 1024),
        mem_budget / (1024 * 1024)
    );

    // L4 interception session host (PR L4-3): armed pre-mount so the
    // bootstrap xattr synthesizes from the first request. Data-plane
    // serves land in PR L4-4; this host is the §5.2 control plane.
    // The W1 notify handle (PR L4-6) exists only post-mount — this cell
    // bridges the gap (fires before it fills are skipped by the hook).
    let ipc_notify_cell: Option<std::sync::Arc<arc_swap::ArcSwap<Option<fuse3::notify::Notify>>>>;
    // VL2 (design-volume-lifecycle §5.1.4): the host arms on EVERY
    // mount. Without `-o interception` it is control-plane-only —
    // listener + ctl threads + the ADMIN lane; data-plane HELLOs
    // refuse before any fd screen, so no shm sessions, no arenas, no
    // service dispatch exist on a default mount.
    {
        let arena_mb = std::env::var("SQUEEZEFS_IPC_ARENA_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(64);
        let max_op_bytes = std::env::var("SQUEEZEFS_IPC_MAX_OP_BYTES")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(squeezefs_ipc::layout::DEFAULT_MAX_OP_BYTES);
        let geometry = squeezefs_ipc::layout::Geometry {
            arena_bytes: arena_mb * 1024 * 1024,
            max_op_bytes,
            ..squeezefs_ipc::layout::Geometry::default_v1()
        };
        // Session-shm admission cap: min(budget/8, 2 GiB), env-overridable
        // (SQUEEZEFS_IPC_MEM_MAX, MiB) — the R5 `ipc_session_arenas`
        // component bound (design §5.7).
        let arena_cap_bytes = std::env::var("SQUEEZEFS_IPC_MEM_MAX")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|mb| mb * 1024 * 1024)
            .unwrap_or_else(|| crate::mem_budget::ipc_arena_cap(mem_budget));
        // OQ-6 (v1.1): the path-socket runtime dir for container-netns
        // clients. `SQUEEZEFS_IPC_SOCKET_DIR` overrides (the literal
        // `none` disables); default = /run/squeezefs for root mounts,
        // $XDG_RUNTIME_DIR/squeezefs else /tmp/squeezefs-il-<uid> for
        // user mounts. Container fleets bind-mount this dir alongside
        // the filesystem (documented in operations.md); bind failure
        // degrades loudly to abstract-only inside the host.
        let socket_dir = match std::env::var("SQUEEZEFS_IPC_SOCKET_DIR") {
            Ok(v) if v.trim() == "none" => None,
            Ok(v) if !v.trim().is_empty() => Some(std::path::PathBuf::from(v.trim())),
            _ => {
                // SAFETY: geteuid is trivially safe.
                let euid = unsafe { libc::geteuid() };
                Some(if euid == 0 {
                    std::path::PathBuf::from("/run/squeezefs")
                } else {
                    std::env::var("XDG_RUNTIME_DIR")
                        .ok()
                        .filter(|v| !v.trim().is_empty())
                        .map(|v| std::path::PathBuf::from(v).join("squeezefs"))
                        .unwrap_or_else(|| {
                            std::path::PathBuf::from(format!("/tmp/squeezefs-il-{euid}"))
                        })
                })
            }
        };
        let cfg = crate::ipc_host::IpcHostConfig {
            socket_name: format!("sqz-il0-{}-{:08x}", std::process::id(), fastrand::u32(..)),
            socket_dir,
            build_commit: crate::version::build_commit(),
            allow_dev: std::env::var("SQUEEZEFS_IPC_ALLOW_DEV").is_ok_and(|v| v == "1"),
            geometry,
            arena_cap_bytes,
            per_uid_session_cap: 64,
            // §5.7 idle reap (PR L4-6), default 300 s; 0 disables.
            idle_secs: std::env::var("SQUEEZEFS_IPC_IDLE_SECS")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(300),
            // VL2: data plane only with `-o interception` (KD-11 posture
            // unchanged); the ADMIN lane serves either way.
            data_plane: posture.interception,
            owner_uid: {
                let (uid, _gid) = crate::config_ops::invoking_owner();
                uid
            },
        };
        // PR L4-4: the real data plane — fast path + async handoff over
        // THIS filesystem instance (the same daemon state kernel requests
        // reach; coherence is structural, §5.6.2). PR L4-6 adds the W1
        // invalidator: the hook pushes FUSE_NOTIFY_INVAL_INODE through
        // the fuse3 Notify handle, captured post-mount into this cell
        // (pre-mount fires are skipped — no kernel cache exists yet).
        let inval_window_ms = std::env::var("SQUEEZEFS_IPC_INVAL_WINDOW_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(1000);
        let notify_cell: std::sync::Arc<arc_swap::ArcSwap<Option<fuse3::notify::Notify>>> =
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(None));
        ipc_notify_cell = Some(notify_cell.clone());
        let hook_runtime = tokio::runtime::Handle::current();
        let hook: std::sync::Arc<dyn Fn(u64) + Send + Sync> = std::sync::Arc::new(move |ino| {
            if let Some(notify) = notify_cell.load().as_ref() {
                let notify = notify.clone();
                // Whole-inode shootdown: attrs + the full page range
                // (off 0, len -1) — the kernel refetches size and data.
                hook_runtime.spawn(async move {
                    notify.invalid_inode(ino, 0, -1).await;
                });
            }
        });
        let sink = std::sync::Arc::new(crate::ipc_service::DataPlaneSink::with_invalidator(
            fs.clone(),
            hook,
            inval_window_ms,
        ));
        match crate::ipc_host::IpcHost::spawn(cfg, sink) {
            Ok(host) => {
                crate::mem_budget::register_ipc_session_arena_component(
                    &crate::mem_budget::MEM_BUDGET,
                    std::sync::Arc::new(|| METRICS.ipc_arena_bytes.load(Ordering::Relaxed)),
                    std::sync::Arc::new({
                        let host = host.clone();
                        move |target| host.shed_to(target)
                    }),
                );
                info!(
                    "IPC session host armed (socket {}, session shm cap {} MiB)",
                    host.socket_name(),
                    arena_cap_bytes / (1024 * 1024)
                );
                // VL2: the ADMIN lane serves the job fabric when the
                // mount wired one (main.rs creates it right after the
                // meta backend exists, before start_mount).
                if let Some(fabric) = fs.job_fabric.load().as_ref() {
                    host.set_admin_sink(std::sync::Arc::new(
                        // VL3: the fs handle serves the volume verbs
                        // (add-data / list / health overrides) beside the
                        // fabric's job verbs.
                        crate::ipc_service::FabricAdminSink::with_fs(
                            fabric.clone(),
                            std::sync::Arc::new(fs.clone()),
                        ),
                    ));
                    // R5: the `job_copy_buffers` component (§5.1.5) —
                    // sheddable; pauses jobs loudly under pressure.
                    let shed_fabric = fabric.clone();
                    crate::mem_budget::MEM_BUDGET.register(crate::mem_budget::Component::new(
                        "job_copy_buffers",
                        0,
                        1,
                        std::sync::Arc::new(|| {
                            METRICS.job_copy_buffer_bytes.load(Ordering::Relaxed)
                        }),
                        std::sync::Arc::new(move |target| shed_fabric.shed_to(target)),
                    ));
                }
                fs.ipc_host.store(std::sync::Arc::new(Some(host)));
            }
            Err(e) => {
                // Fail the mount loud: `-o interception` was an explicit
                // request — silently mounting without the host would fake
                // the posture (charter rule 4's silent-passthrough class).
                error!("interception session host failed to start: {e}");
                return Err(format!("interception session host failed to start: {e}").into());
            }
        }
    }

    if is_root {
        let filtered_opts = if let Some(ref opts) = custom_opts {
            filter_kernel_mount_options(opts)
        } else {
            "max_read=1048576".to_string()
        };
        options.custom_options(filtered_opts);
    } else {
        if let Some(opts) = custom_opts {
            let parsed = parse_custom_options(&opts);
            options.custom_options(parsed);
        } else {
            // Default custom options. (`max_background`/`congestion_threshold`
            // are NOT mount-string options — they live in the INIT reply and
            // default to the L1 policy above; the historical tokens here were
            // dead letters, filtered before reaching the kernel.)
            options.custom_options(
                "max_read=1048576,max_write=1048576,max_pages=256,max_readahead=4194304,async_read",
            );
        }
    }

    info!(
        "SqueezeFS version {} initializing mount",
        crate::version::version_line()
    );
    info!(
        "FUSE Daemon: Mounting squeezefs at {:?}...",
        mountpoint.as_ref()
    );
    info!("FUSE Daemon: Metadata connection active.");

    let mount_path = mountpoint.as_ref().to_path_buf();

    // Check for stale FUSE mount (ENOTCONN or EIO)
    #[cfg(target_os = "linux")]
    {
        let check_metadata = std::fs::metadata(&mount_path);
        let is_stale = match check_metadata {
            Err(e) => {
                let os_err = e.raw_os_error();
                os_err == Some(107)
                    || os_err == Some(5)
                    || e.kind() == std::io::ErrorKind::NotConnected
            }
            _ => false,
        };

        if is_stale {
            error!(
                "Stale mount point detected at {:?}.\nTo resolve this, run:\n    sudo squeezefs umount {:?}\n    # or: sudo umount -f {:?}\n    # last resort: sudo umount -l {:?}",
                mount_path, mount_path, mount_path, mount_path
            );
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "Transport endpoint is not connected",
            )));
        }
    }

    let _dismount_wait = fs.dismount_wait;
    let nvme_cache = fs.router.cache.nvme.clone();

    let client_id_str = uuid::Uuid::new_v4().to_string();
    *fs.client_id.lock().unwrap() = client_id_str.clone();
    *fs.mountpoint.lock().unwrap() = mount_path.to_string_lossy().to_string();

    // Heartbeat: periodically refresh this client's mount registration so peers
    // can distinguish a live mount from a crashed one. If this process dies
    // ungracefully (kill -9), the heartbeat stops and the registration goes stale
    // after CLIENT_STALE_TTL_SECS, so it no longer blocks `format`.
    //
    // PR M1 (design-metadata-throughput §5.0): the single-writer guard
    // rides the SAME cadence — one staleness law. Each beat refreshes the
    // per-volume `writer_claim` heartbeat and, on PR-capable namespaces,
    // runs the Reservation Report re-check (PTPL-lapse re-acquire /
    // foreign-holder fail-stop / host-identity stability).
    let heartbeat_handle = {
        let fs = fs.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(
                CLIENT_HEARTBEAT_INTERVAL_SECS,
            ));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                fs.refresh_client_registration().await;
                if let Some(mb) = fs.meta_backend.as_ref() {
                    for vol in &mb.volumes {
                        vol.guard_heartbeat().await;
                    }
                }
            }
        })
    };

    // PR 6 / N6 (design-nvmeof-target-management §6.9): the fabric_*
    // controller-state sampler. Rides the SAME cadence constant as the
    // heartbeat (one staleness law; the §6.9 "10 s cadence") but its OWN
    // task, deliberately: a fabric outage stalls the heartbeat loop's
    // guard writes on the dead device, and the storm detector must keep
    // sampling exactly then — sharing that loop would blind it to the
    // outage it exists to observe. Sysfs reads are tiny control-plane
    // file I/O (the sanctioned nvmeof-module precedent), pushed through
    // spawn_blocking to keep the runtime clean.
    let fabric_stats_handle = tokio::spawn(async move {
        let mut sampler = crate::nvmeof::fabric::FabricStatsSampler::default();
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(
            CLIENT_HEARTBEAT_INTERVAL_SECS,
        ));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let controllers = tokio::task::spawn_blocking(|| {
                crate::nvmeof::fabric::enumerate_fabric_controllers(std::path::Path::new(
                    crate::nvmeof::fabric::SYSFS_NVME,
                ))
            })
            .await
            .unwrap_or_default();
            let sample = sampler.observe(&controllers);
            METRICS
                .fabric_controllers
                .store(sample.controllers, Ordering::Relaxed);
            METRICS
                .fabric_ctrl_not_live
                .store(sample.not_live, Ordering::Relaxed);
            if sample.reconnects_observed > 0 {
                METRICS
                    .fabric_ctrl_reconnects
                    .fetch_add(sample.reconnects_observed, Ordering::Relaxed);
                info!(
                    "fabric sampler: {} controller reconnect(s) observed (not_live={} of {})",
                    sample.reconnects_observed, sample.not_live, sample.controllers
                );
            }
        }
    });

    // Spawns the mount loop using fuse3 Session
    let session = fuse3::raw::Session::new(options);

    // PR L4-6: capture the notify handle BEFORE mount() consumes the
    // session; it clones the reply channel, so it stays valid for the
    // mount's lifetime (rides the classical reply path post-arm).
    if let Some(cell) = &ipc_notify_cell {
        cell.store(std::sync::Arc::new(Some(session.get_notify())));
    }
    // The general daemon-side notify (generic/683 setid strip et al).
    fs.kernel_notify
        .store(std::sync::Arc::new(Some(session.get_notify())));

    #[cfg(target_os = "linux")]
    let mut handle = if unsafe { libc::getuid() } == 0 {
        session.mount(fs.clone(), mount_path.clone()).await?
    } else {
        session
            .mount_with_unprivileged(fs.clone(), mount_path.clone())
            .await?
    };

    #[cfg(not(target_os = "linux"))]
    let mut handle = session.mount(fs.clone(), mount_path.clone()).await?;

    if let Some(conn) = handle.connection() {
        fs.session_connection.store(std::sync::Arc::new(Some(conn)));
    }

    // L4 interception: resolve the live mount's `st_dev` for the §5.2 fd
    // screen (screen rule: `st_dev` must equal the mount device; binds
    // refuse class `mode` until this lands — fail-safe). Read from
    // /proc/self/mountinfo, never by stat'ing our own mount (zero
    // self-FUSE traffic).
    if let Some(host) = fs.ipc_host.load().as_ref().as_ref().cloned() {
        let mp = mount_path.clone();
        tokio::spawn(async move {
            for _ in 0..100 {
                let mp_probe = mp.clone();
                let dev = tokio::task::spawn_blocking(move || mount_st_dev(&mp_probe))
                    .await
                    .ok()
                    .flatten();
                if let Some(dev) = dev {
                    host.set_expected_st_dev(dev);
                    info!("IPC session host: mount st_dev resolved ({dev})");
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            error!(
                "IPC session host: mount st_dev NEVER resolved — every bind will \
                 refuse (class mode) until remount; interception is inert on this mount"
            );
        });
    }

    println!("\x1b[92mOK\x1b[0m Squeezefs is ready at {:?}", mount_path);

    let mut should_exit = false;
    while !should_exit {
        let shutdown = async {
            #[cfg(unix)]
            {
                println!("[SIG] Registering signal handlers inside shutdown block...");
                let sigterm_opt =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
                let sigint_opt =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt());
                if let (Ok(mut sigterm), Ok(mut sigint)) = (sigterm_opt, sigint_opt) {
                    println!("[SIG] Signal handlers registered successfully.");
                    loop {
                        tokio::select! {
                            _ = tokio::signal::ctrl_c() => {
                                println!("[SIG] Ctrl+C pressed (ctrl_c future matched)!");
                                eprintln!("\nWARNING: Ctrl+C pressed! If you really want to unmount/exit, hit Ctrl+C again.");
                                tokio::select! {
                                    _ = tokio::signal::ctrl_c() => {
                                        println!("[SIG] Second Ctrl+C, exiting...");
                                        break;
                                    }
                                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {
                                        eprintln!("\nUnmount timeout elapsed. Resuming filesystem...");
                                    }
                                }
                            }
                            _ = sigterm.recv() => {
                                println!("[SIG] Received SIGTERM signal in sigterm.recv()!");
                                break;
                            }
                            _ = sigint.recv() => {
                                println!("[SIG] Received SIGINT signal in sigint.recv()!");
                                eprintln!("\nWARNING: SIGINT received! If you really want to unmount/exit, send SIGINT again.");
                                tokio::select! {
                                    _ = sigint.recv() => {
                                        println!("[SIG] Second SIGINT, exiting...");
                                        break;
                                    }
                                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {
                                        eprintln!("\nUnmount timeout elapsed. Resuming filesystem...");
                                    }
                                }
                            }
                        }
                    }
                } else {
                    println!("[SIG] Failed to register unix signal handlers. Falling back...");
                    loop {
                        let _ = tokio::signal::ctrl_c().await;
                        eprintln!("\nWARNING: Ctrl+C pressed! If you really want to unmount/exit, hit Ctrl+C again.");
                        tokio::select! {
                            _ = tokio::signal::ctrl_c() => {
                                info!("Received second Ctrl+C, exiting...");
                                break;
                            }
                            _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {
                                eprintln!("\nUnmount timeout elapsed. Resuming filesystem...");
                            }
                        }
                    }
                }
            }
            #[cfg(not(unix))]
            {
                loop {
                    let _ = tokio::signal::ctrl_c().await;
                    eprintln!("\nWARNING: Ctrl+C pressed! If you really want to unmount/exit, hit Ctrl+C again.");
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {
                            info!("Received second Ctrl+C, exiting...");
                            break;
                        }
                        _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {
                            eprintln!("\nUnmount timeout elapsed. Resuming filesystem...");
                        }
                    }
                }
            }
        };

        tokio::select! {
            res = &mut handle => {
                if let Err(e) = res {
                    error!("FUSE session loop ended with error: {:?}", e);
                    eprintln!("FUSE session loop ended with error: {:?}", e);
                } else {
                    info!("FUSE session loop ended successfully.");
                    println!("\n[!] The FUSE filesystem was unmounted externally (e.g. via umount). Squeezefs is now shutting down safely.\n");
                }
                should_exit = true;
            }
            _ = shutdown => {
                use colored::Colorize;
                use std::io::Write;
                use std::io::IsTerminal;

                println!("[SIG] Shutdown future resolved. Running shutdown logic...");
                info!("Received shutdown signal. Force flushing memory buffers to staging...");
                // Phase 1: Force flush RAM buffers to staging (cancellation NOT allowed)
                if let Err(e) = fs.flush_all_memory_buffers_to_staging().await {
                    error!("Error flushing memory buffers to staging: {:?}", e);
                }

                // Check staging status
                let mut staged_count = 0;
                let mut active_writes_count = 0;
                for key in nvme_cache.list_staged_files() {
                    if key.starts_with("active_block:") {
                        active_writes_count += 1;
                    } else {
                        staged_count += 1;
                    }
                }

                let has_unflushed = staged_count > 0 || active_writes_count > 0;
                if has_unflushed {
                    if std::io::stdin().is_terminal() {
                        println!("\n{}", "WARNING: There are unflushed staged writes on this node!".red().bold());
                        println!("Remaining local staged files: {}", staged_count);
                        println!("Active write transaction directories: {}", active_writes_count);
                        println!("If you unmount now, other nodes will not see this data.");
                        println!("\nChoose an option:");
                        println!("  [w] Wait for staged files to drain/flush to NVMe-oF backend");
                        println!("  [c] Continue/force unmount immediately (unsafe)");
                        println!("  [a] Abort unmount and continue running mount");
                        print!("Select option [w/c/a]: ");
                        let _ = std::io::stdout().flush();

                        let mut input = String::new();
                        let choice = if std::io::stdin().read_line(&mut input).is_ok() {
                            input.trim().to_lowercase()
                        } else {
                            "c".to_string()
                        };

                        if choice == "a" || choice == "abort" {
                            println!("Aborting exit. Resuming squeezefs mount.");
                            continue;
                        } else if choice == "w" || choice == "wait" {
                            println!("Waiting for staged writes to drain. Press Ctrl+C again to force exit.");
                            tokio::select! {
                                summary = fs.flush_all_staged_blocks_to_backend() => {
                                    if summary.failed > 0 {
                                        error!(
                                            "Staged-block drain: {} flushed, {} failed (first errors: {:?})",
                                            summary.flushed, summary.failed, summary.error_samples
                                        );
                                    } else {
                                        println!("\nAll staged files and active writes drained cleanly!");
                                    }
                                }
                                _ = tokio::signal::ctrl_c() => {
                                    println!("\nCtrl+C received. Cancelling staging flush to NVMe-oF backend and exiting immediately...");
                                }
                            }
                        } else {
                            println!("Continuing with unmount.");
                        }
                    } else {
                        // Non-interactive/daemon mode: flush all staged files automatically before exiting
                        info!("FUSE Daemon: Non-interactive shutdown. Automatically flushing staged files to backend...");
                        let _ = fs.flush_all_staged_blocks_to_backend().await;
                    }
                }
                should_exit = true;
            }
        }
    }

    // Stop heartbeat + fabric sampler tasks
    heartbeat_handle.abort();
    fabric_stats_handle.abort();

    // Tear down the interception session host (poisons nothing — live
    // clients observe socket EOF and degrade to passthrough, §5.7).
    if let Some(host) = fs.ipc_host.load().as_ref().as_ref().cloned() {
        let _ = tokio::task::spawn_blocking(move || host.shutdown()).await;
    }

    // Clean up the mount by unmounting the session if it hasn't been done already.
    if let Err(e) = handle.unmount().await {
        debug!("Unmount on exit status (may already be unmounted): {:?}", e);
    } else {
        info!("Cleanly unmounted filesystem on exit.");
    }

    // VL8 item 4: the dismount teardown runs on a spawned task that survives
    // the session task's cancellation (external unmount drops the destroy
    // future mid-flight). Wait for it (bounded) before the process exits so
    // `client:`/`writer_claim` heartbeat records deregister instead of
    // lingering to the 45 s staleness TTL. Bound: the teardown's own graceful
    // drain window plus flush margin — never an unbounded hang on exit.
    let teardown_wait = std::time::Duration::from_secs(fs.dismount_wait.saturating_add(60));
    if tokio::time::timeout(teardown_wait, fs.wait_dismount_teardown())
        .await
        .is_err()
    {
        warn!(
            "Dismount teardown did not complete within {:?}; heartbeat records \
             may linger to the staleness TTL",
            teardown_wait
        );
    }

    Ok(())
}

pub async fn get_volume_status(meta_lv_path: &str) -> Result<serde_json::Value, SqueezefsError> {
    // Read-only probe mount (no checkpoint task, nothing written): status
    // must be safe against a volume another process has live-mounted.
    let backend = crate::meta_backend::kv::backend::KvMetaBackend::open_probe(
        std::path::Path::new(meta_lv_path),
    )
    .await?;
    // PR VL5b: the config record rides the slot-0 keyspace (guest-
    // namespaced after a slot-0 migration).
    let val_opt = backend
        .getxattr(
            backend.slot0_root_ino(),
            crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR,
        )
        .await?;
    let val = val_opt.ok_or_else(|| {
        SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Volume not formatted",
        ))
    })?;
    let config: crate::FormatConfig = serde_json::from_slice(&val).map_err(|e| {
        SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Invalid format config: {:?}", e),
        ))
    })?;

    // The durable data-volume set (KD-5, design-volume-lifecycle §5.3):
    // keyed by the never-reused volume id, each row carrying the record's
    // REAL lifecycle state — active/disabled/draining/retired — mirroring
    // the `.config` display convention. Legacy (`data_lv`-only) configs
    // grandfather through `resolved_data_volumes()` (basename ids).
    // Retired tombstones keep their permanent id + state but OMIT the
    // cleared backing_dev (never rendered as an empty placeholder).
    let mut storage_backends = serde_json::Map::new();
    for rec in config.resolved_data_volumes() {
        let mut row = serde_json::Map::new();
        row.insert("id".into(), serde_json::json!(rec.id));
        if !rec.backing_dev.is_empty() {
            row.insert("backing_dev".into(), serde_json::json!(rec.backing_dev));
        }
        row.insert("status".into(), serde_json::json!(rec.state));
        storage_backends.insert(rec.id, serde_json::Value::Object(row));
    }

    // The real mount registrations on this volume's root ino (client
    // heartbeats + the writer claim), classified under the ONE staleness
    // law — the same records the format preflight refuses on and
    // `squeezefs clients` lists.
    let clients: Vec<serde_json::Value> = backend
        .mount_registrations()
        .await
        .iter()
        .map(|r| r.to_json())
        .collect();

    // PR 6 / N6 (design-nvmeof-target-management §6.9): the per-volume
    // "Fabric" section — present only when a backing device (this meta
    // volume or a data LV) is fabric-attached, rendered from the same
    // sysfs source as the daemon `.stats` fabric_* family. One-shot
    // sample: `fabric_ctrl_reconnects` is 0 by construction here (no
    // history to observe a transition in — the live counter is the
    // consuming daemon's `.stats` field).
    let mut backing_devices: Vec<String> = vec![meta_lv_path.to_string()];
    backing_devices.extend(
        config
            .resolved_data_volumes()
            .into_iter()
            .map(|r| r.backing_dev)
            .filter(|p| !p.is_empty()),
    );
    let fabric_section = crate::nvmeof::fabric::fabric_status_section(
        &crate::nvmeof::fabric::device_base_names(&backing_devices),
        &crate::nvmeof::fabric::enumerate_fabric_controllers(std::path::Path::new(
            crate::nvmeof::fabric::SYSFS_NVME,
        )),
    );

    // Unset optional format fields are OMITTED — never rendered as ""/[]
    // placeholders (a `None` here means "not configured", not "empty").
    let mut setting = serde_json::Map::new();
    setting.insert("Name".into(), serde_json::json!(config.name));
    setting.insert("BlockSize".into(), serde_json::json!(config.block_size));
    setting.insert("Capacity".into(), serde_json::json!(config.capacity));
    setting.insert("Inodes".into(), serde_json::json!(config.inodes));
    setting.insert("Compression".into(), serde_json::json!(config.compression));
    setting.insert("EncryptAlgo".into(), serde_json::json!(config.encrypt_algo));
    if let Some(ref v) = config.mem_cache_size {
        setting.insert("MemCacheSize".into(), serde_json::json!(v));
    }
    if let Some(ref v) = config.disk_cache_size {
        setting.insert("DiskCacheSize".into(), serde_json::json!(v));
    }
    if let Some(ref v) = config.disk_cache_paths {
        setting.insert("DiskCachePaths".into(), serde_json::json!(v));
    }
    setting.insert(
        "StorageBackends".into(),
        serde_json::Value::Object(storage_backends),
    );

    let mut status = serde_json::json!({
        "Setting": setting,
        "Clients": clients
    });
    if let Some(fabric) = fabric_section {
        status["Fabric"] = fabric;
    }
    Ok(status)
}

async fn run_constant_writeback_worker(
    mut rx: tokio::sync::mpsc::Receiver<WritebackRequest>,
    requeue_tx: tokio::sync::mpsc::Sender<WritebackRequest>,
    router: DataRouter,
    dlm: DlmClient,
    active_inode_locks: std::sync::Arc<StripeLocks<tokio::sync::RwLock<()>, 4096>>,
    max_uploads: usize,
) {
    let upload_semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_uploads));

    while let Some(req) = rx.recv().await {
        log::info!(
            "Constant Writeback: Received writeback request for ino {}, block {} (attempt {})",
            req.ino,
            req.block_idx,
            req.attempts
        );
        let router_clone = router.clone();
        let dlm_clone = dlm.clone();
        let locks_clone = active_inode_locks.clone();
        let sem_clone = upload_semaphore.clone();
        let requeue_tx = requeue_tx.clone();

        tokio::spawn(async move {
            let _permit = match sem_clone.acquire().await {
                Ok(p) => p,
                Err(e) => {
                    log::error!("Constant Writeback: Semaphore acquire failed: {:?}", e);
                    return;
                }
            };

            let file_path = format!("inode_{}", req.ino);
            let meta = match router_clone.fetch_metadata(&file_path).await {
                Ok(m) => m,
                Err(e) => {
                    log::error!(
                        "Constant Writeback: Failed to fetch metadata for {}: {:?}",
                        req.ino,
                        e
                    );
                    requeue_or_hard_fail(&requeue_tx, req, format!("fetch: {e:?}")).await;
                    return;
                }
            };

            let is_striped = meta.file_type == "striped";

            match flush_single_active_block(
                req.ino,
                req.block_idx,
                Some(req.fencing_token),
                &router_clone,
                &dlm_clone,
                &locks_clone,
                is_striped,
            )
            .await
            {
                Ok(()) => {}
                Err(e) => {
                    if matches!(e, SqueezefsError::FencingTokenExpired { .. }) {
                        // Post-FIND-M11-A this is a TRANSIENT acquire-race
                        // (generation bumped between the merge-credential
                        // read and the merge's revalidation) — superseded
                        // units no-op inside the flush unit and never get
                        // here. Sustained growth = the livelock regressing.
                        METRICS
                            .writeback_stale_token_retries
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    log::error!(
                        "Constant Writeback: Failed to flush block {} of inode {} (attempt {}): {:?}",
                        req.block_idx,
                        req.ino,
                        req.attempts,
                        e
                    );
                    requeue_or_hard_fail(&requeue_tx, req, format!("{e:?}")).await;
                }
            }
        });
    }
}

/// Never-lossy writeback retry ladder. A failed unit's bytes are SAFE in
/// staging (custody invariant), so every disposition here must converge on
/// "try again" — never a sticky per-ino error that later poisons fsync for
/// healed conditions (the multi-volume bench-suite EIO cascade: transient
/// upload failures burned WRITEBACK_MAX_ATTEMPTS in ~0.75 s, went sticky,
/// and the next per-file sync_all surfaced EIO for durable-safe data).
///
/// - Bounded fast retries first (`WRITEBACK_MAX_ATTEMPTS`, exponential
///   backoff capped at 3.2 s), then the attempt counter WRAPS: the unit
///   re-enqueues at the capped backoff forever, with
///   `writeback_retry_exhaustions` counting each wrap for observability.
/// - A FULL queue WAITS (`send`, not `try_send`): the worker owns no locks
///   here, and dropping the unit would orphan its staged bytes' durability
///   promise. `Closed` means shutdown — teardown's force-flush owns the
///   staged data from there.
/// - Genuinely superseded units never reach this ladder: the flush unit
///   itself resolves them as clean no-ops — a missing staged source
///   (flushed/truncated/newer-write-took-RAM-authority) and a re-staged
///   stamp (`owner_token` mismatch, `writeback_superseded_noops`) both
///   return `Ok`. Enforced since FIND-M11-A: the unit's staging-era
///   fencing token is ONLY the supersession test; the merge presents the
///   ino's current generation, so `FencingTokenExpired` here is a
///   transient bump race (`writeback_stale_token_retries`), never a
///   permanently-stale token cycling at capped backoff (the incident_013
///   kill-9 livelock).
async fn requeue_or_hard_fail(
    requeue_tx: &tokio::sync::mpsc::Sender<WritebackRequest>,
    mut req: WritebackRequest,
    err_msg: String,
) {
    if req.attempts + 1 >= WRITEBACK_MAX_ATTEMPTS {
        warn!(
            "Constant Writeback: retries exhausted for ino {} block {} ({}); \
             continuing at capped backoff (bytes remain staged; fsync's own \
             flush is the error surface)",
            req.ino, req.block_idx, err_msg
        );
        METRICS
            .writeback_retry_exhaustions
            .fetch_add(1, Ordering::Relaxed);
        // Wrap to the capped-backoff steady state instead of going sticky.
        req.attempts = 0;
    }
    req.attempts += 1;
    let backoff_ms = 50u64.saturating_mul(1u64 << req.attempts.min(6));
    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
    // Wait for queue room WITHOUT a bare `send().await`: this task's own
    // sender clone keeps the channel open, so a full queue whose RECEIVER
    // already exited (dismount) would park the send forever and wedge
    // daemon exit. try_send + is_closed polling escapes to teardown —
    // which owns every staged block from there (never-lossy teardown).
    let mut unit = req;
    loop {
        match requeue_tx.try_send(unit) {
            Ok(()) => return,
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                warn!("Constant Writeback: requeue after shutdown; unit handed to teardown");
                return;
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(r)) => {
                if requeue_tx.is_closed() {
                    warn!(
                        "Constant Writeback: queue receiver gone at shutdown; \
                         unit handed to teardown"
                    );
                    return;
                }
                unit = r;
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
}

/// Flush every listed staged active block of `ino` durably: one atomic
/// per-block unit each ([`flush_one_active_block`] — upload AND merge under
/// that block's `BLOCK_FLUSH_LOCKS`), streamed with bounded concurrency.
/// Distinct blocks merge independently (the §5.3 primitive serializes map
/// RMW under `INODE_META_LOCKS`); a missing staged source is a clean no-op.
/// An AUTHORITATIVE sweep (`owner_token = None`): it owns whatever is
/// staged for each block right now — fsync's durability barrier and the
/// queue-full fallback both flush the newest staged state, and any queued
/// unit for a block flushed here resolves as a no-op (source gone).
///
/// Deliberately takes NO `active_inode_locks` guard: per-block atomicity
/// (hazard 1) and the layout-prune epoch (hazard 2) carry the correctness,
/// and callers reach here from under the inode WRITE guard (punch/truncate →
/// write_file_staged → enqueue_writeback full-queue fallback), where the old
/// batch-merge write().await self-deadlocked.
async fn flush_due_active_blocks_for_inode(
    ino: u64,
    block_indices: Vec<u32>,
    router: &DataRouter,
    _dlm: &DlmClient,
    _active_inode_locks: &StripeLocks<tokio::sync::RwLock<()>, 4096>,
) -> Result<(), SqueezefsError> {
    use futures::stream::{self, StreamExt};

    let file_path = crate::keys::inode_path(ino);
    let meta = router.fetch_metadata(&file_path).await?;
    let is_striped = meta.file_type == "striped";

    let router_clone = router.clone();
    let mut flushes = stream::iter(block_indices.into_iter().map(move |block_idx| {
        let router = router_clone.clone();
        async move { flush_one_active_block(ino, block_idx, None, &router, is_striped).await }
    }))
    .buffer_unordered(8);

    while let Some(res) = flushes.next().await {
        res?;
    }
    Ok(())
}

/// Durable escalation when the staging segment refuses an active block
/// (never-lossy backpressure): upload the RAM copy straight to a backend
/// block and commit it into the inode's block map. Loud and slower than
/// staging, but the data is durable the moment this returns.
///
/// The map RMW goes through the §5.3 merge primitive (INODE_META_LOCKS) —
/// the old `active_inode_locks` write-guard merge was the second
/// serialization discipline whose coexistence with write-through would
/// have opened a lost-update window exactly under the staging-refusal
/// backpressure regime in which both fire concurrently. Callers may hold
/// the victim's `BLOCK_FLUSH_LOCKS` (P1-9 extended order: block locks →
/// INODE_META_LOCKS).
///
/// Custody path (FIND-M11-A discipline): the merge presents the ino's
/// CURRENT DLM generation, read at merge time — its callers (fsync's
/// buffer flush, dismount's RAM flush) move already-acked bytes, and an
/// op-start token snapshot going stale mid-flush must degrade to a
/// transient retryable error at worst, never a permanently-stale hard
/// failure that strands dirty RAM at dismount.
async fn upload_active_block_bytes(
    ino: u64,
    b: u32,
    block_bytes: bytes::Bytes,
    router: &DataRouter,
) -> Result<(), SqueezefsError> {
    let processed_block = router
        .get_crypto()
        .process_write_async(block_bytes.clone())
        .await?;
    let (be_id, block_allocator, nvme_writer) = router.backend_router.get_active_backend()?;
    crate::block_allocator::ensure_stored_block_image_fits(
        processed_block.len(),
        block_allocator.chunk_size(),
        "staging-refusal durable escalation",
    )?;
    let offset = block_allocator.allocate_block().await?;
    // PR VL6a: live-owner registration for the allocate→merge window.
    let _inflight = block_allocator.inflight_register(offset);
    if let Err(e) = nvme_writer.write_block(offset, processed_block).await {
        let _ = block_allocator.free_block(offset).await;
        return Err(e);
    }
    block_allocator.publish_block(offset);
    let stored_block_key = router.backend_router.persist_block_key(&be_id, offset);

    let merge_token = router.dlm.get_fencing_token_ino(ino);
    let entries = [(b, stored_block_key)];
    let merge_res = router
        .merge_block_mappings(
            ino,
            crate::routing::BlockMapOp::Merge(&entries),
            0,
            crate::routing::LayoutFlip::ToStripedKeepStagedIdentity,
            merge_token,
        )
        .await;
    let displaced = match merge_res {
        Ok(d) => d,
        Err(e) => {
            // The uploaded block never reached the map: free it before
            // propagating (same leak rule as flush_one_active_block).
            let _ = block_allocator.free_block(offset).await;
            return Err(e);
        }
    };
    for bk in displaced {
        let _ = router.backend_router.free_block(&bk).await;
    }
    Ok(())
}

/// W2 fold upload (design-random-small-writes §5.2): one durable CoW
/// block write + map merge for a fold-composed image. The
/// [`upload_active_block_bytes`] shape (merge presents the ino's CURRENT
/// generation — FIND-M11-A) plus `upload_full_block`'s dead-incarnation
/// shielding: the new key's read tiers are purged after the DMA and
/// before the map names it, and the stale whole-file LRU snapshots drop
/// with it. `min_size` stays 0 — a fold must never grow the file (folded
/// tail blocks legitimately end past `i_size`).
async fn fold_upload_block(
    ino: u64,
    b: u32,
    block_bytes: bytes::Bytes,
    router: &DataRouter,
) -> Result<(), SqueezefsError> {
    let processed_block = router.get_crypto().process_write_async(block_bytes).await?;
    let (be_id, block_allocator, nvme_writer) = router.backend_router.get_active_backend()?;
    crate::block_allocator::ensure_stored_block_image_fits(
        processed_block.len(),
        block_allocator.chunk_size(),
        "fold block upload",
    )?;
    let offset = block_allocator.allocate_block().await?;
    // PR VL6a: live-owner registration for the allocate→merge window.
    let _inflight = block_allocator.inflight_register(offset);
    if let Err(e) = nvme_writer.write_block(offset, processed_block).await {
        let _ = block_allocator.free_block(offset).await;
        return Err(e);
    }
    block_allocator.publish_block(offset);
    let stored_block_key = router.backend_router.persist_block_key(&be_id, offset);
    // No-put owner of a possibly-reused key: purge the dying incarnation's
    // tier entries (the PR 6 shielding `upload_full_block` carries).
    router.cache.purge_block_key(&stored_block_key);

    let merge_token = router.dlm.get_fencing_token_ino(ino);
    let entries = [(b, stored_block_key)];
    let displaced = match router
        .merge_block_mappings(
            ino,
            crate::routing::BlockMapOp::Merge(&entries),
            0,
            crate::routing::LayoutFlip::ToStripedKeepStagedIdentity,
            merge_token,
        )
        .await
    {
        Ok(d) => d,
        Err(e) => {
            let _ = block_allocator.free_block(offset).await;
            return Err(e);
        }
    };
    for bk in displaced {
        let _ = router.backend_router.free_block(&bk).await;
    }
    let file_path = crate::keys::inode_path(ino);
    router.cache.write_lru.remove(&file_path);
    router.cache.read_lru.remove(&file_path);
    Ok(())
}

/// Writeback-worker / teardown entry: the inode READ guard (fsync-vs-write
/// ordering courtesy) around the atomic per-block unit. `owner_token` per
/// [`flush_one_active_block`]: `Some(unit token)` for queued writeback
/// units, `None` for authoritative sweeps.
async fn flush_single_active_block(
    ino: u64,
    b: u32,
    owner_token: Option<u64>,
    router: &DataRouter,
    _dlm: &DlmClient,
    active_inode_locks: &StripeLocks<tokio::sync::RwLock<()>, 4096>,
    is_striped: bool,
) -> Result<(), SqueezefsError> {
    let _inode_guard = active_inode_locks.get_inode_lock(ino).read().await;
    flush_one_active_block(ino, b, owner_token, router, is_striped).await
}

/// `Io(NotFound)` classification for the flush unit's superseded-by-delete
/// disposition (the v3 backend's `not_found` shape — "Inode N not found").
/// Callers must pair it with an authoritative attr re-probe before
/// discarding custody (see the merge-error arm in
/// [`flush_one_active_block`]).
fn is_inode_not_found(e: &SqueezefsError) -> bool {
    matches!(e, SqueezefsError::Io(ioe) if ioe.kind() == std::io::ErrorKind::NotFound)
}

/// ONE ATOMIC PER-BLOCK FLUSH UNIT: upload staged active block `b` and merge
/// it into the map. Two delayed-merge hazards force its shape (the
/// generic/075.2 stale-data / lost-write family):
///
/// 1. LOST UPDATE between concurrent flushes of the SAME block: reading the
///    source under the block lock but merging after dropping it lets two
///    flushes merge in reverse content order — the older upload's merge
///    displaces the newer key and the newest bytes silently vanish from the
///    map (observed live: writeback worker vs batch flush of one block).
///    Fix: hold the BLOCK lock across upload AND merge — the same discipline
///    as the write-through `upload_full_block` (block lock →
///    `INODE_META_LOCKS` is the established 3 → 3.5 extended order). The old
///    post-drop `active_inode_locks.write()` batch-merge acquisition is GONE:
///    it added no content protection and self-deadlocked when the flush was
///    reached from under the inode write guard (punch/truncate →
///    write_file_staged → enqueue_writeback full-queue fallback).
/// 2. PRUNE UNDO: a truncate/punch between capture and merge is silently
///    reverted by the merge (it re-inserts the pruned block with pre-prune
///    content). The layout-prune epoch is captured with the content and
///    revalidated inside the merge's critical section; on mismatch the
///    orphaned upload is freed and the flush re-captures — after a prune the
///    purged source is gone, so the retry no-ops.
///
/// FENCING / SUPERSESSION (FIND-M11-A, the constant-writeback livelock):
/// `owner_token` is the SUPERSESSION test, never the merge credential.
///
/// - `Some(T)`: a writeback unit claiming the staging it enqueued with —
///   `T` is the token stamped on the entry at `put_active_block` time. If
///   the entry's CURRENT stamp differs, a newer write re-staged this block
///   (every re-stage pairs with its own custody chain: a queued unit or the
///   fsync/teardown sweeps), so this unit is genuinely superseded and
///   resolves as a clean no-op — the ladder-doc contract.
/// - `None`: an authoritative sweep (fsync-family, queue-full fallback,
///   dismount force-flush) that owns whatever is staged NOW.
///
/// The MERGE presents the ino's *current* DLM generation, read fresh per
/// attempt — never a token snapshotted at staging time. Staged custody
/// bytes are acked data from this mount's own writes (single writer per
/// volume by the mount-owner guard), and the generation only moves when
/// this same process re-acquires the lease (open/close churn) — a
/// staging-era snapshot goes permanently stale and livelocked the ladder
/// (27 k+ error storms, kill-9 unmounts; incident_013). Cross-node/-mount
/// staleness is governed where it always was: lease acquisition, fencing
/// checks on the foreground write paths, and `recover_staging`'s
/// generation binding ("stale fencing tokens discard staged work" is the
/// remount contract, not a license to drop live acked custody). A racing
/// bump between the read and the merge's internal revalidation surfaces
/// as a TRANSIENT `FencingTokenExpired` that the retry ladder converges.
async fn flush_one_active_block(
    ino: u64,
    b: u32,
    owner_token: Option<u64>,
    router: &DataRouter,
    is_striped: bool,
) -> Result<(), SqueezefsError> {
    let cache_key = crate::keys::active_block(ino, b as u64).to_string();

    for _attempt in 0..8 {
        // RW1: the writeback_flush lock site; the returned wait keeps
        // feeding the always-on global histogram (FIND-L1-A comparability).
        let (_block_guard, lock_waited) =
            block_lock_acquire_timed(ino, b, BlockLockSite::WritebackFlush).await;
        METRICS.block_lock_wait.record(lock_waited);

        let capture_epoch = crate::routing::layout_prune_epoch(ino);
        // Existence + ownership probe WITHOUT holding a shard guard across
        // the meta-I/O allocate below: the §5.5 DMA source is a staging-
        // shard READ guard, and a task suspended on `allocate_block().await`
        // while holding it parks every subsequent shard access behind
        // parking_lot's queued-writer fairness until the executor has no
        // worker left to resume this task — the observed total-wedge under
        // the fsx-075 harness. Allocate first (no guard), then take the
        // source; the only await under the guard is the DMA itself, whose
        // request owns the guard.
        let staged_token = match router.cache.nvme.get_staged_fencing_token(&cache_key) {
            Some(t) => t,
            // Gone (flushed / truncated / newer write took RAM authority):
            // nothing to flush.
            None => return Ok(()),
        };
        if let Some(owner) = owner_token {
            if staged_token != owner {
                // Superseded: a newer write re-staged this block under a
                // newer stamp and owns its custody chain. Resolve as the
                // contractual no-op instead of feeding the retry ladder a
                // permanently stale unit.
                METRICS
                    .writeback_superseded_noops
                    .fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
        }

        let (be_id, block_allocator, nvme_writer) = router.backend_router.get_active_backend()?;
        let offset = block_allocator.allocate_block().await?;
        // PR VL6a: the flush unit is THE canonical in-flight owner (the
        // retry-forever FIND-M11-A adversary the fsck registry exists
        // for) — registered per attempt across its allocate→merge window.
        let _inflight = block_allocator.inflight_register(offset);

        let block_data_source = match router.cache.nvme.staged_dma_source(&cache_key) {
            Some(s) => s,
            None => {
                // Purged between probe and capture (truncate/punch/newer
                // write): nothing to flush.
                let _ = block_allocator.free_block(offset).await;
                return Ok(());
            }
        };

        // §5.5: guard-backed bytes never enter any cache — the promotion LRU
        // seed (not-yet-striped files only, cold path) is a bounded real copy
        // taken while the source is alive; striped flushes never put.
        let lru_copy = (!is_striped).then(|| block_data_source.detached_copy());
        let upload_len = block_data_source.len() as u64;

        // Consumes the source (crypto transform severs the guard pre-DMA;
        // passthrough DMAs straight off the staging mmap). The guard is provably
        // dead when this returns (normative §5.5 sequencing).
        if let Err(e) = crate::cache::nvme::write_block_from_staging(
            router.get_crypto(),
            &nvme_writer,
            offset,
            block_allocator.chunk_size(),
            block_data_source,
        )
        .await
        {
            error!(
                "flush_one_active_block: Failed to upload block {} of inode {} to NVMe: {:?}",
                b, ino, e
            );
            let _ = block_allocator.free_block(offset).await;
            return Err(e);
        }
        block_allocator.publish_block(offset);
        // RW1 ledger: the flush unit's durable-upload leg (queued units AND
        // fsync/queue-full sweeps — the drivers are visible at the enqueue
        // counters).
        METRICS
            .durable_upload_bytes_writeback
            .fetch_add(upload_len, Ordering::Relaxed);

        let stored_block_key = router.backend_router.persist_block_key(&be_id, offset);

        // §5.3 one merge discipline: current-map RMW under INODE_META_LOCKS,
        // still under this block's lock (hazard 1). Free only the
        // displaced-from-current keys the primitive returns — the old
        // start-of-call `old_block_key` free was exactly the stale-snapshot
        // anti-pattern the routing merge comment forbids.
        //
        // The merge credential is the ino's CURRENT generation, read at the
        // last responsible moment (see the fencing/supersession doc above):
        // supersession was already decided by the staged stamp, and a
        // staging-era snapshot here is the FIND-M11-A livelock.
        let merge_token = router.dlm.get_fencing_token_ino(ino);
        let entries = [(b, stored_block_key.clone())];
        let merge_res = router
            .merge_block_mappings_if_epoch(
                ino,
                crate::routing::BlockMapOp::Merge(&entries),
                0,
                crate::routing::LayoutFlip::ToStripedKeepStagedIdentity,
                merge_token,
                Some(capture_epoch),
            )
            .await;
        let displaced = match merge_res {
            Ok(d) => d,
            Err(e) => {
                // The uploaded block never reached the map: free it before
                // propagating, or every retry of a failing merge (e.g. a
                // superseded fencing token between release and reopen)
                // leaks one published block of device space.
                let _ = block_allocator.free_block(offset).await;
                // NotFound face of FIND-M11-A: the merge's layout save
                // fails `Io(NotFound "Inode N not found")` when the ino
                // was unlinked + reclaimed after this block staged (the
                // delete sweep misses indices invisible to the persisted
                // layout). The record can never come back (v3 inos are
                // monotonic — no reuse), so retrying is the observed
                // NotFound spin storm. VERIFY the inode is truly gone with
                // an authoritative attr probe — a NotFound from a flaky
                // block/device read must keep retrying (never-lossy) — and
                // only then discard the orphan staged custody: the
                // recovery contract's "missing inode meta discards orphan
                // active blocks", applied live. Never resurrect a dead
                // inode; drain the entry so teardown stays bounded.
                if is_inode_not_found(&e) {
                    if let Some(backend) = router.meta_backend.get() {
                        if matches!(backend.getattr(ino).await, Err(ref ge) if is_inode_not_found(ge))
                        {
                            METRICS
                                .writeback_orphan_discards
                                .fetch_add(1, Ordering::Relaxed);
                            let _ = router
                                .cache
                                .nvme
                                .remove_active_block_async(cache_key.clone())
                                .await;
                            return Ok(());
                        }
                    }
                }
                return Err(e);
            }
        };
        let Some(displaced) = displaced else {
            // A prune invalidated this capture (hazard 2): the uploaded
            // block is unreachable — free it and re-capture.
            let _ = block_allocator.free_block(offset).await;
            continue;
        };

        // Cache in RAM (bypass entirely if file is striped layout). Promotion
        // puts a detached copy — never the guard-backed staging bytes (§5.5).
        match lru_copy {
            Some(copy) => {
                // Put-owner of a possibly-reused key: the fresh put covers
                // the RAM tier; the NVMe read tier still needs the purge — a
                // validated fill of the key's dying incarnation may have
                // published there before our allocate, and it would serve
                // dead bytes once the RAM entry evicts (same
                // dead-incarnation shielding as `upload_full_block`, PR 6).
                router.cache.purge_block_key(&stored_block_key);
                router.cache.read_lru.put(&stored_block_key, copy);
            }
            None => {
                // No-put owner of a possibly-reused key: purge instead (same
                // dead-incarnation shielding as `upload_full_block`, PR 6).
                router.cache.purge_block_key(&stored_block_key);
            }
        }

        for bk in displaced {
            let _ = router.backend_router.free_block(&bk).await;
        }

        // ONLY remove active write block from cache if it hasn't been
        // modified by a newer write — keyed on the CAPTURE-TIME staged
        // stamp, never the merge credential: teardown/fsync sweeps flush
        // under the current generation while the entry carries its
        // staging-era stamp, and comparing against the presented token
        // leaked every such entry (`staged_writes_in_flight` never drained
        // — the FIND-M11-A unmount drain-wait wedge). spawn_blocking: the
        // shard WRITE lock must never park an async worker (see the probe
        // comment above — this exact remove was a parked frame in the
        // observed wedge).
        {
            let nvme = router.cache.nvme.clone();
            let key = cache_key.clone();
            tokio::task::spawn_blocking(move || {
                let current_token = nvme.get_staged_fencing_token(&key);
                if let Some(tok) = current_token {
                    if tok == staged_token {
                        nvme.remove_active_block(&key);
                    }
                } else {
                    nvme.remove_active_block(&key);
                }
            })
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        }

        return Ok(());
    }
    Err(SqueezefsError::InvalidOperation(format!(
        "flush_one_active_block: layout-prune epoch kept moving for ino {ino} block {b}"
    )))
}

/// Drain one reclaim batch: after the first ino arrives, hold a short
/// gather window so a FORGET storm coalesces into a REAL batch. Without the
/// window, a 1:1 unlink→FORGET storm outruns `try_recv` and every "batch"
/// degenerates to 1–2 inos — reclaim then interferes op-for-op with the
/// foreground delete path (measured: ~360 µs/unlink vs ~180 µs with reclaim
/// quiescent). Reclaim is background by definition; +`window` of slot-reuse
/// latency is free, and the batch's meta work amortizes ~cap× (§4.5).
/// Returns `None` when the channel closed with nothing pending.
///
/// Records the D4.a fill-vs-window attribution
/// (design-metadata-throughput §5.4): the post-dedup gather fill
/// (`meta_reclaim_gather_fill`) and how the batch closed
/// (`meta_reclaim_gather_{cap,window,channel}_closes`). `pub` because it
/// is the reclaim worker pool's gather step and the seam
/// `tests/meta_entry_economy_tests.rs` drives to pin those counters.
pub async fn drain_reclaim_batch(
    rx: &mut tokio::sync::mpsc::Receiver<u64>,
    cap: usize,
    window: std::time::Duration,
) -> Option<Vec<u64>> {
    let first = rx.recv().await?;
    let mut batch = vec![first];
    let mut channel_closed = false;
    if !window.is_zero() {
        let deadline = tokio::time::Instant::now() + window;
        while batch.len() < cap {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(ino)) => batch.push(ino),
                Ok(None) => {
                    channel_closed = true;
                    break;
                }
                Err(_) => break, // window elapsed
            }
        }
    }
    while batch.len() < cap {
        match rx.try_recv() {
            Ok(ino) => batch.push(ino),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                channel_closed = true;
                break;
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
        }
    }
    // D4.a fill-vs-window attribution: how did this gather close? Cap
    // (the healthy storm outcome) beats channel-close beats window-expiry
    // (the trickle outcome — a FORGET storm landing here with tiny fills
    // is the batch-fill degeneration D4.c hunts).
    if batch.len() >= cap {
        METRICS
            .meta_reclaim_gather_cap_closes
            .fetch_add(1, Ordering::Relaxed);
    } else if channel_closed {
        METRICS
            .meta_reclaim_gather_channel_closes
            .fetch_add(1, Ordering::Relaxed);
    } else {
        METRICS
            .meta_reclaim_gather_window_closes
            .fetch_add(1, Ordering::Relaxed);
    }
    // FORGET can enqueue an ino more than once across sessions.
    batch.sort_unstable();
    batch.dedup();
    METRICS.meta_reclaim_gather_fill.record(batch.len());
    Some(batch)
}

async fn run_reclaim_worker_pool(
    mut rx: tokio::sync::mpsc::Receiver<u64>,
    fs: SqueezefsFilesystem,
    concurrency: usize,
) {
    // Group-commit batching (design §4.5): drain up to SQUEEZEFS_RECLAIM_BATCH
    // inos per unit of work — sequential allocation clusters doomed inos in
    // the same inode-table sectors, so a batch's slot zeroes merge into
    // shared sector images and one apply write.
    let batch_cap = std::env::var("SQUEEZEFS_RECLAIM_BATCH")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|v| v.clamp(1, 1024))
        .unwrap_or(64);
    let window_ms = std::env::var("SQUEEZEFS_RECLAIM_BATCH_WINDOW_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|v| v.min(1000))
        .unwrap_or(20);
    let window = std::time::Duration::from_millis(window_ms);
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
    let fs_arc = std::sync::Arc::new(fs);
    while let Some(batch) = drain_reclaim_batch(&mut rx, batch_cap, window).await {
        let fs_clone = fs_arc.clone();
        let sem_clone = semaphore.clone();
        let permit = sem_clone.acquire_owned().await.unwrap();
        tokio::spawn(async move {
            let _permit = permit;
            fs_clone.reclaim_orphaned_batch(batch).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// NEVER-LOSSY WRITEBACK, pinned (the multi-volume bench-suite EIO):
    /// a writeback unit whose upload keeps failing has its bytes SAFE in
    /// staging (never-lossy custody) — the retry ladder must therefore
    /// never park the request on a sticky per-ino failure map that later
    /// poisons fsync into EIO after the condition healed. The taped
    /// cascade: churn overshoot → transient upload failures → 4 retries
    /// in ~0.75 s → sticky → the bench write-rand pass's per-file
    /// `sync_all` returns the errno → the suite dies while the daemon and
    /// every byte are fine. fsync's honest error surface is its OWN
    /// synchronous flush of the same staged blocks.
    ///
    /// Contract: a requeue that finds the queue FULL must WAIT (bounded
    /// await, never drop, never sticky) — the sticky map must stay empty.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writeback_requeue_on_full_queue_never_goes_sticky() {
        let ino = 990_001u64;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<WritebackRequest>(1);
        // Occupy the single slot so the requeue hits Full.
        tx.try_send(WritebackRequest {
            ino: 1,
            block_idx: 0,
            fencing_token: 1,
            attempts: 0,
        })
        .unwrap();
        let req = WritebackRequest {
            ino,
            block_idx: 7,
            fencing_token: 1,
            attempts: 0,
        };
        let tx2 = tx.clone();
        let requeue = tokio::spawn(async move {
            requeue_or_hard_fail(&tx2, req, "transient upload failure".into()).await;
        });
        // Give the requeue a moment to hit the Full arm, then drain.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let _ = rx.recv().await; // frees the slot
        requeue.await.unwrap();
        // The request must still be queued (never lost, never sticky).
        let got = rx.recv().await.expect("requeued unit");
        assert_eq!(got.ino, ino);
        assert_eq!(got.block_idx, 7);
    }

    /// Contract: EXHAUSTED bounded retries re-enqueue with capped backoff
    /// (observability counter, no sticky map entry) — retry-forever is the
    /// only disposition consistent with never-lossy custody; genuinely
    /// superseded units (fencing) already no-op inside the flush unit and
    /// never reach this ladder.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writeback_exhausted_retries_requeue_forever_not_sticky() {
        let ino = 990_002u64;
        let before = METRICS.writeback_retry_exhaustions.load(Ordering::Relaxed);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<WritebackRequest>(8);
        let req = WritebackRequest {
            ino,
            block_idx: 3,
            fencing_token: 1,
            attempts: WRITEBACK_MAX_ATTEMPTS - 1,
        };
        requeue_or_hard_fail(&tx, req, "transient upload failure".into()).await;
        let got = rx
            .try_recv()
            .expect("exhausted unit must be RE-ENQUEUED (retry forever), not dropped");
        assert_eq!(got.ino, ino);
        assert_eq!(got.block_idx, 3);
        assert!(
            METRICS.writeback_retry_exhaustions.load(Ordering::Relaxed) > before,
            "each exhaustion wrap must be counted for observability"
        );
    }

    /// `SQUEEZEFS_TIMEOUT` is a LAUNCH-TIME knob: the op timeout must be
    /// resolved once and memoized, not re-read per FUSE op —
    /// `std::env::var` takes the process-global env lock and allocates,
    /// and it showed up at ~0.7% of daemon cycles (plus lock
    /// serialization) on the warm rand-4k transport row, called on every
    /// request. Mid-run env mutation taking effect was never a contract.
    #[test]
    fn fuse_timeout_is_memoized_not_per_op_env_read() {
        let first = get_fuse_timeout();
        // A later env mutation must NOT change the resolved timeout.
        std::env::set_var("SQUEEZEFS_TIMEOUT", "1234");
        let second = get_fuse_timeout();
        std::env::remove_var("SQUEEZEFS_TIMEOUT");
        assert_eq!(
            first, second,
            "get_fuse_timeout consulted the environment after first \
             resolution — a per-op env::var read on the hot path"
        );
        assert_ne!(
            second,
            Duration::from_secs(1234),
            "the post-launch env mutation leaked into the op timeout"
        );
    }

    /// Regular-file open/create replies must advertise
    /// FOPEN_PARALLEL_DIRECT_WRITES (kernel ABI bit 1 << 6, fuse ≥ 7.38 /
    /// Linux ≥ 6.2). Without it the kernel takes the inode lock EXCLUSIVE
    /// around every O_DIRECT write submission, so multi-threaded /
    /// iodepth>1 O_DIRECT writers to one file serialize in the kernel
    /// before the daemon ever sees a request (elbencho `-w -b 4k --iodepth
    /// 16 --direct`: 16 in-flight buys zero concurrency). Daemon-side
    /// correctness does not depend on that kernel lock: the write handler
    /// serializes per inode via `active_inode_locks` for meta-prep and
    /// per-block `BLOCK_FLUSH_LOCKS` for data merges, and concurrent
    /// overlapping O_DIRECT writes carry no POSIX atomicity guarantee.
    /// Extending writes stay kernel-exclusive regardless (fuse_dio_lock
    /// checks past-EOF), so size-extension ordering is unaffected.
    #[test]
    fn regular_open_reply_advertises_parallel_direct_writes() {
        assert_eq!(
            FOPEN_PARALLEL_DIRECT_WRITES,
            1 << 6,
            "kernel ABI value for FOPEN_PARALLEL_DIRECT_WRITES is 1 << 6 \
             (include/uapi/linux/fuse.h); any other value advertises a \
             different capability"
        );
        assert_eq!(
            regular_open_reply_flags() & FOPEN_PARALLEL_DIRECT_WRITES,
            FOPEN_PARALLEL_DIRECT_WRITES,
            "regular files must advertise parallel direct writes"
        );
        assert_eq!(
            regular_open_reply_flags() & 1,
            0,
            "FOPEN_DIRECT_IO must stay reserved for the virtual \
             .stats/.config inodes"
        );
    }

    /// D2.a (design-metadata-throughput §5.2, PR M5): regular-file
    /// open/create replies must advertise FOPEN_NOFLUSH (kernel ABI bit
    /// 1 << 5, fuse ≥ 7.35 / Linux ≥ 5.16) so the kernel elides the FLUSH
    /// round trip on close of clean handles — the −1.0 op of G7's
    /// 5.18 → ≤ 4.2 ledger. Safe on every kernel: pre-7.35 kernels ignore
    /// unknown open flags; SqueezeFS FLUSH is already soft (fsync is the
    /// durable barrier), the M1 single-writer guard closes the
    /// cross-mount coherence class, and later-dirtied handles keep their
    /// close-time flush via RELEASE's background path.
    #[test]
    fn regular_open_reply_advertises_noflush() {
        assert_eq!(
            FOPEN_NOFLUSH,
            1 << 5,
            "kernel ABI value for FOPEN_NOFLUSH is 1 << 5 \
             (include/uapi/linux/fuse.h); any other value advertises a \
             different capability"
        );
        assert_eq!(
            regular_open_reply_flags(),
            FOPEN_NOFLUSH | FOPEN_PARALLEL_DIRECT_WRITES,
            "regular files must advertise exactly NOFLUSH + parallel \
             direct writes (no FOPEN_DIRECT_IO — the page-cache path \
             stays enabled)"
        );
    }

    #[test]
    fn component_name_len_accepts_name_max() {
        let name = "a".repeat(FUSE_NAME_MAX);
        assert!(check_component_name_len(OsStr::new(&name)).is_ok());
    }

    #[test]
    fn component_name_len_rejects_over_name_max() {
        let name = "a".repeat(FUSE_NAME_MAX + 1);
        let err = check_component_name_len(OsStr::new(&name)).unwrap_err();
        // fuse3::Errno stores the negated libc errno.
        assert_eq!(i32::from(err).unsigned_abs(), libc::ENAMETOOLONG as u32);
    }

    #[test]
    fn component_name_len_accepts_empty_and_short() {
        assert!(check_component_name_len(OsStr::new("")).is_ok());
        assert!(check_component_name_len(OsStr::new("x")).is_ok());
        assert!(check_component_name_len(OsStr::new(&"b".repeat(255))).is_ok());
    }
}
