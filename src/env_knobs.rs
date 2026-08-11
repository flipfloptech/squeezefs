//! The env-knob registry and the startup refusal gate (ENG-10, pre-RC
//! spec §10).
//!
//! `env_knob_core` says what a VALUE means; this module says which knobs
//! exist, what each accepts, and what happens when the environment carries
//! something else. Both halves are needed: a shared parser that 56 silent
//! call sites still route around would not change a single operator
//! outcome.
//!
//! # The convention
//!
//! * **Absent** = unset, empty, or whitespace-only.
//! * **A malformed or out-of-range value refuses the process at startup**,
//!   naming every offending knob at once (not the first one, then the next
//!   one on the next attempt). [`refusal_report`] runs
//!   before any volume is opened or mounted, so a typo costs an exit code,
//!   never a half-mounted filesystem or a silently different tuning than
//!   the operator asked for.
//! * **An unknown `SQUEEZEFS_*` / `SQZ_*` name is announced, not refused.**
//!   That is the typo detector for knob NAMES (previously invisible), and
//!   it must not refuse: a mixed-version fleet legitimately carries the
//!   next release's knobs, and the interception shim's client-side knobs
//!   live in the same environment as the daemon's.
//! * **Retired spellings refuse loudly, naming the successor** — the
//!   `format --meta-slots` precedent, never a silent alias.
//!
//! # Why a registry rather than 117 rewritten call sites
//!
//! The call sites keep their `unwrap_or(default)` shape, which is what
//! makes them cheap and readable — but the branch is now unreachable for a
//! MALFORMED value, because the process refused before any of them ran.
//! One enforcement point, no per-site drift, and the table doubles as the
//! documentation the spec found missing for ~20 knobs (`docs/operations.md`
//! §Environment knobs points here as the authority).
//!
//! Contract: `tests/env_knob_convention_tests.rs` — every knob literal in
//! the tree must be registered (so a new knob cannot be born undocumented),
//! no name may be a strict prefix of another (the `SQUEEZEFS_RECLAIM_BATCH`
//! collision, pinned dead), and the parsing/refusal behavior is pinned per
//! kind.

use crate::env_knob_core as core;

/// What a knob accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `1/true/yes/on` or `0/false/no/off` (ASCII-case-insensitive).
    Bool,
    /// An integer in an inclusive admissible range. Out-of-range refuses
    /// (a knob set to 10× the maximum is a mistake worth naming).
    Int { lo: i128, hi: i128 },
    /// One of a fixed set of words.
    Enum(&'static [&'static str]),
    /// A path, URI or comma list — presence is all this layer checks; the
    /// consumer produces the better error for bad content.
    Str,
    /// A retired spelling: always refused, naming the successor.
    Retired { successor: &'static str },
    /// Injected by `build.rs` as a rustc env (compile time), not read from
    /// the process environment. Registered so the name census is complete.
    BuildTime,
    /// A test-harness variable (suite seams, re-exec child markers). Known,
    /// deliberately unvalidated — the harness owns its own contract.
    Harness,
}

/// One registered knob.
#[derive(Debug, Clone, Copy)]
pub struct Knob {
    pub key: &'static str,
    pub kind: Kind,
    /// The default when absent, phrased for an operator ("derived" where a
    /// sizing function owns it).
    pub default: &'static str,
    /// One line: what it does.
    pub doc: &'static str,
}

const fn k(key: &'static str, kind: Kind, default: &'static str, doc: &'static str) -> Knob {
    Knob {
        key,
        kind,
        default,
        doc,
    }
}

const fn int(lo: i128, hi: i128) -> Kind {
    Kind::Int { lo, hi }
}

/// Milliseconds ceiling used by the timing knobs: 10 minutes. Past that a
/// "timeout" is a hang with extra steps.
const MS_MAX: i128 = 600_000;
/// A byte-count ceiling: 1 TiB. Nothing here is legitimately larger, and
/// past it the value is a units mistake (bytes given where MiB was meant).
const BYTES_MAX: i128 = 1 << 40;
/// A day in milliseconds: the ceiling for the knobs whose 'effectively
/// never' idiom is a huge timer value (`tests/fsync_*_tests.rs` park the
/// metadata flush cadence at an hour).
const DAY_MS: i128 = 86_400_000;

/// Every `SQUEEZEFS_*` / `SQZ_*` name the tree reads, with its contract.
///
/// Grouped by subsystem; the order is documentation, not semantics.
pub static KNOBS: &[Knob] = &[
    // -- Build identity (build.rs rustc envs, compile time) -------------
    k("SQUEEZEFS_BUILD_COMMIT", Kind::BuildTime, "git", "Build commit hash embedded by build.rs."),
    k("SQUEEZEFS_BUILD_COMMIT_SHORT", Kind::BuildTime, "git", "Short form of the build commit."),
    k("SQUEEZEFS_BUILD_DIRTY", Kind::BuildTime, "git", "1 when the build tree carried uncommitted tracked changes."),
    k("SQUEEZEFS_BUILD_TAG", Kind::BuildTime, "git", "Exact stable-*/lts-* tag on the build commit, else empty."),
    k("SQUEEZEFS_BUILD_TIMESTAMP", Kind::BuildTime, "git", "UTC RFC3339 build timestamp (honors SOURCE_DATE_EPOCH)."),
    k("SQUEEZEFS_IL_BUILD_COMMIT", Kind::BuildTime, "git", "The shim's build commit (KD-7 daemon/shim pairing)."),
    // -- CLI / process ---------------------------------------------------
    k("SQUEEZEFS_META_URI", Kind::Str, "none", "Default sqmeta:// URI for the verbs that take one (clap env)."),
    k("SQUEEZEFS_ENCRYPT_KEY_FILE", Kind::Str, "/etc/squeezefs/keys/<id>.key", "Encryption key material path (docs/design-key-handling.md)."),
    k("SQUEEZEFS_DAEMON_PIPE", int(0, i32::MAX as i128), "none", "Internal: fd the forked daemon writes `ready` to. Set by the parent, never by hand."),
    k("SQUEEZEFS_TIMEOUT", int(1, 86_400), "30", "Metadata-operation timeout, seconds (memoized at launch)."),
    k("SQUEEZEFS_SUPERVISE_INTERVAL_SECS", int(1, 86_400), "5", "`mount --supervise` probe interval, seconds."),
    k("SQUEEZEFS_SUPERVISE_UNRESPONSIVE_SECS", int(1, 86_400), "30", "`mount --supervise` unresponsiveness threshold before abort, seconds."),
    // -- Memory budget (R5) ---------------------------------------------
    k("SQUEEZEFS_MEM_BUDGET_MB", int(1, 1 << 30), "derived", "R5 memory budget, MiB (absolute > pct > cgroup/RAM derivation)."),
    // -- Metadata (KV v3) -----------------------------------------------
    k("SQUEEZEFS_META_NODE_CACHE_MB", int(1, 1 << 30), "derived", "Per-volume KV node-cache budget, MiB (absolute wins over _PCT)."),
    k("SQUEEZEFS_META_NODE_CACHE_PCT", int(1, 100), "derived", "Per-volume KV node-cache budget as a percentage of the R5 budget."),
    k("SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES", int(1, 1 << 32), "derived", "Dirty-node checkpoint cap = the mount-replay working-set bound."),
    k("SQUEEZEFS_META_COMMIT_BATCH_TXS", int(1, 1 << 20), "derived", "M7 commit-conveyor batch cap, transactions."),
    k("SQUEEZEFS_META_COMMIT_BATCH_BYTES", int(1, BYTES_MAX), "derived", "M7 commit-conveyor batch cap, bytes (clamped to the ring's admissible capacity)."),
    k("SQUEEZEFS_META_FLUSH_INTERVAL_MS", int(0, DAY_MS), "50", "Journal/checkpoint cadence, ms; 0 = strict per-commit. A very large value is the 'park the timer' idiom two suites use, hence the day-long ceiling."),
    k("SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS", int(0, DAY_MS), "unset", "Legacy alias for SQUEEZEFS_META_FLUSH_INTERVAL_MS (the new spelling wins)."),
    k("SQUEEZEFS_META_REVALIDATE_MS", int(1, DAY_MS), "derived", "Coherent-READER node-cache revalidation cadence, ms (spec §6.8 item 2). Derived default = max(flush cadence, the 1 s checkpoint ceiling) — polling faster than the writer mints ledger records buys no freshness and pays a drop pass. Trades staleness (interval + 1 s) against reload cost; inert on write mounts."),
    k("SQUEEZEFS_BLOCK_REFS_VERIFY", Kind::Bool, "off", "Run the durable-vs-derived block-reference oracle at MOUNT (spec §6.2 item 1). Off by default because it pays the inode-tree walk the durable records exist to delete; fsck runs the same comparison unconditionally as class C8."),
    // -- Layout / publish economy ---------------------------------------
    k("SQUEEZEFS_DEFAULT_BLOCK_SIZE", int(4096, BYTES_MAX), "4194304", "Default striped block size, bytes, when format did not record one."),
    k("SQUEEZEFS_PUBLISH_COALESCE_MAX", int(0, 1 << 20), "64", "Per-ino publish-coalescing window; 1 = the pre-campaign serialized posture (A/B lever)."),
    k("SQUEEZEFS_LAYOUT_DELTA_MAX_CHAIN", int(0, 1 << 20), "64", "Layout delta-record chain cap before a full re-base save; 0 = full saves only (A/B lever)."),
    k("SQUEEZEFS_PUBLISH_COMMIT_GROUP_MAX", int(0, 1 << 20), "0 (derives from META_COMMIT_BATCH_TXS)", "Layout saves aggregated into one multi-ino KvTx per conveyor window; 1 = per-save commits (A/B lever)."),
    // -- Write path ------------------------------------------------------
    k("SQUEEZEFS_PATCH_MAX_BYTES", int(0, BYTES_MAX), "derived block_size/8", "W1 sole-owner in-place patch ceiling, bytes; 0 = the acceptance A/B lever."),
    k("SQUEEZEFS_FOLD_MAX_EXTENTS", int(0, 1 << 24), "64", "W2 fold trigger: parked extents per block."),
    k("SQUEEZEFS_FOLD_MAX_BYTES", int(0, BYTES_MAX), "derived block_size/4", "W2 fold trigger: parked bytes per block."),
    k("SQUEEZEFS_PARKED_BUFFERS", int(0, 1 << 24), "derived", "W2 parked-write budget in buffers' worth of bytes (× block size)."),
    k("SQUEEZEFS_PARKED_GATE_ASSIST_MS", int(0, MS_MAX), "200", "R5-Red parked-gate self-flush assist window, ms."),
    k("SQUEEZEFS_INPLACE_OVERWRITE", Kind::Bool, "off", "Opt a substrate into in-place eligible full-block overwrites (real-SSD DSM fleets; measured loss on zram-lz4)."),
    k("SQUEEZEFS_DEVICE_OVERLAY", Kind::Bool, "on", "Approach B device-backed visible overlay (design-device-overlay, PR B2; one-path write store): eligible fresh/hole aligned segments store slot/Bytes -> unpublished dest, ACK-early, publication at coverage completion/fsync. Default ON — this is the 35 GB/s A-leg, not an opt-in. `0` = accumulation A/B (the B2 0.77x ACK-after-CQE control). Engagement: overlay_store_bytes / overlay_ack_early_bytes."),
    k("SQUEEZEFS_REWRITE_SHADOW", Kind::Bool, "on", "Rewrite-program shadow swaps; 0 = the A/B control."),
    k("SQUEEZEFS_WRITEBACK_QUEUE_CAP", int(1, 1 << 24), "4096", "Writeback flush-unit queue capacity."),
    k("SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS", int(0, 1 << 20), "derived (BDP)", "Write-pipeline depth target override, blocks — MEASUREMENT lever; the default is the runtime BDP derivation."),
    // -- Block reclaim (device deallocation) -----------------------------
    k("SQUEEZEFS_RECLAIM_BATCH_BLOCKS", int(1, 1024), "64", "Background block-reclaim drain batch, blocks."),
    k("SQUEEZEFS_RECLAIM_BATCH_MS", int(0, MS_MAX), "2", "Background block-reclaim batch-accumulation window, ms."),
    k("SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS", int(1, 1 << 20), "4096", "Block-reclaim queue cap; at-cap enqueues park (park-don't-spill)."),
    k("SQUEEZEFS_RECLAIM_LANES_PER_DEV", int(1, 64), "32", "Parallel reclaim drain lanes per device."),
    k("SQUEEZEFS_ALLOC_LANE_RESERVE_BLOCKS", int(0, 1 << 32), "derived", "Fresh blocks one durable data-plane allocation-lane reservation covers (DLM S9 blocker #3). Derived from the write pipeline's cold window (FLOOR_BLOCKS_PER_LANE × HEADROOM × cpus), floored at the 8-lane cold aggregate and capped at 1/64 of a lane share; 0 = derived, 1 = a commit per fresh block (the pathological A/B control). Inert on every unpartitioned (single-writer) mount."),
    k("SQUEEZEFS_RECLAIM_CAP_PARK_MS", int(0, 60_000), "1000", "At-cap reclaim enqueue park bound, ms, before soft overflow."),
    k("SQUEEZEFS_DISCARD_ELISION", Kind::Bool, "on", "Elide discards for blocks a rewrite is about to overwrite; 0 = the A/B control."),
    // -- Inode reclaim (ENG-10 rename: the old spellings collided with the
    //    block-reclaim family above, `SQUEEZEFS_RECLAIM_BATCH` being a
    //    strict prefix of `SQUEEZEFS_RECLAIM_BATCH_BLOCKS`) --------------
    k("SQUEEZEFS_INODE_RECLAIM_BATCH", int(1, 1024), "64", "Deferred inode-reclaim gather batch, inodes."),
    k("SQUEEZEFS_INODE_RECLAIM_WINDOW_MS", int(0, 1000), "20", "Deferred inode-reclaim gather window, ms."),
    k("SQUEEZEFS_INODE_RECLAIM_CONCURRENCY", int(1, 1 << 16), "max(4, cpus)", "Concurrent inode-reclaim workers."),
    k("SQUEEZEFS_RECLAIM_BATCH", Kind::Retired { successor: "SQUEEZEFS_INODE_RECLAIM_BATCH" }, "-", "Retired (ENG-10): collided with the SQUEEZEFS_RECLAIM_BATCH_BLOCKS block-reclaim family."),
    k("SQUEEZEFS_RECLAIM_BATCH_WINDOW_MS", Kind::Retired { successor: "SQUEEZEFS_INODE_RECLAIM_WINDOW_MS" }, "-", "Retired (ENG-10): inode reclaim, renamed out of the block-reclaim prefix."),
    k("SQUEEZEFS_RECLAIM_CONCURRENCY", Kind::Retired { successor: "SQUEEZEFS_INODE_RECLAIM_CONCURRENCY" }, "-", "Retired (ENG-10): inode reclaim, renamed out of the block-reclaim prefix."),
    // -- Read path -------------------------------------------------------
    k("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", int(0, 1 << 30), "derived", "R4 hot-block RAM tier budget, MiB."),
    k("SQUEEZEFS_READ_ADMISSION_FILL_PCT", int(0, 100), "5", "Scan-resistant admission governor's bounded-waste share, percent of device read bytes."),
    k("SQUEEZEFS_READ_TIER_ADMISSION", Kind::Enum(&["always", "second-touch", "never"]), "second-touch", "R1b tier-admission policy."),
    k("SQUEEZEFS_READ_PREFETCH_WINDOW", int(0, 1 << 20), "derived", "R2 prefetch window override, blocks."),
    k("SQUEEZEFS_READ_PREFETCH_SHARE_PCT", int(1, 100), "50", "R2 prefetch share of the read budget, percent."),
    k("SQUEEZEFS_READ_RANGED_THRESHOLD", int(0, BYTES_MAX), "262144", "R3 ranged-read threshold, bytes."),
    k("SQUEEZEFS_READ_DEST_LEASE", Kind::Bool, "on", "READ dest-window lease (copy-elimination phase 1): cold aligned sub-block windows DMA device bytes straight into the reply's registered dest — the serve dest-copy deleted; 0 = the A/B control (pre-campaign fill+serve-copy shape)."),
    k("SQUEEZEFS_READ_LANE", Kind::Bool, "on", "R1b read-lane hold (the shipped default win); 0 = the A0 control."),
    k("SQUEEZEFS_READ_LANE_DEPTH", int(0, 1 << 16), "derived (engage-governor)", "Read-ahead lane depth pin, blocks — unset = probe-governed (probe-adopt-retreat); 0 = ahead-issue off (the hold-only A/B control)."),
    k("SQUEEZEFS_NVME_READ_LANES", int(1, 1024), "derived (cpus / data devices)", "Per-device READ submission fan-out lanes (read-queue-wall campaign); 1 = single-worker A/B posture."),
    k("SQUEEZEFS_NVME_WRITE_LANES", int(1, 1024), "derived (cpus / data devices)", "Per-device DATA-WRITE submission fan-out lanes, block-offset affinity (write-lane-fanout campaign); 1 = single-worker pre-fanout A/B posture."),
    k("SQUEEZEFS_DIRECT_DEVICE_TRUE", Kind::Bool, "off", "Strict device-true O_DIRECT (the amplification-measurement escape); default serves O_DIRECT like buffered."),
    // -- Copy economy ----------------------------------------------------
    k("SQUEEZEFS_WRITE_SHARED", Kind::Bool, "on", "Shared-mode write admission (design-write-inode-convoy): fully-mapped within-EOF striped overwrites take the inode READ guard (classify → acquire-as-classified → revalidate → at most one upgrade). Default ON — the 2026-08-11 elision-era A-B-B-A: +7-8% order-independent on kern AND il at 523-530k IOPS; `=0` is the measurement A/B lever."),
    k("SQUEEZEFS_NT_COPY", Kind::Bool, "on", "Non-temporal stores at the two DMA-destined copy sites; 0 = the A/B control."),
    k("SQUEEZEFS_NT_COPY_MIN", int(0, BYTES_MAX), "262144", "NT-store engagement floor, bytes."),
    k("SQUEEZEFS_NT_READ_SERVE", Kind::Bool, "on", "Non-temporal stores on dest-arm read serves; 0 = the A/B control."),
    k("SQUEEZEFS_NT_READ_SERVE_MIN", int(0, BYTES_MAX), "262144", "Read-serve NT-store floor, bytes (keeps rand-4k/warm-small serves cached)."),
    k("SQUEEZEFS_NUMA", Kind::Bool, "on", "NUMA placement actions (structurally inert on single-node hosts); 0 = the A/B lever, instrument stays live."),
    k("SQUEEZEFS_READ_MOSTLY_CACHE", Kind::Bool, "on", "L3 coherence campaign (2026-08-08): read-mostly backing for the hot process-global caches (metadata/stream-lanes/attrs) — reads are pure loads (scc peek), eviction rides a moka policy shell; deletes moka's per-read bookkeeping (54.6%/33.4% of svc/dd cycles on the 2-socket field box). 0 = the classic moka value caches verbatim (the A/B control)."),
    k("SQUEEZEFS_CACHE_TOUCH_SECS", int(0, 86_400), "derived (TTI/4)", "L3 mech-2: the TTI-cache policy-touch sampling horizon, seconds — an entry read at least once per horizon keeps access-refreshed residency (4x margin inside the 300 s TTI). 0 = touch the policy shell on EVERY read (the un-sampled isolation lever); explicit value wins verbatim."),
    // -- Transport (fuse3 fork) ------------------------------------------
    k("SQUEEZEFS_FUSE_MAX_WRITE", int(4096, BYTES_MAX), "derived", "Negotiated FUSE max_write, bytes."),
    k("SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES", int(1, 512), "kernel possible CPUs", "FUSE-over-io_uring queue count (TESTING only — fewer than possible CPUs never becomes ready)."),
    k("SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH", int(1, 32), "derived (desired 32)", "Entries per FUSE-over-io_uring queue; explicit value wins verbatim."),
    k("SQUEEZEFS_FUSE_IO_URING_ENTRIES", int(64, 4096), "1024", "SQ depth for the classical FUSE rings (INIT/notify)."),
    k("SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS", int(0, MS_MAX), "off", "Enable SQPOLL on the transport rings with this idle timeout, ms (measured NOT recommended)."),
    k("SQUEEZEFS_FUSE_IO_URING_SQPOLL_CPU", int(0, 4095), "unpinned", "Pin the SQPOLL kernel thread to this CPU."),
    k("SQUEEZEFS_FUSE_PIN_SCOPE", Kind::Enum(&["node", "core"]), "node", "Transport thread affinity scope; `core` is the pre-campaign hard-pin posture."),
    k("SQUEEZEFS_FUSE_SAME_LANE_DISPATCH", Kind::Bool, "1", "READ handler futures spawn_local on the dispatching TPC lane instead of round-robining to another lane (transport-ingress lever 1); `0` restores the rotation (A0 control)."),
    k("SQUEEZEFS_FUSE_DRAIN_GROUP", int(1, 512), "derived (node possible CPUs / 4, floor 1)", "Queues per FUSE-over-io_uring drain context (ingress-queue-spread lever 2); explicit width wins verbatim, `1` = the per-queue-worker A0 control."),
    k("SQUEEZEFS_FUSE_KMBUF", Kind::Bool, "on", "kmbuf reply-buffer negotiation; 0 = the A/B control."),
    k("SQUEEZEFS_FUSE_ZC", Kind::Bool, "on", "FUSE_URING_ZERO_COPY serve integration (K1 kill; sqz kernel + CAP_SYS_ADMIN required — declines loud elsewhere, stock kernels run the bufring path byte-identically). Default ON per ruling D16 (2026-08-07, rc-manifest §3f — supersedes the 0.97x all-write-rows rule): reads +40-75%; known caveat = un-shimmed kernel-lane rand-4k writes pay ~20% (extraction hop) until handler/worker fusion lands; `0` is the escape/A-B lever."),
    k("SQUEEZEFS_FUSE_ZC_WRITE_FUSION", Kind::Bool, "on", "Handler/worker FUSION for small armed FUSE WRITEs. Default ON again (fused-lane-predicate fix, 2026-08-08): the field falsification (0.45x armed rand-4k at fabric RTT) was the shape-only hold PREDICATE double-paying W1-ineligible ops (hold + fused poll + LATE extraction) — the hold now gates on the filesystem's W1-eligibility seam and ineligible shapes extract at delivery on the classic dispatch; fabric-emulated + un-emulated A-B-B-A acceptance in .benchmarks/2026-08-08-fused-lane-predicate.md. `0` = the A/B control. Engagement: fuse3_zc_write_fusions/_bytes; fuse3_zc_write_lazy_extractions ~ 0 is the hold gate's staleness law."),
    k("SQUEEZEFS_FUSE_ZC_FUSION_MAX", int(4096, 1 << 30), "derived (payload/8)", "Fused-dispatch payload ceiling, bytes — bounds the handler work the queue worker's drain loop runs inline (the write-bracket law: never move payload-scale memcpys onto the worker). Default derives from the negotiated transport payload size: payload/8 = 128 KiB at the shipped 1 MiB geometry, bracketing the measured hop-vs-inline-copy crossover (~2 cross-thread wakes + 2 schedules vs a DRAM-bandwidth merge). Explicit value wins verbatim."),
    k("SQUEEZEFS_FUSE_PLACED_MERGE", Kind::Retired { successor: "(deleted — FUSE placed-merge was falsified; IL placed_sever is not this knob)" }, "-", "Retired (class-1 delete, one-path P1): FUSE placed-merge was falsified at ~25% cohort capture (.benchmarks/2026-08-09-fuse-placed-merge.md). IL placed_sever is a different path and is not this knob."),
    k("SQUEEZEFS_FUSE_ZC_RETENTION", Kind::Bool, "on", "zc payload-retention ARMING (kernel 0029, design-zc-write-kernel-v2 §6.1): where the RELEASE_PAYLOAD opcode probes Present and the session runs zc, REGISTER carries FUSE_URING_PAYLOAD_RETENTION. Default ON because arming alone is bit-identical (§3.6 — zero RETAIN commits ride the wire until the ACK-early daemon posture engages, a separate lever); pre-0029/stock kernels decline loud-informational and keep ACK-after-CQE. `0` = the A/B escape. Arm proof: fuse3_zc_retention_negotiated + the mount-log buffers= verdict."),
    k("SQUEEZEFS_ZC_ACK_EARLY", Kind::Bool, "on", "ACK-early for device-overlay stores: eligible overlay stores reply while the device DMA runs. Page-cache-sound writes retain the zc slot (kernel 0029). O_DIRECT overlay extracts at delivery (batched) and ACKs on the owned Bytes. Overlay is default ON, so this lever is live on a shipped mount. Coverage/publication/fsync/read-wait stay CQE-anchored; acked custody retries forever (overlay_ack_early_retries). `0` = ACK-after-CQE A/B. Engagement: overlay_ack_early_stores/_bytes."),
    k("SQUEEZEFS_ZC_ACK_EARLY_ODIRECT", Kind::Bool, "off", "Lets a HELD O_DIRECT/GUP overlay slot ACK early by snapshotting (extract) before the reply — bytes sampled at ACK, so a post-ACK buffer reuse cannot change what lands (the 2026-08-09 live-smoke aliasing). Production O_DIRECT overlay does not HOLD: it extracts at delivery and rides Bytes ACK-early without this knob. `1` is the labeled held-GUP snapshot posture (R4 / in-process slot seam). Engagement: overlay_ack_early_stores + fuse3_zc_write_extract_bytes (retain_commits stay on the page-cache arm)."),
    k("SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS", int(100, 600_000), "30000", "zc bridge-op deadline, ms (the bounded-outcome law): past it the worker pushes AsyncCancel and the op's CQE resolves through the loud fallback ladders (fuse3_zc_bridge_cancels)."),
    k("SQUEEZEFS_TEST_ZC_DROP_WRITE_CQES", int(0, 1_000_000), "0", "Test seam: the zc worker consumes-and-drops the first N WRITE-class bridge CQEs (pend + deadline stay live) — the deterministic lost-CQE interleave of the zcws-9 W4 wedge. Never set in production."),
    k("SQUEEZEFS_FUSE_NO_KILLPRIV", Kind::Bool, "off", "TESTING escape: refuse FUSE_HANDLE_KILLPRIV_V2 and restore the kernel's per-write GETXATTR probe."),
    k("SQUEEZEFS_FUSE_ATTR_TTL_MS", int(0, MS_MAX), "1000", "Kernel attribute-cache TTL, ms (-o attr_timeout overrides)."),
    k("SQUEEZEFS_FUSE_ENTRY_TTL_MS", int(0, MS_MAX), "1000", "Kernel dentry TTL, ms (-o entry_timeout overrides)."),
    k("SQUEEZEFS_FUSE_DIR_ENTRY_TTL_MS", int(0, MS_MAX), "1000", "Kernel directory-entry TTL, ms (-o dir_entry_timeout overrides)."),
    k("SQUEEZEFS_FUSE_NEGATIVE_TTL_MS", int(0, MS_MAX), "1000", "Kernel negative-entry TTL, ms (-o negative_timeout overrides)."),
    k("SQUEEZEFS_TRANSPORT_MEM_MAX", int(1, 1 << 30), "derived", "Transport payload-buffer cap, MiB (absolute > pct > derived)."),
    k("SQUEEZEFS_TRANSPORT_MEM_PCT", int(1, 100), "derived", "Transport payload-buffer cap as a percentage of the R5 budget."),
    k("SQUEEZEFS_TRANSPORT_DEBUG", Kind::Bool, "off", "Verbose transport tracing (development)."),
    // -- L4 interception: daemon side ------------------------------------
    k("SQUEEZEFS_IPC", Kind::Bool, "off", "Env half of `-o interception` (arms the session host)."),
    k("SQUEEZEFS_IPC_ALLOW_DEV", Kind::Bool, "off", "Admit degenerate (`unknown`/`-dirty`) build identities past the KD-7 skew gate — DEV ONLY, announced loudly on both ends (ENG-11)."),
    k("SQUEEZEFS_IPC_ARENA_MB", int(1, 1 << 20), "derived", "Per-session payload arena, MiB."),
    k("SQUEEZEFS_IPC_ARENA_THP", Kind::Bool, "on", "PMD-align + MADV_COLLAPSE session arenas; 0 disables."),
    k("SQUEEZEFS_IPC_IDLE_SECS", int(0, 86_400), "300", "Idle-session reap window, seconds; 0 disables."),
    k("SQUEEZEFS_IPC_INVAL_WINDOW_MS", int(1, MS_MAX), "1000", "W1 invalidation coalescing window, ms."),
    k("SQUEEZEFS_IPC_MAX_OP_BYTES", int(1, BYTES_MAX), "layout default", "Per-op payload ceiling, bytes."),
    k("SQUEEZEFS_IPC_MEM_MAX", int(1, 1 << 30), "derived", "Session-arena admission cap, MiB (absolute > pct > derived)."),
    k("SQUEEZEFS_IPC_MEM_PCT", int(1, 100), "derived", "Session-arena admission cap as a percentage of the R5 budget."),
    k("SQUEEZEFS_IPC_SERVICE_THREADS", int(1, 1 << 12), "derived clamp(3*cpus/8,2,64)", "Service-thread ceiling (threads spawn on session admission; the drain-lane pair's submitter half — same derivation as DD_SHARDS)."),
    k("SQUEEZEFS_IPC_DD_SHARDS", int(1, 1 << 12), "derived clamp(3*cpus/8,2,64)", "Direct-drive uring shards (one ring + pinned reaper per shard, one lane per service thread; override/measurement lever)."),
    k("SQUEEZEFS_IPC_DD_EAGER_FLUSH", int(0, 1 << 12), "0 (sweep-only)", "Direct-drive issue cadence: explicit K > 0 enters inline once a lane's unflushed SQE count reaches K (counted measurement lever); absent/0 = sweep-only flush — re-proven optimal on the r5 CALIBRATED venue (955k vs adaptive 822k vs K=16 878k at 32x32; the r4 adaptive default was a miscalibrated-venue artifact)."),
    k("SQUEEZEFS_IPC_DD_INLINE_REAP", Kind::Bool, "on", "Reaper/drain fusion: the owning service thread drains its lane's direct-drive CQ inline (zero syscall); 0 = the A/B lever (reaper-only completion, the pre-fusion posture). Auto-disarmed on kernels without IORING_ENTER_EXT_ARG."),
    k("SQUEEZEFS_IPC_SOCKET_DIR", Kind::Str, "derived (XDG_RUNTIME_DIR / /run)", "Rendezvous socket directory; `none` disables the filesystem-path socket."),
    k("SQUEEZEFS_IPC_SPIN_US", int(0, 1 << 20), "0", "Service-thread empty-pass spin window, µs — an explicit fleet lever (nonzero taxes the sync lane)."),
    // -- L4 interception: client (shim) side -----------------------------
    k("SQUEEZEFS_IL_SESSIONS", int(1, 1 << 12), "derived clamp(cpus/4,2,16)", "Per-mount fd-shard session count (override lever; the default ties to the daemon's ceiling)."),
    k("SQUEEZEFS_IL_SPINS", int(0, 1 << 24), "adaptive", "Fixed completion spin count (pins the adaptive spin for measurement)."),
    k("SQUEEZEFS_IL_MAX_RUN_SLOTS", int(0, 1 << 16), "0 (unbounded)", "Cap on concurrently claimed slots per session."),
    k("SQUEEZEFS_IL_OP_TIMEOUT_MS", int(1, MS_MAX), "bounded default", "Per-op bounded wait against a stalled daemon, ms."),
    k("SQUEEZEFS_IPC_DD_LANE_FLUSH", Kind::Bool, "on", "Direct-drive flush scope: on = a service thread enters only its OWN lane's ring (the r3 drain-funnel fix — flush-all serialized every svc thread on every shard's kernel uring_lock); 0 = the pre-r3 flush-all sweep (A/B lever)."),
    k("SQUEEZEFS_IL_REAP_PARK_MAX", int(0, 1 << 16), "2", "libaio reap: queue depth at or below which the event-driven park is used."),
    k("SQUEEZEFS_IL_REAP_QUANTUM_US", int(1, 1000), "50", "libaio reap: deep-regime quantum, µs — since reap-fanin 2026-08-08 the batch-threshold park's AGE BOUND (the k-th completion cuts it short); the shipped 50 µs is the 2026-07-26 sizing (shim-iops measurement lever)."),
    k("SQUEEZEFS_IL_READ_DEST", Kind::Bool, "on", "Arena-destination read serves (E-IL2); 0 = the A/B lever."),
    k("SQUEEZEFS_IL_DIRECT_WRITE", Kind::Bool, "on", "IL direct-drive WRITE lane (design-il-direct-write §3): W1-patch-shaped ring writes DMA in place on the svc thread's dd lane — no sever bounce on the aligned leg, no handler handoff; 0 = the A0 control (every ring write rides the sever→handoff path, the ipc_dd_write_* ledger stays silent)."),
    k("SQUEEZEFS_IL_KERNEL_LANE_MIN", int(0, BYTES_MAX), "derived (memBW probe x lane-RTT delta, clamp [slab, max_op])", "Hybrid lane gate (D14 corollary): ops STRICTLY larger than this ride the kernel FUSE lane, smaller ops the IPC ring; explicit wins verbatim, 0 = gate off (all eligible ops ring - the A/B lever). Offsetful forms latch a whole description kernel-lane on first trigger (sticky - the offset mirror never flaps)."),
    // -- zcrx read lane ---------------------------------------------------
    k("SQUEEZEFS_ZCRX_LANE", Kind::Bool, "off", "Arm the zcrx read lane."),
    k("SQUEEZEFS_ZCRX_LANE_FORCE_COPY", Kind::Bool, "off", "Test seam: allow the classic-recv backend to arm (copy parity)."),
    k("SQUEEZEFS_ZCRX_LANE_AREA_SIM", Kind::Bool, "off", "Test seam: AREA backend over simulated NIC DMA."),
    k("SQUEEZEFS_ZCRX_LANE_SIM_CHUNK", int(1, BYTES_MAX), "derived", "Simulated area chunk size, bytes (rounded up to a power of two)."),
    k("SQUEEZEFS_ZCRX_LANE_TARGET", Kind::Str, "none", "Lane target tuple (6 comma-separated fields); malformed = lane not armed, loudly."),
    // -- Diagnostics / forensics -----------------------------------------
    k("SQUEEZEFS_OP_PROFILE", Kind::Bool, "off", "Per-op phase histograms + the under-i_rwsem estimator (zero cost off)."),
    k("SQUEEZEFS_FREE_FORENSICS", Kind::Bool, "off", "Capture a backtrace per block free to attribute double frees (expensive)."),
    k("SQUEEZEFS_STATS_KEY_CENSUS", Kind::Bool, "off", "VAL-7a: arm the `.stats` key census (live object keys + per-inode write custody — a debugging surface). The `*_count` replacements stay unconditional."),
    // -- Cluster wire (DLM S3 — the ONE cluster transport) ---------------
    k("SQZ_CLW_RTT_ENDPOINT", Kind::Harness, "loopback", "RTT instrument: dial a REAL coordinator instead of a local loopback listener (tests/cluster_wire_tests.rs `rtt_row`)."),
    k("SQZ_CLW_RTT_SECRET_HEX", Kind::Harness, "none", "RTT instrument: the target volume set's job:enroll secret, hex (required for a remote row)."),
    k("SQZ_CLW_RTT_SAMPLES", Kind::Harness, "2000", "RTT instrument: counted samples after the discarded warm-up."),
    k("SQZ_CLW_RTT_PAYLOAD", Kind::Harness, "0", "RTT instrument: payload bytes per direction."),
    k("SQUEEZEFS_CLUSTER_WIRE_SVC_THREADS", int(1, 1 << 12), "derived clamp(cpus/8, 1, 8)", "Owner-side RPC lanes (pinned service threads — §6.7's venue rule: never the conveyor's task). Absolute override; still clamped to the core count so it cannot oversubscribe the box."),
    k("SQUEEZEFS_MULTI_WRITER", Kind::Bool, "off", "DLM S7/S9: arm the MULTI-WRITER planes — a device-enforced WERO (rtype 2) hold on every data namespace, remote write custody (grant/renew/revoke/expire) served to peers, metadata ownership, and the daemon's publish path on the wire. Refuses the mount loud, naming the missing piece: a substrate without NVMe reservation support (every loop device, including tests/dev_substrate.sh's default — §6.9's S9 guarantee is 'refused on non-PR'), a format missing one of the six capability bits 7/9/10/11/13/14 (nothing stamps them — ruling D9), a membership plane that is off (an unseeable co-writer is an unevictable one), no durable writer era, or SQUEEZEFS_MW_BIND=off. Off = the shipped single-writer posture: the D0 guard arbitrates and the data plane is fenced locally by the custody epoch."),
    k("SQUEEZEFS_MW_ROLE", Kind::Enum(&["authority", "co-writer"]), "authority", "DLM S9: which HALF of the multi-writer plane this mount is. `authority` (the default, and today's posture verbatim) holds the D0 claim, serves custody + the publish path, and is the only process that writes the claim-set roster. `co-writer` demands the CO-WRITER posture — metadata read-only locally with mutations shipped to the authority, data read-write under a granted custody lease — and REFUSES the mount unless all five admission rungs hold (see `docs/operations.md` §Multi-writer co-writer mounts). Read only when SQUEEZEFS_MULTI_WRITER is on; a co-writer role without the opt-in refuses rather than silently mounting as an authority, because the D0 refusal must never be bypassed by inference."),
    k("SQUEEZEFS_MW_AUTHORITY", Kind::Str, "none", "DLM S9: the `addr:port` a CO-WRITER dials for write custody and the shipped publish path (the authority's SQUEEZEFS_MW_BIND endpoint). Required for `SQUEEZEFS_MW_ROLE=co-writer` — a co-writer with no custody source is inert, so an absent value refuses the mount. Named residual: the endpoint is not published in any durable record yet, so an operator declares it here exactly as the membership and job-wire binds are declared."),
    k("SQUEEZEFS_MW_MEMBERS", Kind::Str, "none", "DLM S9: the AUTHORITY's operator-declared co-writer roster — a comma list of node ids (`node_{16 hex}`, printed by a refused co-writer's own mount log). The authority commits one durable claim-set member entry per rostered id (§6.2 item 7), which is how a co-writer becomes enrolled without being able to commit: enrollment is an act of the authority, never a claim the joining node makes about itself. Read only when SQUEEZEFS_MULTI_WRITER is on and the volume set carries incompat bit 14; an unrostered node is refused at admission naming its own id."),
    k("SQUEEZEFS_MW_BIND", Kind::Str, "auto", "DLM S9: where this mount serves the write-custody and publish authority — `auto` (0.0.0.0:0, ruling D2's posture; the default), an explicit `addr:port`, or `off`. `off` REFUSES a multi-writer arm rather than arming an inert one that grants no custody and serves no peer's publish path. Read only when SQUEEZEFS_MULTI_WRITER is on; a malformed address refuses rather than binding somewhere the operator did not ask for."),
    // -- Metadata function shipping (DLM S8) -----------------------------
    k("SQUEEZEFS_META_SHIP_BATCH_MAX", int(1, 4096), "derived clamp(cpus x 2, 64, 4096)", "Verbs per shipped metadata frame — the S8 pipelining unit. Derived like the M7 commit-conveyor batch cap, because a frame's ops become that many transactions on the owner's conveyor; a frame also stays inside the cluster wire's CONTROL class cap."),
    k("SQUEEZEFS_META_SHIP_DEDUP_MAX", int(1, 1 << 24), "derived max(batch_max x 128, 8192)", "Owner-side idempotency window entries — how far back a client's retry may reach and still be answered from its original outcome (S8 requirement 6). Retiring an entry only weakens exactly-once for a retry arriving after that many newer ops from the SAME client."),
    k("SQUEEZEFS_DLM_TOKEN_CACHE_MAX", int(1, 1 << 24), "derived max(R5 budget/8192/32 B, 4096)", "Client fencing-token cache entries (S8's resolution of the S4 fencing-read contract; spec §6.5 item 1 requires >= 99.5 % of lock operations served locally). Every eviction costs one loud miss and one shipped getattr refresh, never a wrong answer."),
    // -- Membership plane (DLM S6 — liveness off the journal) ------------
    k("SQUEEZEFS_MEMBERSHIP_BIND", Kind::Str, "off", "DLM S6: where this mount serves the membership plane — `off` (default), `auto` (0.0.0.0:0, ruling D2's posture), or an explicit `addr:port`. Armed, a WRITE mount becomes the lease authority (its census is what `squeezefs clients` reads, and readers become visible for the first time) and a READ-ONLY mount joins as a member; the durable footprint is ONE rendezvous record written at arm. Default off because flipping it changes what an operator sees, and the measured validation that would justify a new default is deferred (ruling D11)."),
    k("SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS", int(1_000, MS_MAX), "45000 (= CLIENT_STALE_TTL_SECS)", "DLM S6: the OWNER's lease TTL, ms. Defaults to the ONE staleness law's 45 s so `live`/`stale` means the same thing on the plane and in the `client:`/`writer_claim` records. The member's own deadline is always stricter: T_self = T_owner − 2·skew_max − D_purge."),
    k("SQUEEZEFS_MEMBERSHIP_SKEW_MAX_MS", int(1, 60_000), "derived max(T_owner × 500 ppm, observed RTT)", "DLM S6 (spec §6.7): clock-skew bound between owner and member, ms. Derived from the physical monotonic-clock rate bound (500 ppm ⇒ 22.5 ms at a 45 s TTL) and the measured renewal RTT, whichever is larger. Raising it shortens T_self; a value that collapses T_self REFUSES the plane rather than clamping."),
    k("SQUEEZEFS_MEMBERSHIP_PURGE_MS", int(0, MS_MAX), "derived max(2 × checkpoint cadence, observed RTT)", "DLM S6 (spec §6.7's D_purge): how long a member may take to fail-stop — purge cached custody / stop in-flight DMA — once it decides to. Derived from two revalidation cadences (observe, then finish) floored at one RTT. It is subtracted from the member's deadline, so an honest value is a safety input, not a tuning knob."),
    k("SQUEEZEFS_MEMBERSHIP_GRACE_MS", int(0, MS_MAX), "derived (= the lease TTL)", "DLM S6 (spec §6.7 Recovery): the successor owner's failover grace window, ms — reclaim admitted, conflicting fresh acquires refused, closing early when every prior member has re-asserted. `0` opens no window, which is the forced-flush-storm posture the spec names; the default gives every previously-live member a full lease period to re-assert."),
    // -- Freed-offset grace period (spec §6.8 item 3) ---------------------
    k("SQUEEZEFS_FREE_GRACE_MAX_MS", int(1_000, MS_MAX), "derived (2 x one acknowledgement cycle)", "Spec §6.8 item 3: how long a terminally-freed offset may wait for a reader's acknowledgement before that reader is FENCED (evicted through S6) and the offset released — 'a reader that fails to acknowledge is fenced, not waited on', because an unbounded wait converts a slow reader into the writer's ENOSPC. Derived default = 2 × `free_grace::ack_cycle` (3 × renewal interval + 3 × reader staleness bound + skew_max + D_purge), i.e. one missed cycle tolerated. A value BELOW one cycle REFUSES rather than clamping: it would fence readers answering exactly as designed. Under space pressure allocation evaluates half this bound, floored at one cycle. Inert unless a reader plane is armed."),
    k("SQUEEZEFS_FREE_GRACE_MAX_OFFSETS", int(1, 1 << 26), "derived max(budget/1024/24 B, 131072)", "Spec §6.8 item 3: per-volume cap on offsets held in the freed-offset grace period. Derived from the R5 budget (< 0.1 % of it in ring memory) with a FIELD floor — the measured 12.7 GB/s saturated ingest over one default acknowledgement cycle displaces ≈ 120 k 4 MiB blocks, so a smaller floor would fence a healthy reader merely because the writer is fast. At cap the ring forces progress through the same fence act the deadline uses: never a silent early release, never unbounded RAM."),
    // -- fsck / jobs ------------------------------------------------------
    k("SQUEEZEFS_FSCK_SETTLE_MS", int(0, MS_MAX), "2000", "fsck suspect-settle window, ms (the zero-FP ladder)."),
    k("SQUEEZEFS_JOB_WIRE_BIND", Kind::Str, "off", "Job-shard execution wire listen address (VAL-6: the posture switch)."),
    k("SQUEEZEFS_JOB_WIRE_CA_CERT", Kind::Str, "none", "Job-wire CA certificate (PEM or DER)."),
    k("SQUEEZEFS_JOB_WIRE_CA_KEY", Kind::Str, "none", "Job-wire CA private key (PEM or DER)."),
    k("SQUEEZEFS_JOB_WIRE_ENROLL_FRESHNESS_MS", int(0, MS_MAX), "VAL-6 default", "Enrollment challenge freshness window, ms. 0 is refused by the job-wire loader itself, whose message explains WHY (it would expire every challenge) — the registry leaves that refusal where the better wording lives."),
    k("SQUEEZEFS_JOB_WIRE_MAX_CONNS", int(0, 1 << 20), "derived from cores", "Concurrent job-wire connection cap. 0 is refused by the job-wire loader itself (it would refuse every worker) — same reasoning as the freshness knob."),
    k("SQUEEZEFS_JOB_WIRE_VERIFY_PERMILLE", int(0, 1000), "1000 plaintext / sampled TLS", "Pre-publish verify-read sampling, per mille."),
    // -- NVMe-oF target management ---------------------------------------
    k("SQUEEZEFS_NVMEOF_TARGET_STACK", Kind::Enum(&["spdk", "nvmet"]), "probed", "Target stack selection; a malformed value never falls back to a default."),
    k("SQUEEZEFS_NVMEOF_STATE_DIR", Kind::Str, "/var/lib/squeezefs/nvmeof", "Durable target-ledger directory."),
    k("SQUEEZEFS_NVMEOF_RUN_DIR", Kind::Str, "/run/squeezefs/nvmeof", "Runtime directory for target sockets/pids."),
    k("SQUEEZEFS_NVMET_PORT_ID_BASE", int(0, u32::MAX as i128), "derived", "kernel-nvmet port-id allocation base."),
    k("SQUEEZEFS_SPDK_TGT_BIN", Kind::Str, "pinned install", "Override the spdk_tgt binary path."),
    // -- Local file I/O ---------------------------------------------------
    k("SQUEEZEFS_URING_FS_WORKERS", int(1, 64), "derived", "`uring_fs` process-worker count for ad-hoc local file I/O."),
    // -- Test seams (product code, armed only by suites/rigs) ------------
    k("SQUEEZEFS_TEST_UPLOAD_STALL_MS", int(0, MS_MAX), "0", "Test seam: stall each block upload, ms."),
    k("SQUEEZEFS_TEST_WRITE_STALL_MS", int(0, MS_MAX), "0", "Test seam: stall the write handler, ms (the transport-lease-overlong pin)."),
    k("SQUEEZEFS_TEST_RECLAIM_STALL_MS", int(0, MS_MAX), "0", "Test seam: stall each reclaim batch, ms."),
    k("SQUEEZEFS_TEST_THP_PREP_STALL_MS", int(0, MS_MAX), "0", "Test seam: stall each deferred session-arena THP prep job, ms (the fleet-launch admission pin)."),
    k("SQUEEZEFS_TEST_INVAL_TAIL_STALL_MS", int(0, MS_MAX), "0", "Test seam: stall the detached pipeline upload before its invalidation tail, ms (the 2026-08-04 tail-vs-writer race pin)."),
    k("SQUEEZEFS_TEST_CHECKOUT_STALL_MS", int(0, MS_MAX), "0", "Test seam: stall the write handler post-checkout inside its held block-lock window, ms (pairs with the inval-tail stall)."),
    k("SQUEEZEFS_TEST_ATTR_PUBLISH_STALL_MS", int(0, MS_MAX), "0", "Test seam: stall the write handler between its data phase and its size/attr postlude publish, ms (the KD-5/KD-6 attr-merge race pin — design-write-inode-convoy)."),
    k("SQUEEZEFS_TEST_NVME_READ_TIMEOUT_MS", int(1, MS_MAX), "30000", "Test seam: NVMe read timeout, ms."),
    k("SQUEEZEFS_TEST_POWER_CUT_DEVS", Kind::Str, "none", "Test seam: comma list of device paths armed for the power-cut simulator."),
    k("SQUEEZEFS_TEST_OVERLAY_BYTES", Kind::Bool, "0", "Test seam: force the overlay Bytes vehicle on even when SQUEEZEFS_DEVICE_OVERLAY is off (in-process suites). Production overlay already accepts Bytes (O_DIRECT at-delivery extract / IL severs). `set_device_overlay_for_tests` still pins off for slot-only suites."),
    k("SQUEEZEFS_TEST_STAMP_BLOCK_REFS", Kind::Bool, "0", "Test seam: stamp incompat bit 8 (durable block refcounts) at format, so a suite can point the write-path fixtures at the durable ledger and let the §6.2-item-1 oracle grade them. Never set in production — `format` never stamps bit 8 (ruling D9)."),
    k("SQUEEZEFS_TEST_STAMP_WRITER_SCOPE", Kind::Bool, "0", "Test seam: stamp incompat bit 10 (writer-scoped staging — §6.2 items 8/10) at format, so a suite can drive the scoped-key + node-scoped-stamp path end to end. Never set in production — `format` never stamps bit 10 (ruling D9)."),
    k("SQUEEZEFS_NODE_ID_FILE", Kind::Str, "none", "Node-identity source file, ahead of /etc/machine-id, /var/lib/dbus/machine-id and /etc/squeezefs/node-id (writer-scoped staging, §6.2 item 10). Also the suites' two-node seam; the file's bytes must be host-stable and reboot-stable."),
    // -- Harness-only variables (suites, rigs, re-exec children) ---------
    k("SQUEEZEFS_TEST_REQUIRE_MOUNT", Kind::Harness, "-", "Turn mount-class skips into failures (TEST-2)."),
    k("SQUEEZEFS_TEST_REQUIRE_ALL", Kind::Harness, "-", "Promote every skip class to a failure."),
    k("SQUEEZEFS_TEST_REQUIRE_HARDWARE", Kind::Harness, "-", "Promote hardware-class skips."),
    k("SQUEEZEFS_TEST_REQUIRE_NON_ROOT", Kind::Harness, "-", "Promote non-root-class skips."),
    k("SQUEEZEFS_TEST_REQUIRE_OPT_IN", Kind::Harness, "-", "Promote opt-in-class skips."),
    k("SQUEEZEFS_TEST_REQUIRE_ROOT", Kind::Harness, "-", "Promote root-class skips."),
    k("SQUEEZEFS_TEST_REQUIRE_SUDO", Kind::Harness, "-", "Promote sudo-class skips."),
    k("SQUEEZEFS_TEST_REQUIRE_TOOLCHAIN", Kind::Harness, "-", "Promote toolchain-class skips."),
    k("SQUEEZEFS_TEST_SKIP_LEDGER", Kind::Harness, "-", "Path the skip ledger is appended to."),
    k("SQUEEZEFS_CRASH_CHILD_V3", Kind::Harness, "-", "KV crash-suite re-exec child marker."),
    k("SQUEEZEFS_CRASH_CHILD_V3_BATCHED", Kind::Harness, "-", "KV crash-suite batched-child marker."),
    k("SQUEEZEFS_CRASH_LEDGER", Kind::Harness, "-", "KV crash-suite ledger path."),
    k("SQUEEZEFS_CRASH_ROUNDS", Kind::Harness, "-", "KV crash-suite round count."),
    k("SQUEEZEFS_CRASH_VOL", Kind::Harness, "-", "KV crash-suite volume path."),
    k("SQUEEZEFS_GUARD_CHILD", Kind::Harness, "-", "Writer-guard suite child marker."),
    k("SQUEEZEFS_GUARD_HOLD_CHILD", Kind::Harness, "-", "Writer-guard hold-child marker."),
    k("SQUEEZEFS_GUARD_OUT", Kind::Harness, "-", "Writer-guard suite output path."),
    k("SQUEEZEFS_GUARD_READY", Kind::Harness, "-", "Writer-guard readiness handshake path."),
    k("SQUEEZEFS_GUARD_VOL", Kind::Harness, "-", "Writer-guard suite volume path."),
    k("SQUEEZEFS_WCE_CRASH_CHILD", Kind::Harness, "-", "Volatile-cache crash-suite child marker."),
    k("SQUEEZEFS_WCE_CRASH_ROUNDS", Kind::Harness, "-", "Volatile-cache crash-suite round count."),
    k("SQUEEZEFS_WCE_LEDGER", Kind::Harness, "-", "Volatile-cache crash-suite ledger path."),
    k("SQUEEZEFS_WCE_VOL", Kind::Harness, "-", "Volatile-cache crash-suite volume path."),
    k("SQUEEZEFS_M1_ROOT_DEV", Kind::Harness, "-", "Meta-slot migration suite device path."),
    k("SQUEEZEFS_M1_ROOT_VICTIM_KEY", Kind::Harness, "-", "Meta-slot migration suite victim key."),
    k("SQUEEZEFS_TEST_REQUIRE_CAPABILITY", Kind::Harness, "-", "Promote capability-class skips."),
    k("SQUEEZEFS_RECLAIM_CRASH_CHILD", Kind::Harness, "-", "Block-reclaim crash-suite child marker."),
    k("SQUEEZEFS_RECLAIM_CRASH_DEV", Kind::Harness, "-", "Block-reclaim crash-suite device path."),
    k("SQUEEZEFS_RECLAIM_CRASH_LEDGER", Kind::Harness, "-", "Block-reclaim crash-suite ledger path."),
    k("SQUEEZEFS_RECLAIM_CRASH_ROUNDS", Kind::Harness, "-", "Block-reclaim crash-suite round count."),
    k("SQUEEZEFS_STAGED_CRASH_CHILD", Kind::Harness, "-", "Staged-payload crash-suite child marker."),
    k("SQUEEZEFS_STAGED_CRASH_MANIFEST", Kind::Harness, "-", "Staged-payload crash-suite manifest path."),
    k("SQUEEZEFS_STAGED_CRASH_META", Kind::Harness, "-", "Staged-payload crash-suite meta path."),
    k("SQUEEZEFS_STAGED_CRASH_STAGING", Kind::Harness, "-", "Staged-payload crash-suite staging path."),
    k("SQUEEZEFS_SCALE_DIR_ENTRIES", Kind::Harness, "-", "Directory-scale suite entry count."),
    k("SQUEEZEFS_SKIP_LEDGER_CHILD", Kind::Harness, "-", "Skip-ledger suite child marker."),
    k("SQZ_ALLOC_TRACE", Kind::Harness, "-", "ipc op-economy allocation-site profiler."),
    k("SQZ_BSET_ALLOC_CHILD", Kind::Harness, "-", "Bset allocation-authority repro child marker."),
    k("SQZ_FENCING_REMOUNT_CHILD_DIR", Kind::Harness, "-", "Fencing-remount suite child directory."),
    k("SQZ_VAL3_HARDEN_CHILD", Kind::Harness, "-", "Key-handling hardening repro child marker."),
];

/// Registry lookup.
pub fn lookup(key: &str) -> Option<&'static Knob> {
    KNOBS.iter().find(|k| k.key == key)
}

/// Whether a name is in our namespace at all (the only names this module
/// has an opinion about).
pub fn is_ours(key: &str) -> bool {
    key.starts_with("SQUEEZEFS_") || key.starts_with("SQZ_")
}

/// The outcome of validating an environment.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Validation {
    /// Refusals — malformed values and retired spellings, one line each.
    pub errors: Vec<String>,
    /// `SQUEEZEFS_*` / `SQZ_*` names that are not registered: announced as
    /// probable typos, never refused.
    pub unknown: Vec<String>,
}

impl Validation {
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty() && self.unknown.is_empty()
    }
}

/// Validate one `(name, value)` pair. `None` = acceptable.
fn validate_one(knob: &Knob, raw: &str) -> Option<String> {
    let v = Some(raw);
    match knob.kind {
        Kind::Bool => core::parse_bool(knob.key, v).err().map(|e| e.to_string()),
        Kind::Int { lo, hi } => core::parse_int_in::<i128>(knob.key, v, lo, hi)
            .err()
            .map(|e| e.to_string()),
        Kind::Enum(allowed) => core::parse_enum(knob.key, v, allowed)
            .err()
            .map(|e| e.to_string()),
        Kind::Str | Kind::BuildTime | Kind::Harness => None,
        Kind::Retired { successor } => core::present(v).map(|val| {
            format!(
                "{}='{val}' was RETIRED (ENG-10 knob-namespace collision): use {successor} instead",
                knob.key
            )
        }),
    }
}

/// Validate an arbitrary environment (injectable — the tests drive this
/// form; `validate_environment` is the `std::env::vars()` wrapper).
pub fn validate_vars<I, K, V>(vars: I) -> Validation
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut out = Validation::default();
    for (key, value) in vars {
        let key = key.as_ref();
        if !is_ours(key) {
            continue;
        }
        match lookup(key) {
            Some(knob) => {
                if let Some(err) = validate_one(knob, value.as_ref()) {
                    out.errors.push(err);
                }
            }
            None => out.unknown.push(key.to_string()),
        }
    }
    out.errors.sort();
    out.unknown.sort();
    out
}

/// Validate the live process environment.
pub fn validate_environment() -> Validation {
    validate_vars(std::env::vars())
}

/// The startup gate (called from `main` before anything is opened or
/// mounted): announce unknown names, and REFUSE — loudly, naming every
/// offender at once — on any malformed value or retired spelling.
///
/// Returns the human-readable refusal block when the environment is
/// unusable, so the caller owns the exit (the `--log-file` preflight
/// precedent: print, exit 1, mount nothing).
pub fn refusal_report() -> Option<String> {
    let v = validate_environment();
    for name in &v.unknown {
        eprintln!(
            "Warning: {name} is not a SqueezeFS knob (typo? `src/env_knobs.rs` \
             is the registry) — ignored"
        );
    }
    if v.errors.is_empty() {
        return None;
    }
    let mut s = String::from("refusing to start: invalid SqueezeFS environment knob(s)\n");
    for e in &v.errors {
        s.push_str("  - ");
        s.push_str(e);
        s.push('\n');
    }
    s.push_str(
        "  (a malformed knob is never silently defaulted — fix or unset it; \
         empty means unset)",
    );
    Some(s)
}

/// Read a boolean knob under the ONE convention: `1/true/yes/on` enables,
/// `0/false/no/off` disables, absent keeps `default`.
///
/// This replaces the 19 presence-based sites where `env::var(..).is_ok()`
/// meant `SQUEEZEFS_FREE_FORENSICS=0` **enabled** the feature. A malformed
/// value cannot normally reach here (the startup gate refused the process),
/// so the remaining case is a library embedding or a test mutating the
/// environment mid-process: announce it and keep the documented default —
/// never silently, never a panic inside a `OnceLock` initializer.
pub fn bool_knob(key: &str, default: bool) -> bool {
    let raw = std::env::var(key).ok();
    match core::parse_bool(key, raw.as_deref()) {
        Ok(Some(v)) => v,
        Ok(None) => default,
        Err(e) => {
            log::error!("{e} — keeping the default ({default})");
            default
        }
    }
}

/// Read an integer knob under the ONE convention (absent or malformed ⇒
/// `default`, malformed announced). The startup gate owns the refusal; this
/// is the in-process reader for the same law.
pub fn int_knob<T>(key: &str, default: T) -> T
where
    T: std::str::FromStr + std::fmt::Display + Copy,
{
    let raw = std::env::var(key).ok();
    match core::parse_int::<T>(key, raw.as_deref()) {
        Ok(Some(v)) => v,
        Ok(None) => default,
        Err(e) => {
            log::error!("{e} — keeping the default ({default})");
            default
        }
    }
}

/// Read an OPTIONAL integer knob: `None` when absent, and `None` (with the
/// announcement) when malformed — for the knobs whose absence means
/// "derive it", where there is no default number to fall back to.
pub fn opt_int_knob<T>(key: &str) -> Option<T>
where
    T: std::str::FromStr,
{
    let raw = std::env::var(key).ok();
    match core::parse_int::<T>(key, raw.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            log::error!("{e} — ignoring the override");
            None
        }
    }
}

/// Read a one-of-a-set knob: the canonical spelling when set and valid,
/// `default` when absent, and `default` (announced) when the value is not in
/// the set. `allowed` must be the same set the registry declares — the
/// contract test pins that.
pub fn enum_knob(key: &str, allowed: &[&'static str], default: &'static str) -> &'static str {
    let raw = std::env::var(key).ok();
    match core::parse_enum(key, raw.as_deref(), allowed) {
        Ok(Some(v)) => v,
        Ok(None) => default,
        Err(e) => {
            log::error!("{e} — keeping the default ({default})");
            default
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_no_duplicate_names() {
        let mut seen: Vec<&str> = KNOBS.iter().map(|k| k.key).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(before, seen.len(), "duplicate knob registrations");
    }

    #[test]
    fn every_knob_documents_a_default_and_a_purpose() {
        for k in KNOBS {
            assert!(!k.default.is_empty(), "{} has no documented default", k.key);
            assert!(k.doc.len() > 10, "{} has no real doc line", k.key);
        }
    }
}
