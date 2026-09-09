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
    k("SQUEEZEFS_BUILD_PROFILE", Kind::BuildTime, "cargo", "Cargo profile name the binary was built under (release = thin LTO; dist = fat LTO, tagged releases only)."),
    k("SQUEEZEFS_BUILD_TIMESTAMP", Kind::BuildTime, "git", "UTC RFC3339 build timestamp (honors SOURCE_DATE_EPOCH)."),
    k("SQUEEZEFS_IL_BUILD_COMMIT", Kind::BuildTime, "git", "The shim's build commit (KD-7 daemon/shim pairing)."),
    // -- CLI / process ---------------------------------------------------
    k("SQUEEZEFS_META_URI", Kind::Str, "none", "Default sqmeta:// URI for the verbs that take one (clap env)."),
    k("SQUEEZEFS_HOSTNQN", Kind::Str, "none (/etc/nvme/hostnqn)", "KD-MW-3 (design-full-multi-writer §5.2): per-mount NVMe-oF host NQN — every fabrics connection this daemon makes presents it, making the mount its own PR registrant. PAIR-OR-NEITHER with SQUEEZEFS_HOSTID (exactly one configured refuses the mount — a mismatched pair is how registrants alias); explicit identity REQUIRES daemon-owned data-plane connects from durable fabric_endpoint: records (`config set-fabric-endpoints`) and is verified against the ACTUAL controller identity under every device fd. `-o hostnqn=` wins over the env knob."),
    k("SQUEEZEFS_HOSTID", Kind::Str, "none (/etc/nvme/hostid)", "KD-MW-3: the paired per-mount NVMe-oF host id (see SQUEEZEFS_HOSTNQN — pair-or-neither). `-o hostid=` wins over the env knob."),
    k("SQUEEZEFS_ENCRYPT_KEY_FILE", Kind::Str, "/etc/squeezefs/keys/<id>.key", "Encryption key material path (docs/design-key-handling.md)."),
    k("SQUEEZEFS_DAEMON_PIPE", int(0, i32::MAX as i128), "none", "Internal: fd the forked daemon writes `ready` to. Set by the parent, never by hand."),
    k("SQUEEZEFS_TIMEOUT", int(1, 86_400), "30", "Metadata-operation timeout, seconds (memoized at launch)."),
    k("SQUEEZEFS_SUPERVISE_INTERVAL_SECS", int(1, 86_400), "5", "`mount --supervise` probe interval, seconds."),
    k("SQUEEZEFS_SUPERVISE_UNRESPONSIVE_SECS", int(1, 86_400), "30", "`mount --supervise` unresponsiveness threshold before abort, seconds."),
    // -- Memory budget (R5) ---------------------------------------------
    k("SQUEEZEFS_MEM_BUDGET_MB", int(1, 1 << 30), "derived", "R5 memory budget, MiB (absolute > pct > cgroup/RAM derivation)."),
    // -- Fleet share (KD-MW-14, design-full-multi-writer §5.6) -----------
    k("SQUEEZEFS_FLEET_SHARE", int(1, 4096), "1", "KD-MW-14 (design-full-multi-writer §5.6): N co-located daemons divide the machine — ONE divisor applied at the ROOT INPUTS of the derived-sizing tree (the memory-budget root and the sizing CPU-count root become ceil(system/share)), so every downstream derived formula scales untouched. DERIVED TIER ONLY: explicit absolute/percentage knobs keep winning verbatim; FLOORS are never divided (a share whose divided budget cannot satisfy the never-divided transport floor refuses the MOUNT naming the arithmetic); the kernel-mandated-geometry exemption class (the FUSE-over-uring queue COUNT — one queue per possible CPU or the session never becomes ready) is pinned by tie test. Never auto-detected: set by the operator or the mw_fleet.sh rig. 1 = today's whole-machine posture, byte-identical."),
    // -- Metadata (KV v3) -----------------------------------------------
    k("SQUEEZEFS_META_NODE_CACHE_MB", int(1, 1 << 30), "derived", "Per-volume KV node-cache budget, MiB (absolute wins over _PCT)."),
    k("SQUEEZEFS_META_NODE_CACHE_PCT", int(1, 100), "derived", "Per-volume KV node-cache budget as a percentage of the R5 budget."),
    k("SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES", int(1, 1 << 32), "derived", "Dirty-node checkpoint cap = the mount-replay working-set bound."),
    k("SQUEEZEFS_META_COMMIT_BATCH_TXS", int(1, 1 << 20), "derived", "M7 commit-conveyor batch cap, transactions."),
    k("SQUEEZEFS_META_COMMIT_BATCH_BYTES", int(1, BYTES_MAX), "derived", "M7 commit-conveyor batch cap, bytes (clamped to the ring's admissible capacity)."),
    k("SQUEEZEFS_JOURNAL_LANE", Kind::Bool, "on", "C-2 (e2e perf audit DLM board #3): each writable metadata volume gets its OWN journal lane — one OS thread (sqz-jrnl{N}, derived: one per volume) that runs the commit conveyor's apply pass AND durability lane and owns the volume's journal io_uring, parking IN the ring. Stage A submits a window's entries on that ring (no pool queue hop), the lane reaps its own CQEs (the completion never crosses a thread to reach the durability task, never waits behind the sqz-meta lanes' serve work). Default ON. `0` = the shipped D-2 shape (both stages on the shared sqz-meta pool, writes through the uring_fs pool) — the same-binary A/B control. Engagement: journal_ring_lane_writes ≈ conveyor passes when on, 0 when off (journal_ring_pool_writes carries the rest)."),
    k("SQUEEZEFS_META_FLUSH_INTERVAL_MS", int(0, DAY_MS), "50", "Journal/checkpoint cadence, ms; 0 = strict per-commit. A very large value is the 'park the timer' idiom two suites use, hence the day-long ceiling."),
    k("SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS", int(0, DAY_MS), "unset", "Legacy alias for SQUEEZEFS_META_FLUSH_INTERVAL_MS (the new spelling wins)."),
    k("SQUEEZEFS_META_REVALIDATE_MS", int(1, DAY_MS), "derived", "Coherent-READER node-cache revalidation cadence, ms (spec §6.8 item 2). Derived default = max(flush cadence, the 1 s checkpoint ceiling) — polling faster than the writer mints ledger records buys no freshness and pays a drop pass. Trades staleness (interval + 1 s) against reload cost; inert on write mounts."),
    k("SQUEEZEFS_BLOCK_REFS_VERIFY", Kind::Bool, "off", "Run the durable-vs-derived block-reference oracle at MOUNT (spec §6.2 item 1). Off by default because it pays the inode-tree walk the durable records exist to delete; fsck runs the same comparison unconditionally as class C8."),
    // -- Layout / publish economy ---------------------------------------
    k("SQUEEZEFS_DEFAULT_BLOCK_SIZE", int(4096, BYTES_MAX), "4194304", "Default striped block size, bytes, when format did not record one."),
    k("SQUEEZEFS_INLINE_MAX_BYTES", int(crate::routing::INLINE_MAX_FLOOR as i128, crate::routing::INLINE_MAX_CEILING as i128), "derived value_cap - 4096 (61440 at the 256 KiB node)", "The inline-layout ceiling, bytes: a file up to it keeps its whole payload IN its layout record on the metadata volume — visible to every client of the set at the commit, no block, no promotion step; past it the file is staged (with a staging dir) or striped. Derived per volume as the KV xattr value cap `min(64 KiB, node_size/4)` minus the layout wire's 4 KiB framing headroom — the LARGEST payload whose layout record still fits one KV value (`routing::derived_inline_max_bytes`); the explicit value wins verbatim and is bounded by a smaller-node volume's format bound (logged once). The trade the override prices (the phase-B threshold sweep, `.benchmarks/2026-09-09-fsync-promote-staged-ab.md` §4 (iii)): metadata-plane bytes per small write (every layout commit of an inline file carries its payload through the journal and the node log) vs. a whole striped block per small file at promotion (the 64× space law the pricing found). `4096` = the shipped ceiling (one page), the A/B control; the range's floor is that shipped posture and its top the cap any volume can hold. Published live as `inline_max_bytes` on the stats inode. Engagement: layout_inline_writes vs layout_staged_writes; layout_promoted_inline vs layout_promoted_block."),
    k("SQUEEZEFS_PUBLISH_COALESCE_MAX", int(0, 1 << 20), "64", "Per-ino publish-coalescing window; 1 = the pre-campaign serialized posture (A/B lever)."),
    k("SQUEEZEFS_LAYOUT_DELTA_MAX_CHAIN", int(0, 1 << 20), "64", "Layout delta-record chain cap before a full re-base save; 0 = full saves only (A/B lever)."),
    k("SQUEEZEFS_PUBLISH_COMMIT_GROUP_MAX", int(0, 1 << 20), "0 (derives from META_COMMIT_BATCH_TXS)", "Layout saves aggregated into one multi-ino KvTx per conveyor window; 1 = per-save commits (A/B lever)."),
    k("SQUEEZEFS_KVMAP", Kind::Bool, "on", "PB-class block-map tree (design-kvmap-block-map-tree, PR 2): whether NEW beyond-inline crossings take the tree-7 kvmap arm where the ino's home volume can engage it. 0 = new crossings take the legacy indirect-blob arm — the A/B lever, and it NEVER disables kvmap-head resolution (A10: an existing kvmap: head is force-kvmap regardless)."),
    k("SQUEEZEFS_KVMAP_OVERLAY", Kind::Bool, "on", "PB-class bounded maps (design-kvmap-block-map-tree §14, PR 6c-i): whether kvmap inos whose size-estimated map exceeds the derived budget share take the PARTIAL store (dirty overlay + bounded warm windows + tree read-through + overlay saves). 0 = whole-map RAM at any size — the acceptance bracket control, never an operational escape."),
    k("SQUEEZEFS_MAP_MIGRATE_CHUNK", int(64, 1024), "512", "kvmap crossing-train ops per transaction (the finding-38 BLOCK_REF_TX_CHUNK law). The registry cap keeps a mis-set knob under the 128 KiB whole-journal-entry admission (design A10)."),
    // -- Write path ------------------------------------------------------
    k("SQUEEZEFS_PATCH_MAX_BYTES", int(0, BYTES_MAX), "derived block_size/8", "W1 sole-owner in-place patch ceiling, bytes; 0 = the acceptance A/B lever."),
    k("SQUEEZEFS_FOLD_MAX_EXTENTS", int(0, 1 << 24), "64", "W2 fold trigger: parked extents per block."),
    k("SQUEEZEFS_FOLD_MAX_BYTES", int(0, BYTES_MAX), "derived block_size/4", "W2 fold trigger: parked bytes per block."),
    k("SQUEEZEFS_PARKED_BUFFERS", int(0, 1 << 24), "derived", "W2 parked-write budget in buffers' worth of bytes (× block size)."),
    k("SQUEEZEFS_PARKED_GATE_ASSIST_MS", int(0, MS_MAX), "200", "R5-Red parked-gate self-flush assist window, ms."),
    k("SQUEEZEFS_INPLACE_OVERWRITE", Kind::Bool, "off", "Opt a substrate into in-place eligible full-block overwrites (real-SSD DSM fleets; measured loss on zram-lz4)."),
    k("SQUEEZEFS_DEVICE_OVERLAY", Kind::Bool, "on", "Approach B device-backed visible overlay (design-device-overlay, PR B2; one-path write store): eligible fresh/hole aligned segments store slot/Bytes -> unpublished dest, ACK-early, publication at coverage completion/fsync. Default ON — this is the 35 GB/s A-leg, not an opt-in. `0` = accumulation A/B (the B2 0.77x ACK-after-CQE control). Engagement: overlay_store_bytes / overlay_ack_early_bytes."),
    k("SQUEEZEFS_OVERLAY_OVERWRITE", Kind::Bool, "on", "The overlay OVERWRITE arm (design-overlay-overwrite, PR B4): aligned single-block overwrites of mapped striped passthrough blocks ride the zero-copy slot->device overlay with a fresh CoW dest (never in-place, KD-B4-2) and publish by FEEDING the rewrite epoch (arm (a), KD-B4-1). Default ON (KD-B4-9), FIELD-ADJUDICATED 2026-08-15 (.benchmarks/2026-08-15-overlay-b4-overwrite.md): on the deciding CPU-bound fabric venue (2x200GbE nvme-tcp, 32 CPUs) the arm won BOTH A-B-B-A orders (32.1 vs 31.0 / 31.6 vs 30.9 GiB/s sustained 4MiB overwrite) at HALF the daemon CPU (26.5-27.3 vs 52.7-53.1 jiffies/GiB), engagement exact (overwrite share 0.999, nt_copy share 1.000 -> 0.001 — the 96.6% merge-share wall deleted; rewrite_amp 1.0000). VENUE SPLIT, recorded: device-bound substrates (local zram-tcp devsub) prefer `0` — the B2 control's BDP-depth pipelining wins there (1149-1161 vs 783-940 MiB/s at qd4; the gap closes with client qd). `0` = the B2 fresh-only gate restored verbatim. SQUEEZEFS_DEVICE_OVERLAY=0 disables the whole overlay including this arm. Engagement: overlay_overwrite_installs / overlay_overwrite_bytes (the CQE-counted overwrite subset of overlay_store_bytes)."),
    k("SQUEEZEFS_OVERLAY_CLOSE_BARRIER", Kind::Bool, "off", "OQ-5 (design-overlay-overwrite): barrier the data devices (flush_data_devices) BEFORE the coverage-triggered close of an overlay-fed rewrite epoch, upgrading those saves out of the OW-8 DUR-2 volatility class (power loss after an unbarriered non-fsync save on a write-back namespace can read dest residue). Default OFF — the identical window ships in the rewrite program's own non-fsync closes, so taking it for the overlay alone would be an inconsistent durability posture; B4d prices it (the counted lever). Covers the overlay coverage-close venues; the idle-sweeper close keeps the shipped DUR-2 class either way. fsync-class boundaries are always fully ordered (O2)."),
    k("SQUEEZEFS_REWRITE_SHADOW", Kind::Bool, "on", "Rewrite-program shadow swaps; 0 = the A/B control."),
    k("SQUEEZEFS_REWRITE_SUPPLY_CLOSE", Kind::Bool, "on", "The supply-coupled rewrite-epoch close on CO-WRITER lanes (finding 15's parked-supply term, `.benchmarks/2026-09-07-rewrite-epoch-supply-close.md`; design-rewrite-program §5.3, the KD-1.7 amendment). A rewrite parks each displaced A key in the open epoch until a close trigger fires; on a co-writer the freed key returns only through the recycle loop (ship -> grace ring -> lane list -> harvest RPC), so KD-1.7's close-on-StorageFull-then-retry-once finds nothing, and on a shared-file rewrite no routine trigger fires during an iteration. ON (the default): the laned co-writer's ahead-refill tick closes the mount's open epochs when the lane's reachable supply sits below the refill's own watermark (claim-rate EWMA x refill horizon, capped at lane-share/4 — the blocks one loop transit consumes), largest epoch first until the yield covers the deficit `watermark - reachable` (also the per-tick bound: never more closes than blocks short). The close is the shipped KD-1.4 swap verbatim (one whole-tx publish + the deferred frees) — every crash window holds; adds nothing to the write hot path (the tick's reads only). `0` = the shipped KD-1.6/1.7 triggers only (the A/B lever). Inert on every single-writer / authority mount (installs only on a harvesting lane). Engagement: rewrite_shadow_supply_closes / rewrite_shadow_supply_close_blocks; ledger rewrite_shadow_supply_close_declined_{covered,no_parked} / rewrite_shadow_supply_close_bounded."),
    k("SQUEEZEFS_WRITEBACK_QUEUE_CAP", int(1, 1 << 24), "4096", "Writeback flush-unit queue capacity."),
    k("SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS", int(0, 1 << 20), "derived (BDP)", "Write-pipeline depth target override, blocks — MEASUREMENT lever; the default is the runtime BDP derivation."),
    // -- Block reclaim (device deallocation) -----------------------------
    k("SQUEEZEFS_RECLAIM_BATCH_BLOCKS", int(1, 1024), "64", "Background block-reclaim drain batch, blocks."),
    k("SQUEEZEFS_RECLAIM_BATCH_MS", int(0, MS_MAX), "2", "Background block-reclaim batch-accumulation window, ms."),
    k("SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS", int(1, 1 << 20), "derived (floor 4096)", "Block-reclaim deferred-space cap, blocks (queued + in-flight); at-cap enqueues park (park-don't-spill). W-4 derived default: clamp(measured displacement rate × the drain's room latency, 4096 = the shipped floor, budget/1024 ÷ per-entry RAM); explicit value wins verbatim (4096 = the A/B lever)."),
    k("SQUEEZEFS_RECLAIM_LANES_PER_DEV", int(1, 64), "32", "Parallel reclaim drain lanes per device."),
    k("SQUEEZEFS_ALLOC_LANE_RESERVE_BLOCKS", int(0, 1 << 32), "derived", "Fresh blocks one durable data-plane allocation-lane reservation covers (DLM S9 blocker #3). Derived from the write pipeline's cold window (FLOOR_BLOCKS_PER_LANE × HEADROOM × cpus), floored at the 8-lane cold aggregate and capped at 1/64 of a lane share; 0 = derived, 1 = a commit per fresh block (the pathological A/B control). Inert on every unpartitioned (single-writer) mount."),
    k("SQUEEZEFS_RECLAIM_CAP_PARK_MS", int(0, 60_000), "derived (50..1000)", "At-cap reclaim enqueue park bound, ms, before soft overflow — the safety bound only: the park ends on the drain's room-made edge. W-4 derived default: clamp(4 × batch_blocks ÷ measured drain rate, 50 ms manners tick, 1000 ms = the shipped constant, never longer); explicit value wins verbatim (1000 = the A/B lever, 0 = never park)."),
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
    k("SQUEEZEFS_WRITE_GUARD_NARROW", Kind::Bool, "on", "W-2 write-stream-guard (2026-09-05): the Shared class widens to EVERY cache-resident striped write — the fresh/append (extending) stream and hole-fills included — so a stream's meta-prep takes the inode READ guard (its data path already ran under block locks only). `0` = the pre-campaign class (mapped within-EOF overwrites only; extends take the exclusive drop-before-I/O guard) — the same-binary A/B lever, never an operational escape. Inert under SQUEEZEFS_WRITE_SHARED=0."),
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
    k("SQUEEZEFS_FUSE_IO_URING_SPIN_US", int(0, 100_000), "off (0)", "Spin-before-park CAP for the FUSE-over-io_uring queue workers, us (e2e perf audit R-4, reap-thread economy; .benchmarks/2026-09-03-r4-reap-thread-economy.md). With a nonzero cap, where a worker would block in submit_and_wait it first spins a bounded window watching IORING_SQ_TASKRUN / the CQ tail / the wake coalescer, so an event landing inside it is reaped without a park + scheduler wake. The window is ADAPTIVE: 2x the worker's own park-gap EWMA capped by this value, engaged only while that worker's queues hold ops in flight (an idle queue never spins), refused past the box's queueing knee (busy > 80 %, /proc/stat at the shared 100 ms cadence). Default OFF on measurement: at the field (kern rand-4k 24x8, cap 200) the governor deleted 69 % of the worker's parks and moved no latency term (msg_hop / device_cq / wake_hop unchanged) at +5.5 % CPU/op and -2.7 %/+0.6 % IOPS — the absorbed parks were the <= 16 us class the kernel wait already returns from without sleeping. Registered A/B lever: any nonzero value engages the governor. Engagement: transport_spin_{absorbed,expired,ns,refused_busy,window_us} — absorbed = parks deleted, ns = the CPU paid."),
    k("SQUEEZEFS_FUSE_PIN_SCOPE", Kind::Enum(&["node", "core"]), "node", "Transport thread affinity scope; `core` is the pre-campaign hard-pin posture."),
    k("SQUEEZEFS_FUSE_SAME_LANE_DISPATCH", Kind::Bool, "1", "READ handler futures spawn_local on the dispatching TPC lane instead of round-robining to another lane (transport-ingress lever 1); `0` restores the rotation (A0 control)."),
    k("SQUEEZEFS_FUSE_READ_FAST_DISPATCH", Kind::Bool, "on", "READ fast-dispatch from the reap thread (e2e perf audit R-2, read board #2): the FUSE-over-io_uring queue worker runs the filesystem's SYNC try-only warm probe at the delivery CQE — a warm READ is served + committed inline (no inbound queue, no session dispatch task, no lane: queue_wait == dispatch_lag == 0), a miss mints the full READ handler straight onto a fuse3-tpc lane. `0` = the A/B control (every READ rides the inbound queue as before). Engagement: transport_fast_dispatch_{serves,demotes}."),
    k("SQUEEZEFS_FUSE_DRAIN_GROUP", int(1, 512), "derived (node possible CPUs / 4, floor 1)", "Queues per FUSE-over-io_uring drain context (ingress-queue-spread lever 2); explicit width wins verbatim, `1` = the per-queue-worker A0 control."),
    k("SQUEEZEFS_FUSE_KMBUF", Kind::Bool, "on", "kmbuf reply-buffer negotiation; 0 = the A/B control."),
    k("SQUEEZEFS_FUSE_ZC", Kind::Bool, "on", "FUSE_URING_ZERO_COPY serve integration (K1 kill; sqz kernel + CAP_SYS_ADMIN required — declines loud elsewhere, stock kernels run the bufring path byte-identically). Default ON per ruling D16 (2026-08-07, rc-manifest §3f — supersedes the 0.97x all-write-rows rule): reads +40-75%; known caveat = un-shimmed kernel-lane rand-4k writes pay ~20% (extraction hop) until handler/worker fusion lands; `0` is the escape/A-B lever."),
    k("SQUEEZEFS_FUSE_ZC_WRITE_FUSION", Kind::Bool, "on", "Handler/worker FUSION for small armed FUSE WRITEs. Default ON again (fused-lane-predicate fix, 2026-08-08): the field falsification (0.45x armed rand-4k at fabric RTT) was the shape-only hold PREDICATE double-paying W1-ineligible ops (hold + fused poll + LATE extraction) — the hold now gates on the filesystem's W1-eligibility seam and ineligible shapes extract at delivery on the classic dispatch; fabric-emulated + un-emulated A-B-B-A acceptance in .benchmarks/2026-08-08-fused-lane-predicate.md. `0` = the A/B control. Engagement: fuse3_zc_write_fusions/_bytes; fuse3_zc_write_lazy_extractions ~ 0 is the hold gate's staleness law."),
    k("SQUEEZEFS_FUSE_ZC_READ_FUSION", Kind::Bool, "off", "Handler/worker FUSION for armed FUSE READs (e2e audit R-3, read board #3's zc-leg form): arm (b) of the composed READ dispatch law — a READ the reap thread's inline probe demoted runs its handler on the reaping queue worker's fused lane when the session is zc-armed and the size is at or under the fusion ceiling, so the zc direct leg's fetch message, CQE resume and prefilled commit are same-thread (the 4k-random attribution measured those three hops at 35-47 us each around a 40 us DMA). Default OFF on measurement: alone vs the pre-R-2 dispatch it won the field kern rand-4k row (+15.5 %); composed behind R-2's fast dispatch it read -6.5 % IOPS vs R-2 alone (481 k vs 514 k, p50 +70 us, p99 -66 %) — the fused pass's run-queue waits replace the lane hop + two bridge hops at a net loss, so demoted READs ship on arm (c), the lane homed on the queue's CPU. `1` = fuse (the tail-shaped A/B posture). Partition: transport_fast_dispatch_serves + fuse3_zc_read_fusions + transport_fast_dispatch_demotes ≡ READs. Engagement: fuse3_zc_read_fusions ≈ every cold kernel READ on an armed session when on, 0 when off."),
    k("SQUEEZEFS_READ_ZC_SERVE", Kind::Bool, "on", "The fd-SOURCE zc READ serve (e2e perf audit R-4, read board #4/#5, .benchmarks/2026-09-05-r4-read-zc-serve.md). On a zc-armed session every out-paged reply body reaches the request's pages by READ_FIXED(fd -> slot) — an fd-SOURCED primitive (device fd on the direct leg, the per-ent bounce memfd otherwise), so a warm or cold-slice serve paid the daemon's tier -> bounce copy AND the kernel's bounce -> folios pass (2.0 passes/byte warm, 2.69 cold whole-block). `1` builds the whole-block READ fill pool as ONE memfd slab (MAP_SHARED — the ZcBounce two-way-reachability law): fills, hot-tier entries and read-lane holds become fd-addressable and the router/fast-probe hand the transport (fd, offset) of the tier buffer ITSELF; the queue worker bridges it into the folios and the daemon copy is DELETED (1.0 warm, 1.69 cold-slice). No kernel change — the bridge op is the one the bounce already rides. The write path's ActiveBlockBuf pool is untouched (a SEPARATE pool), so the A/B moves the READ serve only. Non-pool sources (NVMe read cache mmap, transform decode output, over-capacity fresh backings) keep the copy path, counted. Default OFF until the field r_cold/r_warm A-B-B-A adjudicates. Engagement: read_zc_pool_serve_bytes (closure term beside dest/bounce/dest_dma/zc_serve) + read_zc_pool_serve_warm_bytes (the hot/hold subset); fuse3_zc_fd_body_fallbacks must stay ~0."),
    k("SQUEEZEFS_FUSE_ZC_FUSION_MAX", int(4096, 1 << 30), "derived (payload/8)", "Fused-dispatch payload ceiling, bytes — bounds the handler work the queue worker's drain loop runs inline (the write-bracket law: never move payload-scale memcpys onto the worker). Default derives from the negotiated transport payload size: payload/8 = 128 KiB at the shipped 1 MiB geometry, bracketing the measured hop-vs-inline-copy crossover (~2 cross-thread wakes + 2 schedules vs a DRAM-bandwidth merge). Explicit value wins verbatim."),
    k("SQUEEZEFS_FUSE_PLACED_MERGE", Kind::Retired { successor: "(deleted — FUSE placed-merge was falsified; IL placed_sever is not this knob)" }, "-", "Retired (class-1 delete, one-path P1): FUSE placed-merge was falsified at ~25% cohort capture (.benchmarks/2026-08-09-fuse-placed-merge.md). IL placed_sever is a different path and is not this knob."),
    k("SQUEEZEFS_FUSE_ZC_RETENTION", Kind::Bool, "on", "zc payload-retention ARMING (kernel 0029, design-zc-write-kernel-v2 §6.1): where the RELEASE_PAYLOAD opcode probes Present and the session runs zc, REGISTER carries FUSE_URING_PAYLOAD_RETENTION. Default ON because arming alone is bit-identical (§3.6 — zero RETAIN commits ride the wire until the ACK-early daemon posture engages, a separate lever); pre-0029/stock kernels decline loud-informational and keep ACK-after-CQE. `0` = the A/B escape. Arm proof: fuse3_zc_retention_negotiated + the mount-log buffers= verdict."),
    k("SQUEEZEFS_ZC_ACK_EARLY", Kind::Bool, "on", "ACK-early for device-overlay stores: eligible overlay stores reply while the device DMA runs. Page-cache-sound writes retain the zc slot (kernel 0029). O_DIRECT overlay extracts at delivery (batched) and ACKs on the owned Bytes. Overlay is default ON, so this lever is live on a shipped mount. Coverage/publication/fsync/read-wait stay CQE-anchored; acked custody retries forever (overlay_ack_early_retries). `0` = ACK-after-CQE A/B. Engagement: overlay_ack_early_stores/_bytes."),
    k("SQUEEZEFS_OVERLAY_DEPTH_GOVERNOR", Kind::Bool, "on", "W-3 (e2e perf audit write board #3): device-overlay stores admit their in-flight segment bytes through the write-pipeline depth governor (write_pipeline_inflight_bytes / depth_target / admission_waits govern BOTH write vehicles; overlay CQEs feed the lane's BDP + probe-up). ACK-early semantics unchanged — only ADMISSION waits when the pipe is over target. Default ON. `0` = the pre-W-3 open-loop overlay store (the A/B control; the overlay notes' 0.68-0.80x device-bound rows). Engagement: overlay_governed_stores ≡ overlay stores admitted (0 with the lever off)."),
    k("SQUEEZEFS_GAP_SEED_RANGED", Kind::Bool, "on", "W-6 (e2e perf audit write board #10): the device overlay's settle sources a PARTIAL overwrite record's uncovered ranges from the captured old binding by RANGED device reads — one read per gap, exactly the gap's bytes (gaps are 4 KiB-page-aligned) — instead of fetching the WHOLE old block image (a 4 MiB block with a 64 KiB hole read 4 MiB to seed 64 KiB). Passthrough + undecorated old bindings only, and only while Σ gaps stays below the block window (the whole read is cheaper past it); decorated `bk:off:len` bindings and transformed volumes keep the whole-image funnel by construction. Default ON. `0` = the whole-image read (the A/B control, byte-identical seeds). Engagement: overlay_gap_seed_ranged_bytes (⊆ overlay_gap_seed_old_bytes) and overlay_gap_seed_read_bytes (device bytes READ per seed — the amplification numerator, = the block window with the lever off)."),
    k("SQUEEZEFS_FSYNC_TOUCHED_NAMESPACES", Kind::Bool, "on", "W-5 (e2e perf audit write board #9 / §5.3 row 15, `.benchmarks/2026-09-08-w5-fsync-economy.md`): an fsync barriers only the DATA namespaces this ino's bytes landed on since its last covering barrier — the per-ino touched set is a stripe word (`fsync_economy::TouchedTable`: per-device ordinal bits + a stamp generation, the D-3 stripe width) stamped at every layout publish (`DataRouter::block_ref_ops`, the same closure the C8 durable-ref oracle proves complete) and at the two in-place DMA shapes (W1 patch, in-place full-block overwrite), observed before the barriers start and cleared only after every leg succeeded. Write-through namespaces (`data_volume_write_cache`) skip the barrier outright — acknowledged writes are power-safe on completion. A shared stripe costs one extra coalesced barrier, never a missed one (a device barrier covers every write completed before it started, whoever issued it). `0` = the shipped shape: every namespace, every fsync, write-through included (the A/B control). Engagement: fsync_data_namespaces_flushed vs fsync_data_namespaces_touched vs fsync_calls × namespace count; fsync_noop_clean (data legs skipped entirely); fsync_write_through_skips; fsync_touched_unresolved should stay 0."),
    k("SQUEEZEFS_FSYNC_PARALLEL_LEGS", Kind::Bool, "on", "W-5 (e2e perf audit row 15): the fsync ladder's INDEPENDENT waits run concurrently and are all awaited — the staged-payload sync and the per-namespace device Fsyncs as one joined step (`fsync_phase_ns.data_barrier` ≈ max(legs), not Σ), and on an S11 co-writer the FlushExtents force overlaps the local ladder (`extent_barrier` reads the residual). Every leg runs to completion (a dropped barrier future would fail the coalescer's queued waiters); the first error wins. The ORDER that carries semantics is untouched: the S10 intent barrier stays first, the whole data-barrier step completes strictly before the meta legs that name the blocks (DUR-1), the meta barrier stays last. `0` = the shipped serialized legs (the A/B control). Engagement: fsync_parallel_joins (barrier steps that joined ≥ 2 legs)."),
    k("SQUEEZEFS_FSYNC_PROMOTE_STAGED", Kind::Bool, "off", "The fsync(2) of a STAGED-layout file (4 KiB < size <= block) also promotes it to the shared backend (one striped block write + one meta commit — `DataRouter::promote_staged_file`, the merge worker's and the dismount pass's own primitive), so 'fsync'd' means 'visible to every client of the set' instead of 'durable in this host's staging root until pressure or unmount promotes it'. The promotion is its own fsync_phase_ns leg (`staged_promote`), after the local flush and before the data barrier that covers the promoted block (its device is stamped into the touched-namespace table) and the meta barrier that names it; a failed promotion never fails the fsync (the local durability holds — the pre-lever contract), it is counted and the entry stays resident. Default off pending the counted A/B on squeeze-test (.benchmarks/2026-09-09-dismount-staged-residue.md §7 item 3); `1` is the pricing arm. Read once per process. Engagement: fsync_promoted_{files,bytes}, fsync_promote_failures, fsync_promote_noops (a racing promoter — another fsync, the merge worker, the dismount pass — got there first)."),
    k("SQUEEZEFS_ZC_ACK_EARLY_ODIRECT", Kind::Bool, "off", "Lets a HELD O_DIRECT/GUP overlay slot ACK early by snapshotting (extract) before the reply — bytes sampled at ACK, so a post-ACK buffer reuse cannot change what lands (the 2026-08-09 live-smoke aliasing). Production O_DIRECT overlay does not HOLD: it extracts at delivery and rides Bytes ACK-early without this knob. `1` is the labeled held-GUP snapshot posture (R4 / in-process slot seam). Engagement: overlay_ack_early_stores + fuse3_zc_write_extract_bytes (retain_commits stay on the page-cache arm)."),
    k("SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS", int(100, 600_000), "30000", "zc bridge-op deadline, ms (the bounded-outcome law): past it the worker pushes AsyncCancel and the op's CQE resolves through the loud fallback ladders (fuse3_zc_bridge_cancels)."),
    k("SQUEEZEFS_TEST_ZC_DROP_WRITE_CQES", int(0, 1_000_000), "0", "Test seam: the zc worker consumes-and-drops the first N WRITE-class bridge CQEs (pend + deadline stay live) — the deterministic lost-CQE interleave of the zcws-9 W4 wedge. Never set in production."),
    k("SQUEEZEFS_TEST_DROP_COMMIT_WAKES", int(0, 1_000_000), "0", "Test seam: the reply path skips the first N eventfd wake writes after arming the coalescer — the commit message stays queued and the worker's PollAdd never fires — the deterministic lost-commit-wake interleave of the 2026-09-08 generic/795 wedge. Never set in production."),
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
    k("SQUEEZEFS_IPC_DD_EAGER_FLUSH", int(0, 1 << 12), "governed (CadenceCore)", "Direct-drive issue cadence (fourth adjudication, write-wall Addendum 8): absent = the CadenceCore-GOVERNED threshold — probed downward by halving from the measured sweep claim size, adopted only on live delivery response, retreating/decaying to sweep-only otherwise (K=16 was a counted +4-8% at t32qd32 but every closed-form derivation was falsified — Addendum 7); explicit K > 0 wins verbatim (measurement lever); 0 = sweep-only with the governor ignored (the ungoverned A/B control). Gauges: ipc_dd_cadence_{k,probe_ups,probe_backoffs}."),
    k("SQUEEZEFS_IPC_DD_INLINE_REAP", Kind::Bool, "on", "Reaper/drain fusion: the owning service thread drains its lane's direct-drive CQ inline (zero syscall); 0 = the A/B lever (reaper-only completion, the pre-fusion posture). Auto-disarmed on kernels without IORING_ENTER_EXT_ARG."),
    k("SQUEEZEFS_IPC_SOCKET_DIR", Kind::Str, "derived (XDG_RUNTIME_DIR / /run)", "Rendezvous socket directory; `none` disables the filesystem-path socket."),
    k("SQUEEZEFS_IPC_SPIN_US", int(0, 100_000), "0 (absent; =1 on SQUEEZEFS_IPC_SPIN_ADAPTIVE routes absent to the governor)", "Service-thread empty-pass spin window, us. Explicit wins verbatim (incl. 0). Absent + SQUEEZEFS_IPC_SPIN_ADAPTIVE=1 = the spin governor (client-topology 2026-08-14): the measured 100us plateau window, engaged only on multi-session lanes (the qd1 2.5x-loss falsification's structural guard) in the park-churn regime (park EWMA <= 200us) under the derived busy ceiling 100-lanes/cores%."),
    k("SQUEEZEFS_IPC_SPIN_ADAPTIVE", Kind::Bool, "on", "The spin governor arm — DEFAULT ON per the efficiency doctrine (user ruling 2026-08-14: same work for less effort is a win — the latch precedent): counted IOPS-wash with parks -40% / 550-700k absorbed spins per 25s fleet row and CLEAN qd1/1x32 gates (the three falsification guards: multi-session lanes only, park-churn EWMA regime, derived busy ceiling). =0 is the A/B control (the pre-governor posture); instruments ipc_spin_{window_us,absorbed_parks,disengaged_busy}."),
    // -- L4 interception: client (shim) side -----------------------------
    k("SQUEEZEFS_IL_SESSIONS", int(1, 1 << 12), "derived clamp(cpus/4,2,16)", "Per-mount fd-shard session count (override lever; the default ties to the daemon's ceiling)."),
    k("SQUEEZEFS_IL_SPINS", int(0, 1 << 24), "adaptive", "Fixed completion spin count (pins the adaptive spin for measurement)."),
    k("SQUEEZEFS_IL_MAX_RUN_SLOTS", int(0, 1 << 16), "0 (unbounded)", "Cap on concurrently claimed slots per session."),
    k("SQUEEZEFS_IL_OP_TIMEOUT_MS", int(1, MS_MAX), "bounded default", "Per-op bounded wait against a stalled daemon, ms."),
    k("SQUEEZEFS_IPC_DD_LANE_FLUSH", Kind::Bool, "on", "Direct-drive flush scope: on = a service thread enters only its OWN lane's ring (the r3 drain-funnel fix — flush-all serialized every svc thread on every shard's kernel uring_lock); 0 = the pre-r3 flush-all sweep (A/B lever)."),
    k("SQUEEZEFS_IPC_DD_COOP_TASKRUN", Kind::Bool, "on", "Direct-drive ring COOP_TASKRUN (r3): on = completion task-work runs only at a ring entry by the submitting task — a CQE submitted by a svc thread posts at that thread's NEXT sweep, coupling CQE-post latency to the pass cadence; 0 = signal-delivered task-work (CQEs post promptly at device completion; the pre-r3 posture — A/B lever for the post-ACK-fast ceiling attribution)."),
    k("SQUEEZEFS_IL_REAP_PARK_MAX", int(0, 1 << 16), "24", "libaio reap: queue depth at or below which the event-driven park is used (24 since the 2026-08-13 fleet-residue recount — the batch-wake threshold retired the wake herd that priced 24 out in 2026-07-28)."),
    k("SQUEEZEFS_IL_SPARSE_BATCH_MARKS", Kind::Bool, "on", "Sparse-arm batch park marks (design-il-wake-economy OQ2): the event park's per-session mark derives from its pending population (reap_batch_wake_threshold, age-bounded at the reap quantum; single-pending sessions stay event-exact by derivation). DEFAULT ON — field adjudication 2026-08-14: consistently higher IOPS on the saturated-reaper field fleet (the design's named target venue); the local unsaturated-venue cost (-3..-3.5%, TCP devsub) stays recorded and =0 is the A/B control for latency-sensitive mounts."),
    k("SQUEEZEFS_IPC_CQE_WAKE_LATCH", Kind::Bool, "on", "Completion-doorbell wake-collapse latch (design-il-wake-economy L1): on = a park era pays at most ONE cqe FUTEX_WAKE (mark-passed completions past the first collapse — ipc_cqe_wake_collapsed counts them); 0 = the pre-campaign wake-per-mark-passed posture (A/B control, never an operational escape)."),
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
    k("SQUEEZEFS_OP_TRACE", Kind::Bool, "off", "e2e audit A2: arm the per-op TRACE RING at mount — one CLOCK_MONOTONIC stamp per phase boundary per sampled op (op id = the FUSE request unique / the il slot ticket), joined on one timeline; `cat <mnt>/.trace` DRAINS it, `tests/op_trace_stitch.py` stitches it against the histograms and the kernel fuse tracepoints. Geometry and the sampling divisor are DERIVED (rings from the thread population, pool from the R5 budget, divisor so one drain interval of the op ceiling fits); the `op-trace on|off|status` admin verb toggles it live. Off = one relaxed load per hook."),
    k("SQUEEZEFS_FREE_FORENSICS", Kind::Bool, "off", "Capture a backtrace per block free to attribute double frees (expensive)."),
    k("SQUEEZEFS_STATS_KEY_CENSUS", Kind::Bool, "off", "VAL-7a: arm the `.stats` key census (live object keys + per-inode write custody — a debugging surface). The `*_count` replacements stay unconditional."),
    // -- Cluster wire (DLM S3 — the ONE cluster transport) ---------------
    k("SQZ_CLW_RTT_ENDPOINT", Kind::Harness, "loopback", "RTT instrument: dial a REAL coordinator instead of a local loopback listener (tests/cluster_wire_tests.rs `rtt_row`)."),
    k("SQZ_CLW_RTT_SECRET_HEX", Kind::Harness, "none", "RTT instrument: the target volume set's job:enroll secret, hex (required for a remote row)."),
    k("SQZ_CLW_RTT_SAMPLES", Kind::Harness, "2000", "RTT instrument: counted samples after the discarded warm-up."),
    k("SQZ_CLW_RTT_PAYLOAD", Kind::Harness, "0", "RTT instrument: payload bytes per direction."),
    k("SQZ_STORM_THREADS", Kind::Harness, "32", "D-3 in-process metadata storm (tests/dlm_stripe_storm_tests.rs): concurrent committers — the in-flight population the stripe census is read against."),
    k("SQZ_STORM_PER_THREAD", Kind::Harness, "512 release / 64 debug", "D-3 in-process metadata storm: names per committer per phase."),
    k("SQUEEZEFS_CLUSTER_WIRE_SVC_THREADS", int(1, 1 << 12), "derived clamp(cpus/8, 1, 8)", "Owner-side RPC lanes (pinned service threads — §6.7's venue rule: never the conveyor's task). Absolute override; still clamped to the core count so it cannot oversubscribe the box."),
    k("SQUEEZEFS_MULTI_WRITER", Kind::Bool, "off", "DLM S7/S9: arm the MULTI-WRITER planes — a device-enforced WERO (rtype 3) hold on every data namespace, remote write custody (grant/renew/revoke/expire) served to peers, metadata ownership, and the daemon's publish path on the wire. Refuses the mount loud, naming the missing piece: a substrate without NVMe reservation support (every loop device, including tests/dev_substrate.sh's default — §6.9's S9 guarantee is 'refused on non-PR'), a format missing one of the six capability bits 7/9/10/11/13/14 (the default format stamps them since the rung-10b Phase-B flip; `--single-writer` formats and pre-flip sets refuse until `volume enable-multi-writer`), a membership plane that is off (an unseeable co-writer is an unevictable one), no durable writer era, or SQUEEZEFS_MW_BIND=off. Off = the shipped single-writer posture: the D0 guard arbitrates and the data plane is fenced locally by the custody epoch."),
    k("SQUEEZEFS_MW_ROLE", Kind::Enum(&["authority", "co-writer", "set-authority", "partial-authority"]), "authority", "DLM S9 + per-volume claim admission: which posture this mount is. `authority` (the default, and today's posture verbatim) holds the D0 claim on EVERY volume, serves custody + the publish path, and is the only process that writes the claim-set roster. `co-writer` demands the CO-WRITER posture — metadata read-only locally with mutations shipped to the authority, data read-write under a granted custody lease — and REFUSES the mount unless all five admission rungs hold (see `docs/operations.md` §Multi-writer co-writer mounts). The two PER-VOLUME postures (docs/design-per-volume-claim-admission.md §5.1.3) claim a SUBSET of the set: `set-authority` appends to the volume hosting slot 0 — which under ruling D20 is what MAKES it the set authority (lane assignment, the S9 custody endpoint, the one freed-offset grace ring, maintenance coordination, ino 1) and is why it needs no SQUEEZEFS_MW_AUTHORITY — and `partial-authority` appends to its own volumes and ships the rest. The role value is a DECLARATION and the seven-rung ladder (src/partial_authority.rs) decides it: a mount that declares `set-authority` without being assigned the slot-0 volume is refused naming the posture it should have declared. Read only when SQUEEZEFS_MULTI_WRITER is on; any role without the opt-in refuses rather than silently mounting as an authority, because the D0 refusal must never be bypassed by inference. Both per-volume roles are reachable end to end since PR 7b: the ladder decides, the partial-writer open serves the volumes this node owns and ships the rest, and each posture's arm composes its halves (a set authority's is `arm_multi_writer`; a partial authority's is `arm_partial_authority` — the co-writer client halves toward the set authority plus an owner half over its own volumes). What still refuses is a set no operator has ASSIGNED: rung 2/3 name the offline `squeezefs volume set-owners` verb, so a plain multi-writer mount is unaffected and every unassigned set in the field keeps its exact behaviour."),
    k("SQUEEZEFS_MW_AUTHORITY", Kind::Str, "none", "DLM S9 + per-volume claim admission: the `addr:port` of the SET authority (ruling D20 — the owner of the volume hosting slot 0), dialled for write custody, the lane grant and the shipped publish path (that node's SQUEEZEFS_MW_BIND endpoint). Required for `SQUEEZEFS_MW_ROLE=co-writer` and for `partial-authority` — either one with no custody source is inert, so an absent value refuses the mount. NOT READ for `set-authority`, which IS the endpoint: a fleet that exports one value everywhere and varies only the role still mounts (the ladder announces the ignored value rather than refusing it). Named residual: the endpoint is not published in any durable record yet, so an operator declares it here exactly as the membership and job-wire binds are declared; a peer authority's endpoint (a partial authority that is not the set authority) resolves from the live membership census instead."),
    k("SQUEEZEFS_MW_MEMBERS", Kind::Str, "none", "DLM S9: the AUTHORITY's operator-declared co-writer roster — a comma list of client ids (KD-MW-2, design-full-multi-writer §5.1: the pair `node_{16 hex}.m{8 hex}` = node token + mount slot, printed by a refused co-writer's own mount log; the bare `node_{16 hex}` form is still accepted as a SLOT WILDCARD naming every mount slot of that node — the single-mount-host convenience). The authority commits one durable claim-set member entry per rostered id (§6.2 item 7), which is how a co-writer becomes enrolled without being able to commit: enrollment is an act of the authority, never a claim the joining node makes about itself. Read only when SQUEEZEFS_MULTI_WRITER is on and the volume set carries incompat bit 14; an unrostered client is refused at admission naming its own id. Scope note (KD-PV-4): this stays the roster for the PURE CO-WRITER topology — under per-volume claim admission the offline `squeezefs volume set-owners` verb writes ownership AND pid-less writer enrollment on every volume in one bracket, so the knob is not read on the set-authority / partial-authority arming path."),
    k("SQUEEZEFS_MW_BIND", Kind::Str, "auto", "DLM S9: where this mount serves the write-custody and publish authority — `auto` (0.0.0.0:0, ruling D2's posture; the default), an explicit `addr:port`, or `off`. `off` REFUSES a multi-writer arm rather than arming an inert one that grants no custody and serves no peer's publish path. Read only when SQUEEZEFS_MULTI_WRITER is on; a malformed address refuses rather than binding somewhere the operator did not ask for."),
    // -- Metadata function shipping (DLM S8) -----------------------------
    k("SQUEEZEFS_META_SHIP_BATCH_MAX", int(1, 4096), "derived clamp(cpus x 2, 64, 4096)", "Verbs per shipped metadata frame — the S8 pipelining unit. Derived like the M7 commit-conveyor batch cap, because a frame's ops become that many transactions on the owner's conveyor; a frame also stays inside the cluster wire's CONTROL class cap."),
    k("SQUEEZEFS_META_SHIP_DEDUP_MAX", int(1, 1 << 24), "derived max(batch_max x 128, 8192)", "Owner-side idempotency window entries — how far back a client's retry may reach and still be answered from its original outcome (S8 requirement 6). Retiring an entry only weakens exactly-once for a retry arriving after that many newer ops from the SAME client."),
    k("SQUEEZEFS_META_SHIP_INLINE_SERVE", Kind::Bool, "off", "D-5 (e2e perf audit DLM board #7): the OWNER-side dispatch venue on both shipped planes (the S8 verb frame; the S9 publish call / group / free / harvest). `off` (the default): the `spawn_meta_join` hop — a served dispatch is spawned onto the shared sqz-meta lanes and joined from the accepting connection's thread. `1`: the dispatch is polled on the accepting connection's own thread (`sqz-clw-conn`, dedicated and parked for exactly that reply) — the two cross-thread hops are 0 by construction and every wake inside the work unparks that thread directly. SHIPS OFF on measurement (the 2026-09-08 squeeze-test fleet row, two same-binary A-B-B-A brackets both orders): the hops vanish but the served work runs 0.6–0.9 ms SLOWER on the connection thread (`run` 536–770 → 1,211–1,473 µs — every in-work wake became an OS unpark, paid by the durability lane's fan-out), co-writer publish latency +15–49 %, ingest −1.5…−7 %; the in-process win needed an artificial lane hog (the field's lanes run at ρ ≈ 0.2). `1` is the same-binary A/B lever for a venue whose lanes ARE saturated — never an operational escape. Read once per served frame (one getenv per wire round trip). Engagement: meta_ship.owner_dispatch_hops ≡ every dispatch on the default (owner_dispatch_inline carries them under `1`); attribution: meta_ship_owner_dispatch_ns.{queue_hop,run,wake_hop,total}."),
    k("SQUEEZEFS_PUBLISH_CONVEYOR_GROUP", Kind::Bool, "on", "DLM S9 publish plane, OWNER side (E2E perf audit D-1c — one conveyor group per shipped frame): a served frame's independent SetLayoutAndSize calls are prepared concurrently and committed as ONE conveyor group per dispatch round (one queue-lock enqueue of every staged tx, so a frame is one apply pass by construction instead of arrival-spread ÷ pass-latency — 3–14 passes per 24 publishes measured before it). One tx = one checksummed journal entry is unchanged. Default ON. `0` = the D-1b per-chain dispatch (every call commits its own tx) — the same-binary A/B control, never an operational escape. Read per served frame (one getenv per wire round trip). Engagement: meta_conveyor_group_{commits,txs} and meta_ship_publish.frame_groups; all 0 under `0` and on a solo mount."),
    k("SQUEEZEFS_PUBLISH_SHIP_DEPTH", int(1, 64), "derived clamp(ceil(cpus/8), 2, 8)", "DLM S9 publish plane (E2E perf audit D-1b): publish FRAMES one co-writer keeps in flight per authority — one authenticated session each (the D-1b session pool; SQUEEZEFS_PUBLISH_SHIP_MULTIPLEX=1 rides them all on ONE pipelined session — D-5, off on measurement). Derived on the owner RPC-lane slope (one per 8 cores, floor 2 = the minimum at which a frame's RTT overlaps a sibling's owner pass, ceiling 8 = the RPC-lane ceiling). `1` is the stop-and-wait A/B control (frames still batch concurrent publishes; nothing pipelines), never an operational escape. A frame is whatever is queued when a slot frees — no timer, no added delay on a serial stream."),
    k("SQUEEZEFS_PUBLISH_SHIP_MULTIPLEX", Kind::Bool, "off", "D-5 (e2e perf audit DLM #8's single-connection half): the co-writer's in-flight publish frames toward one authority ride ONE pipelined cluster-wire session (calls carry their ids, a per-session reader thread `sqz-clw-mux` demultiplexes the replies, the authority's session lane serves the in-flight frames concurrently) instead of one request/reply session per frame — so SQUEEZEFS_PUBLISH_SHIP_DEPTH no longer costs that many connections against the authority's max_connections cap (F-B: one owner thread per connection). SHIPS OFF ON MEASUREMENT: the connection thread is the execution venue, so one session serves its K frames on one owner thread where the pool serves them on K — in-process at equal depth the pool ran ≈ 1.5× the multiplexed publish rate (`.benchmarks/2026-09-08-d5-owner-hop-and-depth.md`). `1` = the capability arm for a fleet whose authority connection cap binds before its CPU; `0`/unset = the D-1b session pool. Read once per publish lane at its first frame. Engagement: meta_ship_publish.ship_mux_frames ≡ ship_frames when engaged (0 on the pool); ship_session_dials reads 1 per endpoint instead of depth."),
    k("SQUEEZEFS_DLM_STRIPES", int(1, 1 << 24), "derived next_pow2(max(shipped, 16 × possible_cpus × q_depth))", "D-3 (e2e perf audit DLM board #4): the stripe population of every DLM-class lock table — the 4a DlmLockManager inode + dentry classes (shipped floor 4096), the lease waiter / grant-floor pair and the S9 owner serve stripe (shipped floor 1024). Explicit wins verbatim (rounded UP to a power of two — the index is a mask); derived = 16 × the FUSE-over-io_uring transport's deliverable concurrency (one queue per kernel possible CPU × SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH, default the desired 32), so a false-sharing wait — which on the 4a tables costs a whole commit, the D5 hold — hits at most one acquire in 16 at full ring occupancy; floored at the shipped width (never-regress). MEASUREMENT lever: `4096` is the shipped-4a A/B control, `1` the everything-serializes crucible. Read once per process at the first table's construction. Engagement: the `<table>_stripes` words on the stats inode; conviction: `<table>_stripe_collisions` vs `<table>_key_waits`."),
    k("SQUEEZEFS_DLM_TOKEN_CACHE_MAX", int(1, 1 << 24), "derived max(R5 budget/8192/32 B, 4096)", "Client fencing-token cache entries (S8's resolution of the S4 fencing-read contract; spec §6.5 item 1 requires >= 99.5 % of lock operations served locally). Every eviction costs one loud miss and one shipped getattr refresh, never a wrong answer."),
    // -- S10 recall lane (rung 11 — brake before engine) ------------------
    k("SQUEEZEFS_DLM_RECALL_BATCH_MAX", int(1, 32_768), "derived CONTROL frame cap/2/32 B = 16384", "S10 recall lane (design-full-multi-writer §8, spec R5/R6): recall entries per batched per-client frame — MEASUREMENT lever; the default derives from the cluster wire's CONTROL-class cap so one frame carries a client's whole token set (R6's batch-reclaim law). Ceiling = the cap with zero headroom (32768); past it a frame cannot encode."),
    k("SQUEEZEFS_DLM_RECALL_DEADLINE_MS", int(1, MS_MAX), "derived 4x live p99 (rtt+owner), clamp [1 ms, lease TTL]", "S10 recall lane: per-frame ack deadline, ms — MEASUREMENT lever; the default derives from the LIVE meta_ship_phase_ns.rtt + meta_ship_owner_phase_ns.total p99 (spec R3's law: never a constant), floored at the 1 ms timer grain and ceilinged at the membership lease TTL, past which the S6/S7 fence arithmetic bounds the client anyway. A timed-out recall's grant is DEAD, loudly."),
    k("SQUEEZEFS_DLM_RECALL_COOLDOWN_MS", int(1, MS_MAX), "derived 8x the thrash window (= the deadline)", "S10 thrash valve (spec R5): how long a demoted (owner-served) hot object refuses new delegation grants — MEASUREMENT lever; the default bounds the worst-case residual thrash duty cycle at cycles/(cycles+8) ~= 27 % of the un-valved volume. The valve ITSELF has no knob: the brake is structural, never operationally removable."),
    k("SQUEEZEFS_DELEGATION", Kind::Bool, "on", "DLM S10 (rung 12): LOOKUP-class subtree delegations — the A/B lever (design-full-multi-writer §11; the instrument stays alive both sides). On (the default), an armed authority piggybacks delegation grants on the metadata replies it was already sending and enforces recall-before-conflicting-publish through the rung-11 recall lane; an armed co-writer serves delegated lookups/getattrs/readdir from its reader-revalidation view with zero round trips. Read ONLY when the mw ownership plane is armed (the SQUEEZEFS_MW_ROLE precedent): `=1` on an unarmed mount is announced-inert (every dlm_delegation gauge structurally 0), never a refusal; `=0` on an armed mount is the A/B control the S10 rows compare against."),
    k("SQUEEZEFS_RANGE_CUSTODY", Kind::Bool, "off", "DLM S11 (rung 15, KD-MW-7): distributed byte-range custody on the S9 custody lease. `=1` on an armed co-writer routes sub-file write custody through required/desired range grants (block-aligned desired, admit-time coalescing, the §9.2 geometry cap + dlm_grant_table_bytes R5 ceiling) instead of whole-file leases, cached in the S8 token cache's range extension. Read ONLY when the mw ownership plane is armed (the SQUEEZEFS_DELEGATION precedent); set-but-unarmed is announced-inert (every range_custody gauge structurally 0), never a refusal. STATIC DEFAULT OFF (every rung ships dark — the rollout rule): the concurrent same-ino publish composition LANDED — rungs 16/17's machinery plus the zeros-interleave fix arming the custody-scoped Put in production — and the s11-range composition gate is GREEN ×3 from zero (.benchmarks/2026-08-17-s11-zeros-interleave-fix.md); the default-ON revisit is rung 18's decision behind that gate staying green. The s11-range leg arms it explicitly and is the composition's acceptance surface."),
    k("SQUEEZEFS_UPDATE_INTENTS", Kind::Bool, "on", "DLM S10 (rung 13, KD-MW-13): per-directory EXCLUSIVE UPDATE grants + asynchronous create-intent batches — the A/B lever. On (the default), an armed authority piggybacks UPDATE grants (dentry census + ino supply) on shipped-create replies, and an armed co-writer MINTS children of a granted directory locally (zero round trips — the tar-x serial-create recovery), batching them for apply at fsync(dir)/recall/barrier. Rides the delegation plane: read ONLY when the mw ownership plane is armed AND SQUEEZEFS_DELEGATION is on; `=1` on an unarmed mount is announced-inert; `=0` on an armed mount is the tar-x row's control (creates ship exactly as rung 9's)."),
    k("SQUEEZEFS_SLOT_PLACEMENT", Kind::Bool, "on", "DLM S10 (rung 14, KD-MW-6): client-owned-slot placement — the A/B lever. On (the default), an armed authority mints each shipping client's fresh inos into a slot DEDICATED to that client (outside the volume's mint rotor, stable per client — the migratable unit), and the auto-policy migrates a SUSTAINED client's hot slots toward a metadata volume that client OWNS (the fleet-of-authorities shape, spec §6.10 R4) through the existing online migrate-meta-slot engine — valve-bounded (the rung-11 arithmetic; the valve itself has NO knob). Read ONLY when the mw ownership plane is armed: `=1` on an unarmed mount is announced-inert (every meta_ship_placement gauge structurally 0); `=0` on an armed mount is the A/B control (mints ride the rotor exactly as rung 13 shipped them). On today's one-authority fleets the MIGRATION half is structurally dark — no shipping client owns a metadata volume — while the mint-targeting half engages. Under per-volume claim admission (KD-PV-13) the candidate becomes reachable and the MIGRATION half is DISARMED while a multi-owner plane is armed — every migration the policy can select is cross-owner by construction and refused, so migrations_triggered/migrations_failed stay 0 by construction while migration_candidates counts the follow-on's demand signal."),
    // -- Membership plane (DLM S6 — liveness off the journal) ------------
    k("SQUEEZEFS_FLEET_JOBS", Kind::Bool, "on", "KD-MW-16 (rung 10c, docs/design-mw-fleet-jobs.md): fleet-parallel maintenance participation. On a READER/CO-WRITER mount that has joined the S6 membership plane, `on` (the default) spawns the fleet job worker at mount — it proves storage trust with the job:enroll HMAC (the access it already holds IS the credential) and serves fleet READ shards (fsck census/scrub residues) to the coordinator over the §5.1.6 wire. On the COORDINATOR, `on` lets the fsck detect pass fan out across enrolled read workers (zero enrolled workers = the local run verbatim). `0` disarms both halves on the mount that sets it; it never affects peers."),
    k("SQUEEZEFS_MEMBERSHIP_BIND", Kind::Str, "off", "DLM S6: where this mount serves the membership plane — `off` (default), `auto` (0.0.0.0:0, ruling D2's posture), or an explicit `addr:port`. Armed, a WRITE mount becomes the lease authority (its census is what `squeezefs clients` reads, and readers become visible for the first time) and a READ-ONLY mount joins as a member; the durable footprint is ONE rendezvous record written at arm. Default off because flipping it changes what an operator sees, and the measured validation that would justify a new default is deferred (ruling D11)."),
    k("SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS", int(1_000, MS_MAX), "45000 (= CLIENT_STALE_TTL_SECS)", "DLM S6: the OWNER's lease TTL, ms. Defaults to the ONE staleness law's 45 s so `live`/`stale` means the same thing on the plane and in the `client:`/`writer_claim` records. The member's own deadline is always stricter: T_self = T_owner − 2·skew_max − D_purge."),
    k("SQUEEZEFS_MEMBERSHIP_SKEW_MAX_MS", int(1, 60_000), "derived max(T_owner × 500 ppm, observed RTT)", "DLM S6 (spec §6.7): clock-skew bound between owner and member, ms. Derived from the physical monotonic-clock rate bound (500 ppm ⇒ 22.5 ms at a 45 s TTL) and the measured renewal RTT, whichever is larger. Raising it shortens T_self; a value that collapses T_self REFUSES the plane rather than clamping."),
    k("SQUEEZEFS_MEMBERSHIP_PURGE_MS", int(0, MS_MAX), "derived max(2 × checkpoint cadence, observed RTT)", "DLM S6 (spec §6.7's D_purge): how long a member may take to fail-stop — purge cached custody / stop in-flight DMA — once it decides to. Derived from two revalidation cadences (observe, then finish) floored at one RTT. It is subtracted from the member's deadline, so an honest value is a safety input, not a tuning knob."),
    k("SQUEEZEFS_MEMBERSHIP_GRACE_MS", int(0, MS_MAX), "derived (= the lease TTL)", "DLM S6 (spec §6.7 Recovery): the successor owner's failover grace window, ms — reclaim admitted, conflicting fresh acquires refused, closing early when every prior member has re-asserted. `0` opens no window, which is the forced-flush-storm posture the spec names; the default gives every previously-live member a full lease period to re-assert."),
    k("SQUEEZEFS_MEMBERSHIP_RENEW_LANE", Kind::Bool, "on", "The membership renewal's blocking VENUE (finding 15 phase B1, `.benchmarks/2026-09-07-membership-renewal-isolation.md`; the liveness law in docs/design-free-grace-sustain.md — the ack carrier must never queue behind bulk work, on either side). The renewal is the heartbeat AND the freed-offset acknowledgement's carrier; finding 2 gave its POLL the dedicated `sqz-lease` lane, but its socket round trip still hopped onto the shared `sqz-blk` pool — FIFO behind every parked RPC round trip, reclaim lane and crypto job of a co-writer under load, so a parked pool delayed the SEND by the bulk work's own duration. `on` (the default): the round trip runs on ONE dedicated OS thread (`sqz-lease-io`), spawned on the first renewal of an armed member — the `sqz-jrnl` shape, liveness I/O owning its thread. `0` = the shared pool verbatim (the A/B control). Member-side; inert on every mount without a member session. Engagement: membership_renew_lane_calls (≡ renewals on the default); attribution: membership_renew_phase_ns.carry_wait (decision → send — the venue wait shows here)."),
    // -- Freed-offset grace period (spec §6.8 item 3) ---------------------
    k("SQUEEZEFS_FREE_GRACE_MAX_MS", int(1_000, MS_MAX), "derived (2 x one acknowledgement cycle)", "Spec §6.8 item 3: how long a terminally-freed offset may wait for a reader's acknowledgement before that reader is FENCED (evicted through S6) and the offset released — 'a reader that fails to acknowledge is fenced, not waited on', because an unbounded wait converts a slow reader into the writer's ENOSPC. Derived default = 2 × `free_grace::ack_cycle` (3 × renewal interval + 3 × reader staleness bound + skew_max + D_purge), i.e. one missed cycle tolerated. A value BELOW one cycle REFUSES rather than clamping: it would fence readers answering exactly as designed. Under space pressure allocation evaluates half this bound, floored at one cycle. Inert unless a reader plane is armed."),
    k("SQUEEZEFS_COWRITER_LANE_PLACEMENT", Kind::Bool, "on", "Lane-aware write placement + allocation failover on a laned CO-WRITER (`.benchmarks/2026-09-07-cowriter-lane-aware-placement.md`; docs/design-volume-lifecycle.md §5.9, docs/design-mw-data-alloc-partition.md §4). A co-writer's lane share exhausts and refills PER VOLUME, and the §5.9 placement weight (device fill) says nothing about this mount's lane on a volume — on the s11 fleet it read ≈ 0 on both volumes and was stale for the 5 s health-worker cadence while the lane supply churned, so picks landed on the lane-exhausted volume while hundreds of blocks of the same lane sat reachable on its sibling: a wasted harvest RPC and a refused write per pick. ON (the default): the table weighs a lane-governed volume by its lane-reachable fraction of the lane share (`lane_reachable_blocks × 1000 ÷ lane share`; an exhausted lane leaves the band), the pick skips a banded volume an allocation just drained and reaches an out-of-band volume a harvest just refilled (O(1) counter reads, no rebuild), and a `StorageFull` on the picked volume tries the remaining eligible volumes in the SAME attempt before the bounded park (`BackendRouter::allocate_placed_block`). Single-writer and authority mounts are byte-identical either way (their allocators are never lane-governed). `0` = the shipped device-fill pick and no failover — the fleet A/B control. Engagement: backend_placement_lane_failovers (allocations that landed off the pick); backend_placement_lane_exhausted_picks (picks that landed dry while a sibling had lane supply — must stay ≈ 0)."),
    k("SQUEEZEFS_ALLOC_LANE_HARVEST_AHEAD", Kind::Bool, "on", "The ahead-of-stall lane refill (finding 15 part 2, docs/design-free-grace-sustain.md §5.5 / PR 4): a laned co-writer's background task (one per volume, single-flight by construction, off the allocation path) harvests its lane's freed supply from the authority when supply of the lane is WITNESSED there (the renewal grant's lane-supply hint, or the owed word the explicit-ship arm's Freed verdicts keep — never the owed word alone since 2026-09-07, `SQUEEZEFS_ALLOC_LANE_REFILL_HINT`) and its lane-reachable stock sits below the derived watermark — ceil(claim-rate EWMA x refill horizon), capped at lane-share/4, where the horizon is the authority's own measured bound age carried on the harvest reply (schema 8, OQ 2) with the member-local derivation as the fallback. `0` = ENOSPC-triggered harvests only, the shipped shape verbatim (hint included). Engagement: alloc_lane_ahead_harvests / alloc_lane_hint_refills / alloc_lane_harvest_watermark / alloc_lane_horizon_hints / alloc_lane_harvest_horizon_ms (alloc_lane_owed_blocks is the explicit-ship arm's face)."),
    k("SQUEEZEFS_ALLOC_LANE_REFILL_HINT", Kind::Bool, "on", "The co-writer lane refill's HINT GATE (finding 15, the refill gate — `.benchmarks/2026-09-07-lane-refill-hint-gate.md`): both PROACTIVE refill arms (the ahead-of-stall watermark tick and the lane-push pushed refill) arm on the authority's ADVERTISED lane supply — `free_grace_lane_supply_hint`, the authority's own count of this lane's blocks on its free lists carried on every renewal grant, summed over the mount's data volumes — with the owed ledger (`alloc_lane_owed_blocks`, +1 per Freed verdict of an EXPLICITLY shipped free) kept as a second witness. The owed ledger is a strict subset of the hint: a displaced block the authority RECOMPUTES when it serves the co-writer's layout publish (`meta_ship_publish.free_recomputed_blocks`, ≈ 90 % of a rewriting co-writer's displaced blocks on the s11 fleet) reaches its list with nothing noted owed, so gating on owed alone left both proactive arms dark while the authority advertised hundreds of the lane's blocks, and every refill ran from inside an ENOSPC park — each one a refused-write storm. `0` = the retired owed-only gate verbatim (the A/B control; the ENOSPC-path harvest is unchanged either way). Engagement: alloc_lane_hint_refills (proactive harvests that fired with the owed word at 0 — the ones the owed gate would have declined; ⊆ alloc_lane_ahead_harvests + alloc_lane_pushed_harvests)."),
    k("SQUEEZEFS_ALLOC_LANE_HARVEST_SINGLE_FLIGHT", Kind::Bool, "on", "The co-writer lane harvest's SINGLE-FLIGHT lever (finding 15 phase B1 — `.benchmarks/2026-09-07-lane-harvest-single-flight.md`): one in-flight harvest RPC per allocator (per co-writer, per data volume). Every caller of the harvest — an ENOSPC-path allocation, a parked retry (`allocate_block_grace_bounded` re-runs `allocate_block` per park slice), the placed allocation's failover, the ahead and pushed refill ticks — either LEADS (issues the one RPC) or JOINS the RPC already in flight and retries its own funnel against the refilled list (`alloc_lane_harvest_coalesced`); and a fresh EMPTY reply DECLINES re-issue (`alloc_lane_harvest_declined_stale`) until the authority's advertisement moves — a renewal grant (every grant refreshes the hint, the same value included: the renewal cadence, ≤ 500 ms under an ask, is the bound), a nonzero-hint wake, or an owed Freed verdict — never a timer of its own. A joiner's wait is bounded by the park wall, so no parked allocation waits longer than on its own RPC and the ENOSPC verdict's timing is unchanged. On the 8-co-writer fleet's file-per-proc phase every parked allocation issued its own harvest per slice: 124,240 RPCs in 9.5 min for 60,419 blocks, half empty, each a three-pass free-set scan on the authority's serve plane — behind which the members' membership renewals (the acknowledgements the grace ring waits on) queued, ageing the ring's bound to 11 s while every member's own ack was fresh: a positive feedback into more parks and more empty harvests. `0` = one RPC per caller, the shipped shape verbatim (the fleet A/B control). `alloc_lane_harvests` stays the RPC count, so harvests + coalesced + declined_stale accounts for every would-be call."),
    k("SQUEEZEFS_FREE_GRACE_DEMAND", Kind::Bool, "on", "The freed-offset grace loop's DEMAND arm (finding 15 part 2, docs/design-free-grace-sustain.md §5.2–§5.4 / PR 3): when the standing site-0 detector (ring aging past the physics floor while the LANE-REACHABLE supply sits at the trough) or a refusal edge observes recycle coupling, a TTL'd demand mark (a) asks every member holding the free list to renew at the floor cadence (rung a-prime — the fence deadline NEVER tightens off it: asked sooner, never fenced sooner), (b) widens the valve's existing rate-limited bound refresh to the coupling window (T7 collapses to the floor beat), and (c) re-bases the runway's supply input on the lane-reachable number (KD-FG-10 — the passed-global one accumulates foreign-lane releases and provably never troughs). `0` = the A/B control: the pre-campaign valve verbatim, passed-global runway input included; the site-0 OBSERVATION counter stays live either way. Engagement: free_grace_demand_pct / free_grace_demand_prods / free_grace_bound_refreshes."),
    k("SQUEEZEFS_FREE_GRACE_PASS_ELASTIC", Kind::Bool, "on", "L2b of the free-grace sustain campaign (docs/design-free-grace-sustain.md §5.2b — OQ 3, user decision 2026-08-25): a member adopting a PRODDED renewal cadence also tightens its reader revalidation pass cadence to clamp(prodded renew_ms, the 1 s checkpoint ceiling, the routine interval) — more qualify/promote passes, never shorter qualification windows, so the S5 staleness contract (an upper bound) only ever tightens and the PUBLISHED reader_staleness_bound_ms never moves. Structurally inert on venues whose routine interval already sits at the checkpoint floor (the s11 venue). `0` = the routine pass cadence always (the pre-campaign S5 behavior verbatim). Engagement: free_grace_pass_prods / free_grace_pass_interval_ms."),
    k("SQUEEZEFS_FREE_GRACE_ACK_PIPELINE", Kind::Bool, "on", "The freed-offset grace loop's PIPELINED acknowledgement ladder (finding 15 part 2, docs/design-free-grace-sustain.md §5.1 / PR 2): the reader's ReaderAckLadder holds a bounded, derived-depth FIFO of label candidates — each with its OWN learned-at snapshot and the three unchanged gates (epoch-step purge; a pass beginning >= learn + staleness + skew; the staleness + D_purge drain) — so acks advance one label per PASS instead of one per full qualify+drain cycle (the T6 quantization the campaign's rate equation convicted). `0` = the A/B control: depth-1, the pre-campaign single-candidate ladder verbatim. Reader-side, inert on every mount without a member session. Engagement: free_grace_ack_pipeline_depth / free_grace_acked_lag_ms."),
    k("SQUEEZEFS_FREE_GRACE_ACK_RENEWAL", Kind::Bool, "on", "The freed-offset grace loop's hold-time lever (b) (finding 15's remaining half, `.benchmarks/2026-09-06-free-grace-hold-time.md`; docs/design-free-grace-sustain.md §Hold-time campaign): a reader's promoted acknowledgement WAKES its lease-renewal loop instead of waiting out the rest of its beat, so the carry-home term of the hold (up to one prodded cadence — the T5 term) becomes one round trip. The renewal re-anchors the beat (at most one extra renewal per promotion, one promotion per pass); renewing early is always safe under §6.7. `0` = the ack rides the next routine beat verbatim (the pre-campaign carriage, the A/B control). Member-side. Engagement: free_grace_ack_renewals."),
    k("SQUEEZEFS_FREE_GRACE_CAUGHT_UP_RELAX", Kind::Bool, "on", "The freed-offset grace valve's CAUGHT-UP arm (finding 15 phase B1, `.benchmarks/2026-09-07-membership-renewal-isolation.md`): what the owner's renewal path hands a member whose acknowledgement already covers every held label while an ask is LIVE (a pressure reading within one TTL). The shipped arm handed it the ROUTINE cadence (\"not holding the free list, not asked\"), so at the instant the ring DRAINED every member drew the 10 s beat — and a member's LABEL source is that beat (a carriage renewal learns none) — so when the ring refilled two seconds later the re-armed ask could be delivered to nobody for a full beat: served `membership_renewals` 0/s for 7 s, `free_grace_member_ack_lag_ms.max` walking 1 s/s to 10–11.6 s, 3,200 offsets held, every co-writer lane starved (the fleet's file-per-proc phase, 2026-09-07). `on` (the default): the caught-up member is RELAXED one doubling step of the cadence in force (finding 18's decay unit), capped strictly below routine — the refill is learned within 2 × cadence, and a caught-up member still beats half as often as a laggard. The ask's lifetime is unchanged (past the reading TTL a drained ring retires it and routine returns). `0` = the snap to routine verbatim (the A/B control). Authority-side. Engagement: free_grace_prods_caught_up (disjoint from free_grace_prods, the laggards' asks); the member-side face is membership_renew_cadence_ms read against free_grace_prod_renew_ms."),
    k("SQUEEZEFS_FREE_GRACE_REFRESH_ON_ACK", Kind::Bool, "on", "The freed-offset grace loop's hold-time lever (d) (finding 15's remaining half): the owner's renewal marks the reallocation bound DIRTY when a member whose recorded acknowledgement sat at or below the published bound advances it (one compare in the plane's hot op — never the O(members) scan, KD-FG-4), and the next harvest recomputes the minimum at once rather than at the next floor beat / sweep — rate-limited to `ack_refresh_floor ÷ members` (the rate the min can change at) and never faster than twice the scan's own measured cost. `0` = the refresh stays on the floor cadence and the sweep verbatim (the A/B control). Engagement: free_grace_bound_refreshes_on_ack / free_grace_bound_scan_ms."),
    k("SQUEEZEFS_FREE_GRACE_QUALIFY_CEILING", Kind::Bool, "on", "The reader ack ladder's qualify window re-derived (ladder re-derivation item 1, user decision 2026-09-06, `.benchmarks/2026-09-06-free-grace-ladder-rederivation.md`; docs/design-free-grace-sustain.md KD-FG-11): an acknowledgement candidate qualifies on the first purging pass that BEGAN ≥ `writer checkpoint landing ceiling + skew_max` after its label was learned — the ceiling is the writer's cadence trigger plus two checkpoint-task tick periods (`checkpoint_landing_ceiling_ms`; 1,100 ms on the shipped 50 ms flush), read through `MemberSession::checkpoint_ceiling_ms`. The retired window `staleness_bound + skew_max` added the reader's poll interval on top of the ceiling, but for qualification the pass IS the poll, so the interval bounded nothing (−900 ms of hold on the fleet cadences). `0` = `staleness_bound + skew_max` verbatim (the A/B control). Member-side. Published: free_grace_qualify_lag_ms."),
    k("SQUEEZEFS_FREE_GRACE_DRAIN_EPOCH_STAMP", Kind::Bool, "on", "The reader ack ladder's drain window re-derived, part one (ladder re-derivation item 2, user decision 2026-09-06): the daemon's layout cache stamps every entry with the reader's purge GENERATION it was resolved under (`ro_coherence::reader_step_generation`, read before the backend read) and treats an entry stamped below the current generation as a MISS on every binding-serving read — the epoch step's last act bumps the generation, so no pre-step block binding can be served after the step (one relaxed load + compare per serve; one re-resolve per cached ino per step). The drain then no longer waits out the caches' TTL: `drain = D_purge` instead of `staleness_bound + D_purge` (−2,000 ms of hold on the fleet cadences). `0` = the caches serve to their TTL and the drain keeps `S` (the A/B control; the generation keeps counting). Member/co-writer-side; structurally inert on a mount that never steps an epoch. Published: free_grace_drain_lag_ms, reader_layout_step_gen, reader_layout_step_misses."),
    k("SQUEEZEFS_FREE_GRACE_DRAIN_OBSERVED", Kind::Bool, "on", "The reader ack ladder's drain window re-derived, part two (ladder re-derivation item 3, user decision 2026-09-06): the `D_purge = 2 × P` term (the §6.7 lease-clock fail-stop reserve, reused as \"serves in flight when the purge ran must finish\") becomes an OBSERVED drain — every read serve (the FUSE handler, the R-2 fast probe, the il sync probe, the direct-drive DMA to its CQE, the prefetch/read-lane fill tasks, copy_file_range's source read) stamps the purge generation it started under and counts itself in that generation's slot (one SeqCst increment + one SeqCst decrement per serve, per-thread shards, same-word pairs; `ro_coherence::ServeStamp`), and an acknowledgement candidate promotes once every slot below its qualifying generation reads zero — re-checked on the next pass or on the completion wake of the last such serve. Strictly safer than any timer (no timer bounds a serve a fabric timeout can stretch; the count IS the serves) and it completes in the serve residence (ms) instead of two poll intervals (−2,000 ms of hold on the fleet cadences). `D_purge` survives only as the tripwire `free_grace_drain_overdue` (must stay 0; it never shortens the wait). `0` = the `D_purge` timer verbatim and the ledger unarmed (the A/B control). Member/co-writer-side. Published: free_grace_drain_{observed,overdue,wakes}, reader_serves_inflight, reader_serve_step_races, reader_serve_slot_overruns."),
    k("SQUEEZEFS_FREE_GRACE_CHECKPOINT_COMPOSITE", Kind::Bool, "on", "The freed-offset grace loop's writer→member CHECKPOINT COMPOSITE (finding 15 adjudication item 4, user decision 2026-09-06; `.benchmarks/2026-09-06-free-grace-checkpoint-composite.md`; docs/design-free-grace-sustain.md §Hold-time campaign). A faster writer checkpoint alone was measured INERT (the reader qualifies on a time bound, never on observing the checkpoint); it pays as the composite: while the valve is ASKING members to answer sooner (a prod in force — rung (a) off the space runway or rung a′ off the demand mark) the authority's KV checkpoint ceiling becomes `max(P/2, 2 × measured cycle)` against the reader's routine poll P (the Nyquist bound — a new root in every pass window; the routine `CHECKPOINT_MAX_AGE_MS` ceiling with no ask), every membership grant CARRIES the live ceiling (a promise honoured for one routine ceiling past the grant), and on the member it is the prod floor AND L2b's pass floor — so passes and beats run at P/2 too, halving the hold's learn, qualify-rounding and carry terms (in-process `bound_age` 7,724 → 6,750 ms, −974 ms, on the fleet-cadence shape). The accepted COST: ≈ 2× lease-lane renewals and ≈ 2× checkpoint cycles while an ask is in force. The published reader_staleness_bound_ms never moves (the elastic cadence sits inside it). `0` = the shipped shape exactly: the writer's constant/tick, the routine ceiling on every grant, the constant as the member's floor (A/B control). Authority + member side. Engagement: free_grace_checkpoint_ceiling_ms (the ceiling in force) / free_grace_checkpoint_elastic_cycles (writer), free_grace_pass_prods (member — L2b's first engagement on the floor venue)."),
    k("SQUEEZEFS_ALLOC_LANE_VOLUME_HINT", Kind::Bool, "on", "The co-writer lane refill's PER-VOLUME hint (finding 15's file-per-proc residue, `.benchmarks/2026-09-07-cowriter-fpp-supply-residue.md`). The renewal grant's `lane_supply_blocks` is the authority's count of this lane's blocks on its free lists SUMMED over the mount's data volumes, while a decline and a pushed refill are per allocator, i.e. per volume — so on the s11 fleet the volume the authority held nothing for asked on every wake because its sibling made the sum nonzero (≈ 20 % of a fpp co-writer's harvest RPCs came back empty), and a decline on it ended at every grant whether or not anything of ITS lane had moved. The grant now also carries the vector `(vol_tag, blocks)` per data volume (`Grant::lane_supply_volumes`, CLUSTER_WIRE_SCHEMA 3 — KD-7 same-commit fleets). ON (the default): each allocator reads ITS volume's entry — the pushed decision and the ahead witness fire on it, and the single-flight decline's witness advances only when the authority advertises nonzero supply for that volume (a volume the vector does not name keeps the every-grant law). `0` = the summed hint and the mount-wide arrival generation for every volume verbatim (the A/B control; the vector still travels). Co-writer side only; inert on every mount with no partition. Engagement: alloc_lane_volume_hint_skips (pushed decisions the vector declined that the sum would have fired — the empty RPCs saved)."),
    k("SQUEEZEFS_FREE_GRACE_LANE_PUSH", Kind::Bool, "on", "The freed-offset grace loop's LANE-PUSH lever (finding 15 term 2, `.benchmarks/2026-09-06-free-grace-lane-visible.md`): the fleet-only `min_acked→released` term. The authority's harvest is DEMAND-driven (a free, an allocation, a co-writer's harvest RPC) and a released co-writer-lane block is reachable only through that co-writer's next harvest RPC (its ENOSPC park slices or the 1 s watermark tick, which decays dark on a quiet lane). ON (the default): (i) a BINDING member's advancing acknowledgement harvests every ring to its uncovered front on arrival — rate-limited by exactly lever (d)'s law (`ack_refresh_floor ÷ members`, never faster than 2× the measured scan) — so the release follows the ack, not the next demand event; (ii) the membership renewal grant carries `lane_supply_blocks`, the blocks of that member's lane on the authority's free lists (O(1) per-lane counters, never a scan — KD-FG-4), and a co-writer learning a nonzero hint wakes its refill at once (the ahead task and the bounded allocation park both wait on the wake beside their own cadence; the hint alone is the pushed refill's sufficient condition — `SQUEEZEFS_ALLOC_LANE_REFILL_HINT`). `0` = the A/B control: releases at the next demand event, the hint reads 0, refills on the watermark tick and the park slices verbatim. Authority + co-writer side; inert on every mount with no partition. Engagement: free_grace_lane_push_releases / free_grace_lane_push_hints (authority), free_grace_lane_push_wakes / alloc_lane_pushed_harvests (co-writer); the hop itself is alloc_lane_visible_phase_ns."),
    k("SQUEEZEFS_FREE_GRACE_VALVE", Kind::Bool, "on", "Spec §6.8 item 3's pressure-coupled release valve (rung-20 residual 6, docs/design-full-multi-writer.md): the graded ladder that keeps a rewrite storm's deferrals from outrunning the readers' releases. ON (the default): the grace ring's own measured deferral rate against the smaller of its headroom and the volume's free supply produces a RUNWAY in ms, and as it shortens the writer (a) grants the members it is waiting on a shorter renewal cadence — derived by inverting `free_grace::ack_cycle` against that runway, floored at the shortest interval a reader's answer can change in — and (b) slides the fence deadline from the routine bound toward the pressure bound (never below one honest acknowledgement cycle). `0` = the A/B control: the routine bound, the pressure bound at the allocation cliff, and nothing else — the pre-campaign shape, whose measured field signature is `free_grace_offsets` climbing monotonically until the lane's share ENOSPCs. Inert unless a reader plane is armed. Engagement: free_grace_prods / free_grace_bound_tightenings / free_grace_pressure_pct."),
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
    k("SQUEEZEFS_TEST_STAMP_BLOCK_REFS", Kind::Bool, "0", "Test seam: stamp incompat bit 9 (durable block refcounts) at format, so a suite can point the write-path fixtures at the durable ledger IN ISOLATION on a single-writer-class format and let the §6.2-item-1 oracle grade them. Never set in production — the default format carries the bit anyway since the rung-10b Phase-B flip; only `--single-writer` formats omit it."),
    k("SQUEEZEFS_TEST_STAMP_WRITER_SCOPE", Kind::Bool, "0", "Test seam: stamp incompat bit 10 (writer-scoped staging — §6.2 items 8/10) at format, so a suite can drive the scoped-key + node-scoped-stamp path IN ISOLATION on a single-writer-class format. Never set in production — the default format carries the bit anyway since the rung-10b Phase-B flip; only `--single-writer` formats omit it."),
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
    k("SQUEEZEFS_D2_CRASH_CHILD", Kind::Harness, "-", "Two-stage-conveyor crash-suite child marker."),
    k("SQUEEZEFS_D2_CRASH_ROUNDS", Kind::Harness, "-", "Two-stage-conveyor crash-suite round count."),
    k("SQUEEZEFS_D2_LEDGER", Kind::Harness, "-", "Two-stage-conveyor crash-suite ledger path."),
    k("SQUEEZEFS_D2_VOL", Kind::Harness, "-", "Two-stage-conveyor crash-suite volume path."),
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
/// KD-MW-16: fleet-parallel maintenance participation (worker arm on
/// members, fan-out arm on the coordinator). Default ON; `=0` disarms
/// this mount's half only.
pub fn fleet_jobs_enabled() -> bool {
    bool_knob("SQUEEZEFS_FLEET_JOBS", true)
}

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
