//! The env-knob registry and the ONE parsing convention (charter §1.6).
//!
//! Every `SQUEEZEFS_*` knob the product reads is a row in `registry`. The
//! convention, byte-for-byte the reference's (`docs/porting/knobs_and_rails.md`
//! §1):
//!   - absent = unset / empty / whitespace-only (the value is trimmed first);
//!   - booleans: `1/true/yes/on` vs `0/false/no/off`, case-insensitive;
//!     `=0` DISABLES even a default-ON knob;
//!   - integers carry an inclusive admissible range; out-of-range REFUSES,
//!     never clamps, and the message names the range (what IS admissible),
//!     never the integer type;
//!   - enums match case-insensitively and yield the canonical spelling;
//!   - strings are presence-only (a bad path is the consumer's better error);
//!   - a RETIRED knob with any present value refuses naming its successor
//!     (forward-only — never a silent alias);
//!   - a malformed knob refuses the PROCESS at startup, naming every offender
//!     at once (`refusalReport`); no call site may panic on a knob value and
//!     none may silently default one;
//!   - an unregistered `SQUEEZEFS_*`/`SQZ_*` name is announced as a probable
//!     typo, never refused (mixed-version fleets and the shim's client-side
//!     knobs share one environment).
//!
//! Adding a knob means adding a registry row: `tests/rails.zig` fails the gate
//! on an unregistered knob literal in the source tree, and on a registered
//! non-retired knob nothing reads.
//!
//! Precedence for sizing families stays `explicit > percentage > derived`,
//! implemented per family in a pure `resolve*` function that never refuses
//! (the gate already did) — see `mem/budget.zig`.

const std = @import("std");
const log = @import("log.zig");

pub const Kind = union(enum) {
    bool,
    /// Inclusive admissible range.
    int: struct { lo: i128, hi: i128 },
    enumeration: struct {
        allowed: []const []const u8,
        /// `(value, why)` pairs: a retired VALUE refuses naming why.
        retired: []const struct { value: []const u8, why: []const u8 } = &.{},
    },
    /// Presence only.
    str,
    /// Any present value refuses naming the successor.
    retired: struct { successor: []const u8 },
    /// Read only by test binaries, rigs and scripts: registered so the census
    /// passes, never validated.
    harness,
};

/// The charter's classes (§1.6: delete / `SQUEEZEFS_TEST_*` seam / A/B
/// lever) plus the sizing and bookkeeping shapes. There is no "tuning" class:
/// a literal shipped default with a knob on top is a free-floating constant
/// (§1.5) — such a row is an A/B lever whose doc names the control arm, or it
/// is deleted (decision 0003 Q7).
pub const Class = enum {
    /// A real feature/posture toggle.
    switch_,
    /// Int whose absence means "derive it"; explicit wins verbatim.
    derived_override,
    /// Product code armed only by suites (`SQUEEZEFS_TEST_*`); default-inert.
    test_seam,
    /// Same-binary measurement control arm; the doc names the arm; logged at
    /// mount when non-default.
    ab_lever,
    /// Paths, URIs, addresses.
    config,
    enum_select,
    harness,
    retired,
};

pub const Knob = struct {
    key: []const u8,
    kind: Kind,
    /// The default: "derived" (absent ⇒ derive; read through `optIntKnob`),
    /// a literal the typed readers parse at comptime (`intKnob`, `boolKnob`,
    /// `enumKnob` — the ONE source of the default, never a call-site
    /// literal), or "-" (no default: harness, retired).
    default: []const u8,
    class: Class,
    doc: []const u8,
};

/// Range ceilings shared by the table (`docs/porting/knobs_and_rails.md` §2.1).
/// 10 minutes: the longest a timing knob may park anything.
pub const MS_MAX: i128 = 600_000;
/// 1 TiB: the largest byte-count any knob may name.
pub const BYTES_MAX: i128 = 1 << 40;
pub const MIB_MAX: i128 = BYTES_MAX >> 20;

/// THE REGISTRY. Grouped by subsystem; rows land with the module that reads
/// them (the inverse-census rail refuses a row nothing reads).
pub const registry = [_]Knob{
    // ---- R5 memory budget ----
    .{ .key = "SQUEEZEFS_MEM_BUDGET_MB", .kind = .{ .int = .{ .lo = 1, .hi = MIB_MAX } }, .default = "derived", .class = .derived_override, .doc = "R5 memory budget, MiB (absolute > cgroup memory.max × 0.8 > 70 % RAM)." },
    .{ .key = "SQUEEZEFS_TRANSPORT_MEM_MAX", .kind = .{ .int = .{ .lo = 1, .hi = MIB_MAX } }, .default = "derived", .class = .derived_override, .doc = "Transport payload-buffer cap, MiB (absolute > pct > derived budget/8)." },
    .{ .key = "SQUEEZEFS_TRANSPORT_MEM_PCT", .kind = .{ .int = .{ .lo = 1, .hi = 100 } }, .default = "derived", .class = .derived_override, .doc = "Transport payload-buffer cap as a percentage of the R5 budget." },

    // ---- io_uring file layer / metadata device posture ----
    .{ .key = "SQUEEZEFS_URING_FS_WORKERS", .kind = .{ .int = .{ .lo = 1, .hi = 64 } }, .default = "derived", .class = .derived_override, .doc = "io_uring file-layer worker count for ad-hoc local file I/O (derived clamp(cpus/4, 4, 64))." },
    .{ .key = "SQUEEZEFS_TEST_META_BUFFERED", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: keep every registered metadata device on BUFFERED I/O (the shared-LUN coherence repro's control arm)." },

    // ---- metadata commit pipeline (journal lane) ----
    .{ .key = "SQUEEZEFS_META_FLUSH_INTERVAL_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "50", .class = .ab_lever, .doc = "Coalesced journal barrier period, ms (the durability lane sets needs_flush and barriers at this cadence). A/B lever: 0 is the strict control arm — ONE barrier per group before any ack." },

    // ---- NVMe block device workers ----
    .{ .key = "SQUEEZEFS_NVME_READ_LANES", .kind = .{ .int = .{ .lo = 1, .hi = 1024 } }, .default = "derived", .class = .ab_lever, .doc = "Per-device READ submission fan-out lanes (derived cpus ÷ data devices). A/B lever: 1 is the single-submitter control arm (11.5 GB/s vs 16.7 GB/s at 4 lanes on the 8-namespace box)." },
    .{ .key = "SQUEEZEFS_NVME_WRITE_LANES", .kind = .{ .int = .{ .lo = 1, .hi = 1024 } }, .default = "derived", .class = .ab_lever, .doc = "Per-device DATA-WRITE submission fan-out lanes, block-offset affinity (derived cpus ÷ data devices). A/B lever: 1 is the single-submitter control arm." },

    // ---- R5 fleet share + IPC arena sizing (read by mem/) ----
    .{ .key = "SQUEEZEFS_TEST_FLEET_SHARE", .kind = .{ .int = .{ .lo = 1, .hi = 4096 } }, .default = "derived", .class = .test_seam, .doc = "Test seam: pin the fleet share — N co-located daemons sharing one machine — instead of the per-tick census of squeezefs daemons in this process's cgroup (`cgroup.procs` ∩ comm). The share divides the cgroup/RAM roots, the CPU root and the cgroup unreclaimable sample (ceil) BEFORE the ×0.8/×0.7 laws; explicit budgets and floors are never divided." },
    .{ .key = "SQUEEZEFS_IPC_MEM_MAX", .kind = .{ .int = .{ .lo = 1, .hi = MIB_MAX } }, .default = "derived", .class = .derived_override, .doc = "ipc_session_arenas admission cap, MiB (absolute > pct > derived budget/8)." },
    .{ .key = "SQUEEZEFS_IPC_MEM_PCT", .kind = .{ .int = .{ .lo = 1, .hi = 100 } }, .default = "derived", .class = .derived_override, .doc = "ipc_session_arenas admission cap as a percentage of the R5 budget." },
    .{ .key = "SQUEEZEFS_IPC_ARENA_MB", .kind = .{ .int = .{ .lo = 1, .hi = 1 << 20 } }, .default = "derived", .class = .derived_override, .doc = "Per-session IPC payload arena, MiB (derived max(64 MiB, align_down(ipc cap / session population target, 4 MiB)))." },

    // ---- 4a metadata object locks (read by meta/dlm.zig) ----
    .{ .key = "SQUEEZEFS_DLM_STRIPES", .kind = .{ .int = .{ .lo = 1, .hi = 1 << 24 } }, .default = "derived", .class = .ab_lever, .doc = "D-3: the stripe population of the 4a DlmLockManager inode + dentry classes (shipped floor 4096). Explicit wins verbatim (rounded UP to a power of two — the index is a mask); derived = next_pow2(max(4096, 16 × possible_cpus × q_depth)) so a false-sharing wait — which on the 4a tables costs a whole commit — hits at most one acquire in 16 at full ring occupancy. A/B lever: 4096 is the shipped-4a control arm, 1 the everything-serializes crucible. Engagement: dlm_{inode,dentry}_stripes; conviction: dlm_*_stripe_collisions vs dlm_*_key_waits." },

    // ---- MetaLV node cache (read by meta/node_cache.zig) ----
    .{ .key = "SQUEEZEFS_META_NODE_CACHE_MB", .kind = .{ .int = .{ .lo = 1, .hi = MIB_MAX } }, .default = "derived", .class = .derived_override, .doc = "Per-volume node-cache budget, MiB, verbatim (absolute > SQUEEZEFS_META_NODE_CACHE_PCT > derived max(mem_budget/16, 512 MiB))." },
    .{ .key = "SQUEEZEFS_META_NODE_CACHE_PCT", .kind = .{ .int = .{ .lo = 1, .hi = 100 } }, .default = "derived", .class = .derived_override, .doc = "Per-volume node-cache budget as a percentage of the R5 memory budget (below the absolute MiB knob, above the derived default)." },

    // ---- MetaLV checkpoint task (read by meta/checkpoint.zig) ----
    .{ .key = "SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES", .kind = .{ .int = .{ .lo = 1, .hi = 1 << 32 } }, .default = "derived", .class = .derived_override, .doc = "Dirty-node checkpoint cap = the mount-replay working-set bound: a cycle is due once more nodes than this hold un-durable records. Explicit wins verbatim; derived max(4096, mem_budget/32 ÷ node_size) — bounds the RAM pinned by dirty nodes AND the replay working set, 4096 the shipped floor (never regress below shipped: a lower cap checkpoints more often than any box ran, pure overhead)." },
    .{ .key = "SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", .kind = .{ .int = .{ .lo = 1, .hi = MS_MAX } }, .default = "1000", .class = .ab_lever, .doc = "Checkpoint cadence ceiling, ms: a cycle runs at least this often with no other trigger (charter §5.3 'every ≤ 1 s'); also the reader's staleness term and the base of the landing ceiling max_age + 2 × tick (KD-SYM-10, 1,100 ms shipped). A/B lever: 1000 is the charter's shipped arm; a shorter age is the replay-bound crucible (tighter mount replay for more checkpoint I/O). Floor 1 ms = the physical minimum of a cadence; ceiling the timing-knob bound." },

    // ---- the commit conveyor (read by meta/conveyor.zig, meta/backend.zig) ----
    .{ .key = "SQUEEZEFS_META_COMMIT_BATCH_TXS", .kind = .{ .int = .{ .lo = 1, .hi = 1 << 20 } }, .default = "derived", .class = .ab_lever, .doc = "M7 commit-conveyor batch cap, transactions per drained batch. Explicit wins verbatim; derived max(64, cpus × 2) — arrivals scale with handler parallelism, 64 the shipped M7 floor. A/B lever: 1 is the per-tx pre-conveyor control arm." },
    .{ .key = "SQUEEZEFS_META_COMMIT_BATCH_BYTES", .kind = .{ .int = .{ .lo = 1, .hi = BYTES_MAX } }, .default = "derived", .class = .ab_lever, .doc = "M7 commit-conveyor batch cap, Σ entry bytes per drained batch. Explicit wins verbatim; derived max(256 KiB, ring/16) so ≥ 16 batch reservations always cycle the ring; both forms are clamped at open to the ring's user-admissible capacity so one Σ-admission can always eventually succeed. A/B lever: the byte half of the batch-vs-per-tx arm (`_TXS = 1`); a byte cap never splits a single transaction." },
    .{ .key = "SQUEEZEFS_TEST_CRASH_CHILD", .kind = .harness, .default = "-", .class = .harness, .doc = "Harness: the kill -9 matrix's re-exec'd child marker — the metadata volume path the child opens and churns (its ack ledger is `<path>.acks`); the parent test SIGKILLs it mid-stream and reopens." },
    .{ .key = "SQUEEZEFS_TEST_SMOKE_STAGE_LOOPS", .kind = .harness, .default = "-", .class = .harness, .doc = "Harness: the smoke's looped single-stage repro — the cache-path 4 KiB overwrite stage and the 8-way storm stage run this many times each on ONE mount (distinct source pages per pass, `cmp` against the mirror, the must-stay-0 gauges after every pass); unset ⇒ the test skips. `scripts/venue \"SQUEEZEFS_TEST_SMOKE_STAGE_LOOPS=24 zig build --cache-dir /build/zig-cache --prefix zig-out-linux smoke -Dtest-filter=LOOPED\"`." },

    // ---- the FUSE library: transport geometry (read by fuse/geometry.zig) ----
    .{ .key = "SQUEEZEFS_FUSE_MAX_WRITE", .kind = .{ .int = .{ .lo = 4096, .hi = std.math.maxInt(u32) } }, .default = "derived", .class = .derived_override, .doc = "INIT max_write DESIRE, bytes, verbatim (derived max(block_size, 1 MiB)); the plan still clamps it to the kernel's max_pages_limit sysctl and degrades it under the payload-buffer cap, so the published transport_max_write is what the kernel runs." },
    .{ .key = "SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH", .kind = .{ .int = .{ .lo = 1, .hi = 32 } }, .default = "derived", .class = .derived_override, .doc = "FUSE-over-io_uring per-queue ring depth, verbatim (derived clamp(cap / (nqueues × payload_sz), 4, 32): 32 the measured-best, 4 the shipped floor); explicit bypasses the budget ladder. Ceiling 32 = the kernel's per-queue entry population the measured-best posture was taken at." },
    .{ .key = "SQUEEZEFS_TEST_FUSE_QUEUES", .kind = .{ .int = .{ .lo = 1, .hi = 512 } }, .default = "derived", .class = .test_seam, .doc = "Test seam: FUSE-over-io_uring queue count instead of the kernel's possible CPUs. Fewer queues than possible CPUs never becomes ready (the kernel routes to the ring only once every queue holds an entry) — a product mount never sets it." },
    .{ .key = "SQUEEZEFS_FUSE_DRAIN_GROUP", .kind = .{ .int = .{ .lo = 1, .hi = 512 } }, .default = "derived", .class = .derived_override, .doc = "Drain-group width (member queues per sqz-fuse-ur worker), verbatim within a NUMA node — node containment is structural, an explicit width never spans a node (derived max(node_cpus / 4, 1); 1 under kmbuf/zc where registration is per ring per queue)." },

    // ---- the FUSE library: kernel TTL classes (read by fuse/mount_options.zig) ----
    .{ .key = "SQUEEZEFS_FUSE_ATTR_TTL_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "1000", .class = .ab_lever, .doc = "Kernel attribute-cache TTL, ms (GETATTR/SETATTR replies and the attr half of every entry reply); `-o attr_timeout=` overrides it. A/B lever: 1000 is the shipped arm; 0 the no-cache control arm (every stat reaches the daemon)." },
    .{ .key = "SQUEEZEFS_FUSE_ENTRY_TTL_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "1000", .class = .ab_lever, .doc = "Kernel dentry TTL for non-directory entries, ms; `-o entry_timeout=` overrides it. A/B lever: 1000 is the shipped arm; 0 the no-cache control arm (every path walk reaches the daemon)." },
    .{ .key = "SQUEEZEFS_FUSE_DIR_ENTRY_TTL_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "1000", .class = .ab_lever, .doc = "Kernel dentry TTL for directory entries, ms (a directory dentry's invalidation covers a subtree, hence its own class); `-o dir_entry_timeout=` overrides it. A/B lever: 1000 is the shipped arm; 0 the no-cache control arm." },
    .{ .key = "SQUEEZEFS_FUSE_NEGATIVE_TTL_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "1000", .class = .ab_lever, .doc = "Kernel negative-dentry TTL, ms: a LOOKUP miss replies a cacheable negative entry (nodeid 0 + entry_valid) for this long; 0 restores the bare ENOENT. `-o negative_timeout=` overrides it. A/B lever: 1000 is the shipped arm; 0 the bare-ENOENT control arm." },

    // ---- the executor (read by exec/placement.zig, exec/sqz_io.zig, exec/lane.zig) ----
    .{ .key = "SQUEEZEFS_FUSE_PIN_SCOPE", .kind = .{ .enumeration = .{ .allowed = &.{ "node", "core" } } }, .default = "node", .class = .enum_select, .doc = "Handler-lane and transport-worker affinity scope: `node` (the lane may run on any CPU of its home NUMA node — the serve-latency decomposition convicted the 1-CPU pin: queue_wait 1.55 + dispatch_lag 1.70 ms/op on cold reads, ~99 % collapse with widening) or `core` (the pre-campaign hard pin, the control arm of that measurement)." },
    .{ .key = "SQUEEZEFS_NUMA", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "NUMA placement actions (lane affinity by node, node-grouped dispatch). A/B lever: 0 makes no affinity call and ignores node groups while the node map and gauges stay live — the single-node-posture control arm; structurally inert on single-node hosts." },
    .{ .key = "SQUEEZEFS_FUSE_SAME_LANE_DISPATCH", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "A spawn issued from a lane thread lands on THAT lane instead of the fuse-lane rotation (deletes one cross-thread wake per op; the rotation measured 412 µs dispatch_lag at 256 in flight). A/B lever: 0 is the rotation control arm." },
    .{ .key = "SQUEEZEFS_TEST_EXEC_DROP_WAKE", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: every lane drops the FIRST cross-thread eventfd wake write after it starts, so the park tick's lost-wake rescue (exec_lane_tick_rescues) is exercised deterministically." },

    // ---- the FUSE handler layer (read by fuse/handler/{op_trace,op_guard}.zig) ----
    .{ .key = "SQUEEZEFS_OP_PROFILE", .kind = .bool, .default = "0", .class = .ab_lever, .doc = "Per-op phase histograms (fuse_op_phase_ns[class][handler_to_backend|backend|backend_to_reply|total], exact-sum) armed at mount: three extra CLOCK_MONOTONIC reads per op. A/B lever: 0 is the shipped arm — the prologue is ONE relaxed load of the armed word, no clock read, no allocation; the `.stats` surface of a disarmed mount is byte-identical." },
    .{ .key = "SQUEEZEFS_WRITE_SHARED", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "Shared-mode inode admission for cache-resident striped writes (charter §6.2: a WRITE whose range the resident caches prove within-EOF takes the per-inode op guard SHARED; readers run concurrently). A/B lever: 1 is the shipped arm (+7–8 % rand-4k IOPS, 523/530 k vs 489/491 k A-B-B-A); 0 is the exclusive control arm — every write classifies EntireOp/MetaPrepOnly and the 1.12 ms/op scheduler-wake convoy at 32 × qd32 returns." },

    // ---- the FUSE library: the ring transport (read by fuse/transport/session.zig) ----
    .{ .key = "SQUEEZEFS_FUSE_READ_FUSION", .kind = .bool, .default = "0", .class = .ab_lever, .doc = "READ dispatch law arm (b): on a zero-copy session a READ at or under the fusion ceiling (payload/8, floor 4096) is polled on the worker's own fused lane instead of a handler lane. A/B lever: 0 is the shipped arm — composed behind the inline warm-ladder serve the fusion read −6.5 % IOPS (481k vs 514k), p50 +70 µs, p99 1.5 vs 4.3 ms: the two levers converge on the reap thread's per-op serialization; standing alone it won +15.5 %. Inert on a UserEnts (stock) session." },
    .{ .key = "SQUEEZEFS_FUSE_SQPOLL", .kind = .bool, .default = "0", .class = .ab_lever, .doc = "Build each transport worker's ring with IORING_SETUP_SQPOLL (a kernel submission poller per ring; never composed with DEFER_TASKRUN, which the poller's task-work ownership forbids). A/B lever: 0 is the shipped arm — SQPOLL was measured, not recommended (M10): the worker already carries every SQE in the ONE loop-bottom enter it needs for the wait, so the poller buys a core and saves no syscall on the saturated path." },
    .{ .key = "SQUEEZEFS_TEST_FUSE_REGISTER_REFUSE", .kind = .{ .int = .{ .lo = 0, .hi = 1 << 20 } }, .default = "0", .class = .test_seam, .doc = "Test seam: the first N REGISTER pushes across the session's workers are answered as refused (EBUSY) without reaching the kernel, so the backoff ladder (1 ms doubling to 128 ms, retirement on the 8th) and the arm barrier's submission count are exercised deterministically." },

    // ---- the FUSE library: the buffer-mode lattice (read by fuse/transport/{session,probes}.zig) ----
    .{ .key = "SQUEEZEFS_FUSE_BUFFER_MODE", .kind = .{ .enumeration = .{ .allowed = &.{ "auto", "user", "bufring", "zc" } } }, .default = "auto", .class = .enum_select, .doc = "The buffer-mode lattice rung (charter §4): `auto` takes the highest rung the PROBES confirm (ZeroCopy on a full sqz kernel with CAP_SYS_ADMIN, BufRing with kmbuf alone, UserEnts on a stock kernel); `user` / `bufring` / `zc` pin a rung for an A/B measurement. An explicit rung the probes refuse FAILS THE MOUNT LOUD naming the probe — the probe is the only degrade point, never a silent downgrade (charter §3)." },
    .{ .key = "SQUEEZEFS_READ_ZC_SERVE", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "On a zero-copy session the filesystem's whole-block fill pool is ONE MAP_SHARED memfd slab and a warm READ serve bridges READ_FIXED(pool fd @ off → slot) with zero daemon passes (`replyFdBody`); the session tells the filesystem through InitCaps.read_zc_serve. A/B lever: 1 is the shipped arm (measured daemon CPU/GiB −24 % cold / −25 % warm, passes/byte 2.0 → 1.0 warm); 0 is the control arm — the serve is one copy into the bounce window, bridged from there." },
    .{ .key = "SQUEEZEFS_TEST_FUSE_CAP_SYS_ADMIN_ABSENT", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: the zero-copy probe reads CapEff as lacking CAP_SYS_ADMIN, so the lattice's refusal arms (explicit `zc` fails the mount loud; `auto` settles on BufRing) are exercised on a venue that holds the capability." },
    .{ .key = "SQUEEZEFS_TEST_ZC_BRIDGE_STALL", .kind = .{ .int = .{ .lo = 0, .hi = 1 << 20 } }, .default = "0", .class = .test_seam, .doc = "Test seam: the next N reply bridges target a pipe with no writer instead of their source fd, so the bridge never completes and the 30 s deadline ladder (AsyncCancel once, the cancel-CQE table, the header-only -EIO commit) is exercised on a real ring." },
    .{ .key = "SQUEEZEFS_TEST_ZC_BRIDGE_DEADLINE_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "0", .class = .test_seam, .doc = "Test seam: the bridge deadline in ms instead of the 30 s constant (0 = the constant), so the stall seam resolves within a test's budget." },

    // ---- the data plane: layouts, packing, staging (read by data/{layout,pack,staging}.zig) ----
    .{ .key = "SQUEEZEFS_INLINE_MAX_BYTES", .kind = .{ .int = .{ .lo = 4096, .hi = 65_536 - 4096 } }, .default = "derived", .class = .ab_lever, .doc = "The inline layout ceiling, bytes, verbatim up to THIS set's format bound min(64 KiB, node_size/4) − 4 KiB (an override above the bound REFUSES the mount naming the bound — never clamped); derived = one page. A/B lever: the page is the shipped arm — 16 KiB measured −34 % files/s, 2.0× fsync µs, 59× journal bytes/file; 32 KiB −68 %, 117× and a fail-stopped 1 GiB metadata volume. Published as inline_max_bytes." },
    .{ .key = "SQUEEZEFS_SMALL_FILE_PACKING", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "GPFS-style small-file packing: a promoted staged image whose 4 KiB slot is ≤ pack_max_slot lands as a tenant of the open pack block instead of its own 4 MiB block. A/B lever: on is the shipped arm (96,000 fsync-promoted 16 KiB files = 375 blocks instead of 96,000; dismount 20,000 → 148; files/s +0.5 %); 0 is the one-block-per-file control arm (the read funnel is lever-independent)." },
    .{ .key = "SQUEEZEFS_PACK_MAX_SLOT_BYTES", .kind = .{ .int = .{ .lo = 4096, .hi = 4 << 20 } }, .default = "derived", .class = .ab_lever, .doc = "The own-block threshold, bytes: a slot above it takes its own block, at or below it packs; also the compaction victim line (a pack block whose live bytes are ≤ it is a victim). Derived CHUNK/2 — the break-even: an own block wastes CHUNK − s forever, a packed tenant costs one compaction copy of s, so packing wins iff CHUNK − s ≥ s. A/B lever: CHUNK/2 is the shipped arm of the two-faced law; a lower line is the more-own-blocks control arm. Published as pack_max_slot_bytes." },
    .{ .key = "SQUEEZEFS_FSYNC_PROMOTE_STAGED", .kind = .bool, .default = "off", .class = .ab_lever, .doc = "fsync of a staged-layout file also promotes it (cross-client visibility at fsync instead of at the writer's clean unmount); placed before the data barrier so the barrier covers the block. A/B lever: off is the shipped arm — on measured −16.7 % files/s (338 → 425 µs, p99.9 +122 %) on the fsync-promote row; a failure never fails the fsync." },
    .{ .key = "SQUEEZEFS_PUBLISH_COALESCE_MAX", .kind = .{ .int = .{ .lo = 0, .hi = 1 << 20 } }, .default = "64", .class = .ab_lever, .doc = "Per-ino layout publish coalescing window: block-map inserts of one ino gathered into ONE delta record up to this many (charter §5.6; bounds the replay working set per key). A/B lever: 64 is the shipped arm; ≤ 1 is the serialized control arm (one record per block)." },
    .{ .key = "SQUEEZEFS_LAYOUT_DELTA_MAX_CHAIN", .kind = .{ .int = .{ .lo = 0, .hi = 1 << 20 } }, .default = "64", .class = .ab_lever, .doc = "Layout delta chain cap: a publish onto a key whose chain is this deep re-bases with a full Put instead of another delta (bounds the cold-read fold per key; charter §5.6). A/B lever: 64 is the shipped arm; 0 is the full-Puts-only control arm (the O(file-size) publish term returns)." },

    // ---- the data-block allocator, durable refs and reclaim (read by data/{block_alloc,block_refs,reclaim}.zig) ----
    .{ .key = "SQUEEZEFS_RECLAIM_BATCH_BLOCKS", .kind = .{ .int = .{ .lo = 1, .hi = 1024 } }, .default = "64", .class = .ab_lever, .doc = "Reclaim worker batch, blocks: the coalesce quantum (displaced blocks of one stream arrive back-to-back and merge into ranged discards) and the room-latency unit `batch ÷ drain rate`. A/B lever: 64 is the measured coalesce shape of the async-reclaim campaign; 1 is the per-block control arm (one device command per freed block)." },
    .{ .key = "SQUEEZEFS_RECLAIM_BATCH_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "2", .class = .ab_lever, .doc = "Reclaim worker accumulation window after its first wake, ms (large values park the worker — the tests' deterministic hold). A/B lever: 2 is the shipped arm; 0 the drain-at-once control arm." },
    .{ .key = "SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS", .kind = .{ .int = .{ .lo = 1, .hi = 1 << 20 } }, .default = "derived", .class = .ab_lever, .doc = "Deferred-space cap of the reclaim queue (queued + in-flight blocks). Explicit wins verbatim; derived clamp(displacement rate × room latency, 4096, budget/1024 ÷ entry). A/B lever: 4096 is the shipped-floor arm; the derivation is the park-don't-spill arm (an at-cap enqueue parks event-driven instead of spilling device commands inline)." },
    .{ .key = "SQUEEZEFS_RECLAIM_LANES_PER_DEV", .kind = .{ .int = .{ .lo = 1, .hi = 64 } }, .default = "32", .class = .ab_lever, .doc = "Per-device parallel drain lane cap; the width is demand-derived up to it (`clamp(ceil(demand ÷ batch), 1, lanes)`). A/B lever: 32 is the shipped arm (idle drain 2,700 → 5,900 cmd/s from 8 → 32 on the 4-node cluster; no sensitivity under foreground load); 1 is the serial-drain control arm (≈ 500 blocks/s vs ≈ 1,650/s displacement at the field's cap pin)." },
    .{ .key = "SQUEEZEFS_RECLAIM_CAP_PARK_MS", .kind = .{ .int = .{ .lo = 0, .hi = 60000 } }, .default = "derived", .class = .ab_lever, .doc = "At-cap enqueue park bound, ms. Explicit wins verbatim; derived clamp(4 × batch ÷ measured drain rate, 50, 1000) (fleet: 140 ms vs the pinned 1,000; A-B-B-A p99.9 80.6 ms vs 1,000 ms). A/B lever: 1000 is the pinned shipped arm; 0 never parks (immediate soft overflow, the control arm for the cap_overflow tripwire)." },
    .{ .key = "SQUEEZEFS_DISCARD_ELISION", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "Elide block-device discards into RAM debt while debt ≤ virgin bytes (discard is substrate hygiene, never correctness: unmapped blocks read zeros, reuse is guarded by write-before-publish + the incarnation word). A/B lever: on is the shipped arm; 0 is the queued-reclaim control arm AND the operational escape for substrates needing eager space return. Regular-file backings always keep the queued punch (host-FS space return is a real ENOSPC vector on overcommitted hosts)." },
    .{ .key = "SQUEEZEFS_FREE_FORENSICS", .kind = .bool, .default = "0", .class = .ab_lever, .doc = "Capture every free's provenance on a bounded tape of (block, site, thread, monotonic ns) records — no strings, no backtraces — so a double free pairs its two SITES. A/B lever: 0 is the shipped arm (the tape is not allocated); 1 is the diagnostic arm — 128 KiB per allocator and one record per free." },
    .{ .key = "SQUEEZEFS_BLOCK_REFS_VERIFY", .kind = .bool, .default = "0", .class = .ab_lever, .doc = "Run the durable-vs-derived block-reference oracle (fsck C8's walk) at MOUNT — pays the layout walk the ledger exists to delete; the drift report is logged with the first offenders' owners. A/B lever: 0 is the shipped arm (fsck C8 is unconditional and offline); 1 is the mount-time verification arm for a suspect volume." },
    .{ .key = "SQUEEZEFS_TEST_RECLAIM_STALL_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "0", .class = .test_seam, .doc = "Test seam: stall each reclaim batch by this many ms before its first device command (deterministic slow-device schedules for the valve-liveness and room-edge contracts)." },
    .{ .key = "SQUEEZEFS_TEST_CORPSE_SWEEP_FAIL_DESTROY", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: the corpse sweep's destroy rider refuses every chunk after the releases were planned (the released-but-undestroyed state the release witness must make harmless: a retry is a leak-free no-op)." },
    .{ .key = "SQUEEZEFS_TEST_DESTROY_CHUNK_STOP_AFTER", .kind = .{ .int = .{ .lo = 0, .hi = std.math.maxInt(u32) } }, .default = "0", .class = .test_seam, .doc = "Test seam: stop a chunked single-ino destroy after N committed entries (the crash-between-chunks state the next sweep must converge on)." },

    // ---- the data plane: transforms and the volume-set config (read by data/transform.zig, cli/format.zig, cli/mount.zig) ----
    .{ .key = "SQUEEZEFS_ENCRYPT_KEY_FILE", .kind = .str, .default = "-", .class = .config, .doc = "Key resolution rung 2 (charter §6.5): the path of the operator key FILE (≥ 32 B of material, mode 0600, owned by the invoking user or root, never a symlink) when `--encrypt-key` was not given; rung 3 is /etc/squeezefs/keys/<key_id>.key. The file's derived key id must equal the set's recorded one or the mount refuses loud." },
    .{ .key = "SQUEEZEFS_TEST_TRANSFORM_FORCE_RAW", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: every transformed block takes the store-raw arm regardless of compressibility, so the raw flag, the chunk headroom and the widest image are exercised with compressible data." },

    // ---- the data plane: the read path (read by data/{read_path,hot_tier,admission,prefetch,warm_ladder}.zig) ----
    .{ .key = "SQUEEZEFS_READ_TIER_ADMISSION", .kind = .{ .enumeration = .{ .allowed = &.{ "always", "second-touch", "never" } } }, .default = "second-touch", .class = .enum_select, .doc = "R1b scan-resistant admission for fills > 256 KiB: `second-touch` publishes to the NVMe read tier only on a ghost-table second miss (first touch ⇒ hot-tier probation, publish skipped — the 16.9 GiB-per-16 GiB tier-write tax deleted); `always` is the pre-R1b control arm (every fill publishes); `never` disables the disk tier's publishes. A hot budget of 0 degrades second-touch to always loudly." },
    .{ .key = "SQUEEZEFS_READ_MEM_CACHE_MB", .kind = .{ .int = .{ .lo = 1, .hi = MIB_MAX } }, .default = "derived", .class = .derived_override, .doc = "The read RAM cache ceiling (`--read-mem-cache-size`), MiB, verbatim: the ≤ 256 KiB read LRU's budget and the base of the hot-tier default (derived RAM / 10)." },
    .{ .key = "SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", .kind = .{ .int = .{ .lo = 0, .hi = MIB_MAX } }, .default = "derived", .class = .ab_lever, .doc = "R4 hot-block tier budget, MiB, verbatim; derived max(2 × block, read_mem / 4) (2 blocks = one stream's consume-behind window). A/B lever: the derivation is the shipped arm; 0 is the tier-off control arm (every whole-block serve is a hold/NVMe/device serve; admission degrades to `always`)." },
    .{ .key = "SQUEEZEFS_READ_DISK_CACHE_MB", .kind = .{ .int = .{ .lo = 1, .hi = MIB_MAX } }, .default = "derived", .class = .derived_override, .doc = "The NVMe read cache budget (`--read-disk-cache-size`), MiB, verbatim, split evenly per cache device; derived aggregate cache capacity / 4." },
    .{ .key = "SQUEEZEFS_READ_ADMISSION_FILL_PCT", .kind = .{ .int = .{ .lo = 0, .hi = 100 } }, .default = "5", .class = .ab_lever, .doc = "The admission governor's bounded-waste share: under the clamp (wasted ≥ 2 blocks ∧ 2 × wasted ≥ evicted over the previous 2 s epoch) ghost escalations and stream admissions may spend this percentage of the previous epoch's foreground device bytes, by one token CAS each. A/B lever: 5 is the shipped arm (`wasted ÷ device reads ≤ 5 %` under churn); 100 is the unclamped control arm; 0 denies every clamped admission." },
    .{ .key = "SQUEEZEFS_READ_PREFETCH_WINDOW", .kind = .{ .int = .{ .lo = 0, .hi = 1 << 20 } }, .default = "derived", .class = .ab_lever, .doc = "R2 prefetch window CAP per lane, blocks, verbatim up to 4096 (an override above it applies as 4096); derived clamp(share% × hot budget / block, 4, 4096). A/B lever: the derivation is the shipped arm; 0 is the kill switch (no speculative issue — the demand-only control arm)." },
    .{ .key = "SQUEEZEFS_READ_PREFETCH_SHARE_PCT", .kind = .{ .int = .{ .lo = 1, .hi = 100 } }, .default = "50", .class = .ab_lever, .doc = "The prefetch pipeline's share of the hot-tier budget (the resident-share denominator per active stream). A/B lever: 50 is the shipped arm; 100 the whole-tier arm, 1 the one-block arm." },
    .{ .key = "SQUEEZEFS_READ_RANGED_THRESHOLD", .kind = .{ .int = .{ .lo = 0, .hi = BYTES_MAX } }, .default = "262144", .class = .ab_lever, .doc = "R3: a cold request at or under this many bytes on a passthrough volume whose block is larger, not stream-classified, every tier probe missed ⇒ a RANGED 4 KiB-outward DMA (never published, never single-flighted) instead of a whole-block fill. A/B lever: 262144 is the shipped arm (1024× rand-4k amplification → 1.0×); 0 is the whole-block-only control arm." },
    .{ .key = "SQUEEZEFS_READ_DEST_LEASE", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "The dest lease: a ranged window that equals the request and lands 4 KiB-aligned DMAs straight into the registered reply window (zero daemon passes). A/B lever: 1 is the shipped arm (+28.6 % GB/s, daemon CPU/byte −50 %); 0 is the fill-into-bounce + serve-copy control arm." },
    .{ .key = "SQUEEZEFS_READ_LANE", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "The read lane: the hold (cohort-stability RAM store) + the probe-governed ahead issue. A/B lever: 1 is the shipped arm (qd8 +12.8 %, qd32 +14.5 %, read_amp 1.41 → 1.11); 0 is the A0 control arm — no hold, no ahead fetches, the R2 declines boundary at share == 0." },
    .{ .key = "SQUEEZEFS_READ_LANE_DEPTH", .kind = .{ .int = .{ .lo = 0, .hi = 1 << 16 } }, .default = "derived", .class = .ab_lever, .doc = "The ahead lane's depth, blocks, PINNED verbatim; derived = the closed-loop probe governor (both open-loop derivations — pure BDP and the AIMD window — were falsified by counted brackets and deleted). A/B lever: the governor is the shipped arm; 0 is the hold-only arm; a pin is the fixed-depth bracket." },
    .{ .key = "SQUEEZEFS_DIRECT_DEVICE_TRUE", .kind = .bool, .default = "0", .class = .ab_lever, .doc = "The measurement RULER (`-o direct_device_true`): an O_DIRECT read skips the classifier, every tier serve probe, the ghost, the escalation and the single-flight — N diagnostic readers = N device reads; every request size on a passthrough volume is a ranged window. Overlay probes stay (RYW is never diagnostic-optional). A/B lever: 0 is the shipped hybrid-I/O arm (O_DIRECT is NOT an admission class); 1 is the device-true ruler." },
    .{ .key = "SQUEEZEFS_TEST_READ_REBIND_LADDER_CAP", .kind = .{ .int = .{ .lo = 0, .hi = 24 } }, .default = "0", .class = .test_seam, .doc = "Test seam: the non-escalating binding-rebind ladder cap instead of MAX_REBINDS = 24 (0 = the constant), so exhaustion → the serialized settle is reachable without a 24-win schedule lottery." },
    .{ .key = "SQUEEZEFS_TEST_READ_ADMISSION_EPOCH_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "0", .class = .test_seam, .doc = "Test seam: the admission governor's epoch in ms instead of the 2 s house epoch (0 = the constant), so the roll, the clamp and the token mint are observable within a test's budget." },

    // ---- the data plane: the write path (read by data/write_admit.zig through its modules' resolvers) ----
    .{ .key = "SQUEEZEFS_PATCH_MAX_BYTES", .kind = .{ .int = .{ .lo = 0, .hi = BYTES_MAX } }, .default = "derived", .class = .ab_lever, .doc = "W1's ceiling and the device overlay's length floor, bytes, verbatim; derived block/8 (512 KiB at 4 MiB — the natural sub-block denominator; above it a fresh CoW destination beats patching in place under a crash window). A/B lever: the derivation is the shipped arm (rand-4k 354–397 → 61.5–66.7 k IOPS); 0 is the patch-OFF control arm (every length overlay-eligible)." },
    .{ .key = "SQUEEZEFS_FOLD_MAX_EXTENTS", .kind = .{ .int = .{ .lo = 1, .hi = 64 } }, .default = "derived", .class = .ab_lever, .doc = "W2's fold count trigger, extents per parked block, verbatim (bounded by the coverage table's 64 inline runs); derived block / (16 × 4 KiB) = 64 at 4 MiB — the measured amortization trigger (amp ≈ 2048/k + 2 = 34× at k = 64 under the 150× gate). A/B lever: the derivation is the shipped arm; a lower k is the earlier-fold bracket." },
    .{ .key = "SQUEEZEFS_FOLD_MAX_BYTES", .kind = .{ .int = .{ .lo = 4096, .hi = BYTES_MAX } }, .default = "derived", .class = .ab_lever, .doc = "W2's fold byte trigger, parked slab bytes per block, verbatim; derived block/4 (the former flat 1 MiB). A/B lever: the derivation is the shipped arm." },
    .{ .key = "SQUEEZEFS_PARKED_BUFFERS", .kind = .{ .int = .{ .lo = 1, .hi = 1 << 24 } }, .default = "derived", .class = .ab_lever, .doc = "The parked-write buffer budget, 4 MiB-class buffers, verbatim; derived max(256, mem_budget/16 ÷ block) — R5 already gauges and sheds parked RAM; 256 = the shipped floor, never regress. Red halves it. A/B lever: the derivation is the shipped arm." },
    .{ .key = "SQUEEZEFS_DEVICE_OVERLAY", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "Arm B of the write admission: zero-copy device overlay stores for page-aligned pieces above the patch ceiling (WRITE_FIXED straight from the registered slot or ONE DMA from an owned buffer; reader-visible before the ack; ONE tx at completion). A/B lever: on is the shipped arm (aligned overwrite ingest 31.0 → 32.1 GiB/s at half the daemon CPU, merge share 1.000 → 0.001); 0 is the accumulation control arm." },
    .{ .key = "SQUEEZEFS_OVERLAY_OVERWRITE", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "The overlay's OVERWRITE shape (a record over a mapped block captures the old binding in ONE meta-stripe section and FEEDS the rewrite epoch at completion). A/B lever: on is the shipped arm; 0 declines overwrites to accumulation — the venue split recorded: a device-bound local zram-tcp prefers 0 (1149–1161 vs 783–940 MiB/s at qd4)." },
    .{ .key = "SQUEEZEFS_OVERLAY_CLOSE_BARRIER", .kind = .bool, .default = "off", .class = .ab_lever, .doc = "A data barrier before a coverage-complete overlay close on a write-back namespace (the stronger posture for the OW-8 window: power loss after an unbarriered non-fsync save). A/B lever: off is the shipped arm (the inherited DUR-2 class is disclosed, not paid per block); 1 is the barrier-per-close bracket." },
    .{ .key = "SQUEEZEFS_OVERLAY_DEPTH_GOVERNOR", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "Overlay stores take the write-pipeline depth permit (ONE gauge, ONE target, ONE gate for both vehicles; the permit is taken after the registry and before the claim). A/B lever: on is the shipped arm; 0 is the ungoverned control arm (an overlay burst past the BDP target)." },
    .{ .key = "SQUEEZEFS_GAP_SEED_RANGED", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "The overlay settle seeds each uncovered gap of an overwrite by ONE ranged read of exactly its bytes (when Σ gaps < the block window) instead of one whole old-image read. A/B lever: on is the shipped arm; 0 is the whole-image control arm." },
    .{ .key = "SQUEEZEFS_ZC_ACK_EARLY", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "ACK-early: a page-cache-sound held zero-copy payload under COMMIT_RETAIN acks before its overlay DMA lands (the detached continuation retries until it does; the fence latch is the ONE legal drop). A/B lever: on is the shipped arm; 0 acks every store after its CQE." },
    .{ .key = "SQUEEZEFS_ZC_ACK_EARLY_ODIRECT", .kind = .bool, .default = "off", .class = .switch_, .doc = "Opt-in: ACK-early for the O_DIRECT/GUP class as a SNAPSHOT (the pages are not stable after the ack — the 2026-08-09 dd oflag=direct aliasing; NFS UNSTABLE semantics). Off = the O_DIRECT class acks after its CQE." },
    .{ .key = "SQUEEZEFS_REWRITE_SHADOW", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "The rewrite epoch: an overwrite's displaced keys park and ONE whole-tx save swaps them at close (full coverage / fsync / release / shape change / ENOSPC / idle TTI/10 / fence / unmount). A/B lever: on is the shipped arm (rewrite_amp 1.0000); 0 is the per-block durable-publish control arm (`rewrite_shadow_fallbacks`)." },
    .{ .key = "SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS", .kind = .{ .int = .{ .lo = 0, .hi = 1 << 20 } }, .default = "derived", .class = .ab_lever, .doc = "The write-pipeline depth: unset = the adaptive BDP governor (per-lane bw × latency floor × 3, probed up BBR-style, Red-clamped to the measured drain, capped budget/4). A/B lever: the governor is the shipped arm; 0 = synchronous inline uploads (the pre-campaign control arm); N = pinned at N blocks (the fixed-depth bracket: forced 64 measured 13.7 GB/s vs pure-BDP 11.6 vs raw 16.6)." },
    .{ .key = "SQUEEZEFS_FSYNC_TOUCHED_NAMESPACES", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "fsync barriers only the data namespaces the ino TOUCHED (the per-ino stripe word stamped at every layout publish and in-place DMA; write-through namespaces skip). A/B lever: on is the shipped arm; 0 barriers every volatile namespace (the pre-economy control arm)." },
    .{ .key = "SQUEEZEFS_FSYNC_PARALLEL_LEGS", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "fsync's independent data-barrier legs (≥ 2 namespaces) run joined and ALL awaited — the fsync pays the slowest leg, not the sum; never cancels a sibling. A/B lever: on is the shipped arm; 0 runs the legs sequentially." },

    // ---- the data plane: the block-map tree (read by data/kvmap.zig, data/plane.zig) ----
    .{ .key = "SQUEEZEFS_MAP_MIGRATE_CHUNK", .kind = .{ .int = .{ .lo = 64, .hi = 1024 } }, .default = "512", .class = .ab_lever, .doc = "Record ops per block-map-tree crossing / sweep transaction, verbatim; derived floor_pow2((128 KiB / 2) / (map record frame + ref record frame)) = 512 — half the whole-entry cap for the records, the head and inode Puts in the other half (charter §5.6 'chunked Puts (512)'). The sweep threshold is chunk × 64 blocks, so this is the one lever behind both. The range top keeps a mis-set value inside the 128 KiB whole-entry admission. A/B lever: 512 is the shipped arm; 64 is the many-small-txs bracket." },
    .{ .key = "SQUEEZEFS_TEST_KVMAP_CROSSING_KILL_AFTER", .kind = .{ .int = .{ .lo = 0, .hi = std.math.maxInt(u32) } }, .default = "0", .class = .test_seam, .doc = "Test seam: the daemon SIGKILLs itself after the N-th committed chunk of a block-map-tree crossing, before the flip (0 = never) — the mid-crossing crash the remount's ratchet must resume from." },

    // ---- the D0 single-writer guard: the per-mount NVMe host identity (read by meta/guard/pr.zig) ----
    .{ .key = "SQUEEZEFS_HOSTNQN", .kind = .str, .default = "-", .class = .config, .doc = "The NVMe host NQN this daemon's fabric associations present and whose registrations the PR primitive judges as its own; PAIR-OR-NEITHER with SQUEEZEFS_HOSTID (exactly one configured refuses the mount: a hostnqn under the host's default hostid is how registrants alias on the device). Absent = /etc/nvme/hostnqn, read-only — never minted or written by a mount (quirk Q7). `-o hostnqn=` wins over the env." },
    .{ .key = "SQUEEZEFS_HOSTID", .kind = .str, .default = "-", .class = .config, .doc = "The NVMe host identifier (uuid) paired with SQUEEZEFS_HOSTNQN; pair-or-neither. Absent = /etc/nvme/hostid, read-only. Re-read every heartbeat: a change under a live PR mount fail-stops the volume (writer_guard_identity_mismatch). `-o hostid=` wins over the env." },

    // ---- dynamic routing: the online migrate-meta-slot engine's seams (read by cli/volume.zig) ----
    .{ .key = "SQUEEZEFS_TEST_MIGRATE_HOLD", .kind = .{ .enumeration = .{ .allowed = &.{ "off", "after_bulk_copy", "gate_closed" } } }, .default = "off", .class = .test_seam, .doc = "Test seam: the migrate-meta-slot engine parks at the named phase (after the bulk copy; with the cutover gate closed) until released — the e2e CLI leg's hold; in-process tests pass the hold struct directly. `off` = no hold." },
    .{ .key = "SQUEEZEFS_TEST_MIGRATE_CRASH_AFTER_WRITE", .kind = .{ .int = .{ .lo = 0, .hi = 2 } }, .default = "derived", .class = .test_seam, .doc = "Test seam: abort the flip right after durable write N (0 = the target's checkpoint, 1 = the target's claim, 2 = the source's release) and exit without the clean close — the §5.5.2b crash matrix through the binary; the next mount resolves by per-slot highest-epoch-wins and converges the dual claim. Absent = no crash." },

    // ---- the observability surface (read by stats/stats.zig, fuse/handler/op_trace.zig) ----
    .{ .key = "SQUEEZEFS_STATS_KEY_CENSUS", .kind = .bool, .default = "0", .class = .switch_, .doc = "The `.stats` key census (VAL-7a): the stores' live key sets (read LRU keys, NVMe read-cache block keys, staged file ids, active writes by inode, the pack's open blocks) in the payload. Read LIVE on every `.stats` build — the ONE knob read after mount, because a build is cold and a live flip is exactly how an operator uses it; off, the keys are present and empty so a consumer can always key on them, and every *_count gauge exports regardless." },
    .{ .key = "SQUEEZEFS_OP_TRACE", .kind = .bool, .default = "0", .class = .switch_, .doc = "The sampled per-op trace ring (charter §10 `op_trace`): armed at mount, ONE ring per counter stripe with the derived geometry (rings = stripes; ring capacity = prev_pow2(max(budget/1024, rings × 1024 × 24) / rings / 24); divisor = the smallest N such that one second of the machine's op ceiling at the longest chain fits the pool), sampled by a deterministic per-op-id multiply so a traced op's chain is complete. `cat <mountpoint>/.trace` DRAINS it (`stat .trace` drains too — use `cat`); off, the hot path is one relaxed load and nothing is allocated." },

    // ---- the symmetric data planes: the free-grace ring and the allocation holder (read by data/sym_planes/*.zig) ----
    .{ .key = "SQUEEZEFS_FREE_GRACE_MAX_MS", .kind = .{ .int = .{ .lo = 1000, .hi = MS_MAX } }, .default = "derived", .class = .ab_lever, .doc = "The free-grace ring's routine fence bound, ms: how long a freed offset waits for every live reader's acknowledgement before the laggard is FENCED. Derived 2 × ack_cycle (ack_cycle = 3·beat + 3·staleness + skew + D_purge ≈ 38 s shipped, so ≈ 76 s: one missed cycle tolerated, mirroring S6's three attempts). An explicit value below ONE cycle REFUSES the mount (never clamped). A/B lever: derived is the shipped arm; one cycle is the aggressive-fence bracket." },
    .{ .key = "SQUEEZEFS_FREE_GRACE_MAX_OFFSETS", .kind = .{ .int = .{ .lo = 1, .hi = 1 << 32 } }, .default = "derived", .class = .ab_lever, .doc = "The free-grace ring's capacity, offsets (24 B each). Derived max(budget / 1024 / 24, 131,072): the measured saturated ingest of 12.7 GB/s over one ≈ 38 s acknowledgement cycle displaces ≈ 120 k blocks; < 0.1 % of the budget and NOT an R5 component (it cannot shed). Over cap the ring forces progress (fences the laggard) at once. A/B lever: derived is the shipped arm." },
    .{ .key = "SQUEEZEFS_FREE_GRACE_VALVE", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "The free-grace valve's rungs (a)+(b): the force deadline slides linearly from the routine bound to the pressure bound (one cycle) as the ring's runway — min(cap − held, free supply) × span / held — shrinks. A/B lever: 1 is the shipped arm; 0 holds the routine bound until it expires (the control arm that measures what the slide buys under a slow reader)." },
    .{ .key = "SQUEEZEFS_TEST_HOLDER_GRANT_EXHAUST_N", .kind = .{ .int = .{ .lo = 0, .hi = 1 << 20 } }, .default = "0", .class = .test_seam, .doc = "Test seam: the allocation holder answers the next N block-grant carves with the drain's park class (the ring-0 user window full ⇒ the typed Deferred), read once at the holder's arm, so a writer's deferred top-up, its retry and the bits-given-back law are exercised without filling a ring." },

    // ---- the cluster wire (read by wire/listener.zig) ----
    .{ .key = "SQUEEZEFS_CLUSTER_WIRE_MAX_CONNS", .kind = .{ .int = .{ .lo = 64, .hi = 65536 } }, .default = "derived", .class = .derived_override, .doc = "The ONE listener's connection cap, verbatim; derived clamp(raw_cpus × 16, 64, RLIMIT_NOFILE / 8) from the RAW affinity mask (never the fleet-share-divided one: a listener's load is the fleet's width × each member's session demand, which N co-located daemons each serve whole). An explicit value is railed by the fd ceiling only, the rail announced at start. Floor 64 = the shipped posture." },
    .{ .key = "SQUEEZEFS_MW_BIND", .kind = .str, .default = "-", .class = .config, .doc = "THE one listener's bind for every wire verb family (manager, membership, tokens, cross-owner, custody): `auto` (absent — bind an ephemeral port on every interface and advertise the default route's address), an explicit `addr:port` / `[v6]:port` advertised VERBATIM, or `off` — which the join ladder's rung 3 REFUSES on a writable mount ('a writer that cannot be SEEN cannot be EVICTED'); malformed refuses the mount." },

    // ---- symmetric metadata: the writer door, the join's ring ask, the grant and the rotor (read by meta/sym/bind.zig) ----
    .{ .key = "SQUEEZEFS_SYMMETRIC_META", .kind = .bool, .default = "1", .class = .switch_, .doc = "The symmetric plane's writer door (charter §5.5): `=0` on a volume whose superblock carries SYMMETRIC_PLANE REFUSES the writable open — 'once armed, stays armed' (`-o ro` is the read posture, never a knob value). Default 1: a stamped volume's writer is the D0 winner = the manager, a later writer joins." },
    .{ .key = "SQUEEZEFS_SYM_RING_KB", .kind = .{ .int = .{ .lo = 512, .hi = 32768 } }, .default = "derived", .class = .derived_override, .doc = "A joiner's ring ask, KiB, verbatim inside [512 KiB floor, the volume's ring ceiling clamp(volume/64, 8 MiB, 32 MiB)]; above the ceiling the open REFUSES naming it (never a clamp). Derived clamp(2 × ewma_commit_bytes/s × CHECKPOINT_MAX_AGE, floor, ceiling), or the identity's appender_hint after a storm." },
    .{ .key = "SQUEEZEFS_SYM_GRANT_EXTENTS", .kind = .{ .int = .{ .lo = 8, .hi = 65536 } }, .default = "derived", .class = .derived_override, .doc = "A region's extent-grant pool size, extents, verbatim; derived clamp(2 × ewma_smo_rate × failover_bound_s, 8, free_heap / (4 × appenders)) — floor 8 = 2 extents per SMO × 4 pending swaps per cycle, the cap a quarter of the free heap over the live appenders." },
    .{ .key = "SQUEEZEFS_SYM_MINT_SLOTS", .kind = .{ .int = .{ .lo = 1, .hi = 64 } }, .default = "derived", .class = .derived_override, .doc = "The rotor M a node leases at its join, verbatim; derived clamp(W / (2 × writers_known), 1, 64) — W = 65,536 slots, 64 = MINT_SPREAD. The join's pool floor is 8 + M (the rotor's first images + the SMO floor); the slot-lease arm leases M." },

    // ---- symmetric metadata: slot leases, the affinity ceiling, the handover window (read by meta/sym/slots/plane.zig) ----
    .{ .key = "SQUEEZEFS_SYM_AFFINITY_MAX_MB", .kind = .{ .int = .{ .lo = 1, .hi = MIB_MAX } }, .default = "derived", .class = .ab_lever, .doc = "The parent-affinity ceiling A_max, MiB, verbatim; derived DYNAMIC max(used_leaf_bytes(volume)/64, node_size) — no slot tree holds more than 1/64 of the volume's current leaf bytes, never less than one extent. A static value below node_size or above THIS volume's heap REFUSES the mount naming the bound (never a clamp). A/B lever: derived is the shipped divisibility arm; a large static ceiling is the gather-everything bracket." },
    .{ .key = "SQUEEZEFS_SYM_T_IDLE_MS", .kind = .{ .int = .{ .lo = 1000, .hi = MS_MAX } }, .default = "derived", .class = .ab_lever, .doc = "The handover's dominance window T_idle, ms, verbatim; derived T_owner = the membership lease's stale TTL (45,000 ms) — the slot lease lives as long as the membership lease. The window is two half-window buckets (the same window for the holder and every requester). A/B lever: derived is the shipped arm; 1,000 ms is the fast-handover bracket the in-process tests drive." },
    .{ .key = "SQUEEZEFS_DLM_RECALL_COOLDOWN_MS", .kind = .{ .int = .{ .lo = 1, .hi = MS_MAX } }, .default = "derived", .class = .ab_lever, .doc = "The requester-side never-thrash valve after a handover lands, ms, verbatim; derived 8 × T_idle (360 s shipped) — RECALL_COOLDOWN_WINDOWS = 8 bounds the worst-case residual thrash duty cycle at cycles/(cycles + 8) ≈ 27 %. A/B lever: derived is the shipped arm ('the valve itself has no knob')." },
    .{ .key = "SQUEEZEFS_TEST_HANDOVER_HOLD", .kind = .{ .enumeration = .{ .allowed = &.{ "off", "after_page", "after_tree0", "park_after_page" } } }, .default = "off", .class = .test_seam, .doc = "Test seam: the flush-then-transfer stops after a named durable step — `after_page` (the page names the slot Releasing, tree 0 still Leased: crash row 5), `after_tree0` (tree 0 Unleased, the page still names the slot: row 6) — or parks after the page (`park_after_page`: the door contract's window) until the harness releases it. Read once at the plane's open; default-inert." },

    // ---- GPFS-strict read tokens: the holder's seams (read ONCE at the plane's arm by meta/sym/tokens/token_table.zig; never process statics) ----
    .{ .key = "SQUEEZEFS_TEST_TOKEN_RECALL_DEADLINE_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "0", .class = .test_seam, .doc = "Test seam: the token recall deadline in ms instead of T_owner = 45 000 (0 = the constant), so the live-timeout contract (a recall unacked by a LIVE member past the TTL: `dlm_token_recall_timeouts_live`, the token retired, the commit proceeding) completes inside a test's budget. The product deadline is the membership lease TTL by derivation — never a p99, never a lever." },
    .{ .key = "SQUEEZEFS_TEST_TOKEN_CONVEYOR_HOLD_POST_RECALL", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: the holder's pass HOLDS after its A1½ recall completed, before it proceeds to ring admission, until the plane's hold is released — the exact window a first-touch grant on an object of the union must PARK in (register-then-check) and then serve the committed records." },
    .{ .key = "SQUEEZEFS_TEST_TOKEN_GRANT_HOLD_AFTER_CHECK", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: a grant HOLDS between its gate check and its record read until the plane's hold is released — the window in which a pass takes the object in flight DURING the read, so the re-park + re-read arm is exercised deterministically." },

    // ---- membership, the death ledger and recovery: the venue legs' seam (read ONCE at the shard's arm by meta/sym/membership/shard.zig; never a process static) ----
    .{ .key = "SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", .kind = .{ .int = .{ .lo = 1000, .hi = MS_MAX } }, .default = "derived", .class = .test_seam, .doc = "Test seam: the membership lease's T_owner in ms instead of the ONE staleness TTL (45,000 ms), so the two-daemon legs (kill -9 a joiner ⇒ the home shard evicts ⇒ RecordDeath ⇒ recovery ⇒ its slots Unleased; a dead manager ⇒ park-and-reclaim) complete inside a test's budget. Every derived clock (skew_max, T_self, renew, grace, the retirement at 2 × T_owner, the custody quarantine) follows it; absent = the constant." },
};

pub fn lookup(key: []const u8) ?*const Knob {
    for (&registry) |*k| if (std.mem.eql(u8, k.key, key)) return k;
    return null;
}

/// The row of a key known at compile time: an unregistered key is a compile
/// error, so a reader cannot exist without its row.
pub fn row(comptime key: []const u8) *const Knob {
    return comptime lookup(key) orelse @compileError(key ++ " is not a registry row (src/core/knobs.zig)");
}

/// Is this name in our namespace? (`SQUEEZEFS_` or `SQZ_` prefix.)
pub fn isOurs(key: []const u8) bool {
    return std.mem.startsWith(u8, key, "SQUEEZEFS_") or std.mem.startsWith(u8, key, "SQZ_");
}

// ---------------------------------------------------------------------------
// The ONE parser. Pure: no logging, no env reads, no syscalls, no side
// channel — a parse yields a VALUE (`Parsed(T)`), and the detail of a
// refusal travels inside it as a `Diagnostic`.
// ---------------------------------------------------------------------------

/// What an admissible value would have looked like; formats on demand.
pub const Want = union(enum) {
    boolean,
    integer,
    int_range: struct { lo: i128, hi: i128 },
    enum_set: []const []const u8,

    pub fn format(self: Want, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self) {
            .boolean => try w.writeAll(bool_want),
            .integer => try w.writeAll("expected an integer"),
            .int_range => |r| try w.print("expected an integer in {d}..={d}", .{ r.lo, r.hi }),
            .enum_set => |allowed| {
                try w.writeAll("expected one of ");
                for (allowed, 0..) |a, i| {
                    if (i != 0) try w.writeByte('|');
                    try w.writeAll(a);
                }
            },
        }
    }
};

/// The refusal detail: the key, the TRIMMED raw value, what was wanted.
pub const Diagnostic = struct {
    key: []const u8,
    raw: []const u8,
    want: Want,

    pub fn format(self: Diagnostic, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("{s}='{s}' is invalid: {f}", .{ self.key, self.raw, self.want });
    }
};

/// A parse outcome: absent, a value, or a refusal with its diagnostic.
pub fn Parsed(comptime T: type) type {
    return union(enum) {
        absent,
        value: T,
        invalid: Diagnostic,
    };
}

/// Absent = unset / empty / whitespace-only; otherwise the trimmed value.
pub fn present(raw: ?[]const u8) ?[]const u8 {
    const r = raw orelse return null;
    const t = std.mem.trim(u8, r, " \t\r\n");
    return if (t.len == 0) null else t;
}

pub const bool_true = [_][]const u8{ "1", "true", "yes", "on" };
pub const bool_false = [_][]const u8{ "0", "false", "no", "off" };
pub const bool_want = "expected a boolean — 1/true/yes/on or 0/false/no/off";

fn eqlIgnoreCase(a: []const u8, b: []const u8) bool {
    return std.ascii.eqlIgnoreCase(a, b);
}

pub fn parseBool(key: []const u8, raw: ?[]const u8) Parsed(bool) {
    const v = present(raw) orelse return .absent;
    for (bool_true) |t| if (eqlIgnoreCase(v, t)) return .{ .value = true };
    for (bool_false) |f| if (eqlIgnoreCase(v, f)) return .{ .value = false };
    return .{ .invalid = .{ .key = key, .raw = v, .want = .boolean } };
}

/// Inclusive range. Parse failure AND out-of-range share ONE message naming
/// the admissible range. Out-of-range REFUSES, never clamps.
pub fn parseIntIn(comptime T: type, key: []const u8, raw: ?[]const u8, lo: T, hi: T) Parsed(T) {
    const v = present(raw) orelse return .absent;
    const parsed: ?T = std.fmt.parseInt(T, v, 10) catch null;
    if (parsed) |p| if (p >= lo and p <= hi) return .{ .value = p };
    return .{ .invalid = .{ .key = key, .raw = v, .want = .{ .int_range = .{ .lo = lo, .hi = hi } } } };
}

/// Case-insensitive match returning the CANONICAL spelling from `allowed`.
pub fn parseEnum(key: []const u8, raw: ?[]const u8, allowed: []const []const u8) Parsed([]const u8) {
    const v = present(raw) orelse return .absent;
    for (allowed) |a| if (eqlIgnoreCase(v, a)) return .{ .value = a };
    return .{ .invalid = .{ .key = key, .raw = v, .want = .{ .enum_set = allowed } } };
}

pub fn parseStr(raw: ?[]const u8) ?[]const u8 {
    return present(raw);
}

// ---------------------------------------------------------------------------
// The startup gate.
// ---------------------------------------------------------------------------

pub const Validation = struct {
    /// One line per offender, sorted.
    errors: std.ArrayList([]u8) = .empty,
    /// Unregistered names in our namespace, sorted.
    unknown: std.ArrayList([]u8) = .empty,

    pub fn deinit(self: *Validation, gpa: std.mem.Allocator) void {
        for (self.errors.items) |e| gpa.free(e);
        for (self.unknown.items) |u| gpa.free(u);
        self.errors.deinit(gpa);
        self.unknown.deinit(gpa);
    }

    pub fn isClean(self: Validation) bool {
        return self.errors.items.len == 0 and self.unknown.items.len == 0;
    }
};

/// Validate ONE registered knob against its raw value. Returns the error line
/// (caller-owned) or null when acceptable.
pub fn validateOne(gpa: std.mem.Allocator, knob: *const Knob, raw: []const u8) !?[]u8 {
    switch (knob.kind) {
        .bool => switch (parseBool(knob.key, raw)) {
            .invalid => |d| return try std.fmt.allocPrint(gpa, "{f}", .{d}),
            .absent, .value => {},
        },
        .int => |r| switch (parseIntIn(i128, knob.key, raw, r.lo, r.hi)) {
            .invalid => |d| return try std.fmt.allocPrint(gpa, "{f}", .{d}),
            .absent, .value => {},
        },
        .enumeration => |e| {
            const v = present(raw) orelse return null;
            // A retired VALUE is checked BEFORE the allowed set; its message
            // must not read "expected one of" (it was valid once; it is not an
            // unknown spelling).
            for (e.retired) |rv| if (eqlIgnoreCase(v, rv.value)) {
                return try std.fmt.allocPrint(gpa, "{s}='{s}' was RETIRED (forward-only — never a silent alias): {s}", .{ knob.key, v, rv.why });
            };
            switch (parseEnum(knob.key, raw, e.allowed)) {
                .invalid => |d| return try std.fmt.allocPrint(gpa, "{f}", .{d}),
                .absent, .value => {},
            }
        },
        .str, .harness => {},
        .retired => |r| {
            const v = present(raw) orelse return null;
            return try std.fmt.allocPrint(gpa, "{s}='{s}' was RETIRED (forward-only — never a silent alias): use {s} instead", .{ knob.key, v, r.successor });
        },
    }
    return null;
}

/// Validate every `(key, value)` pair the iterator yields. Keys outside our
/// namespace are skipped; registered keys validate; unregistered ours land in
/// `unknown`. Both lists are sorted so the report is deterministic.
pub fn validateVars(gpa: std.mem.Allocator, env: *const std.process.Environ.Map) !Validation {
    var v: Validation = .{};
    errdefer v.deinit(gpa);
    var it = env.iterator();
    while (it.next()) |entry| {
        const key = entry.key_ptr.*;
        if (!isOurs(key)) continue;
        if (lookup(key)) |knob| {
            if (try validateOne(gpa, knob, entry.value_ptr.*)) |line| try v.errors.append(gpa, line);
        } else {
            try v.unknown.append(gpa, try gpa.dupe(u8, key));
        }
    }
    std.mem.sort([]u8, v.errors.items, {}, lessThanBytes);
    std.mem.sort([]u8, v.unknown.items, {}, lessThanBytes);
    return v;
}

fn lessThanBytes(_: void, a: []u8, b: []u8) bool {
    return std.mem.lessThan(u8, a, b);
}

/// The startup gate. Announces every unknown name on `warn` ("probable typo",
/// never refused) and returns the refusal report when any registered knob is
/// malformed — the caller prints `Error: {report}` and exits 1 BEFORE anything
/// is opened or mounted.
pub fn refusalReport(gpa: std.mem.Allocator, env: *const std.process.Environ.Map, warn: *std.Io.Writer) !?[]u8 {
    var v = try validateVars(gpa, env);
    defer v.deinit(gpa);
    for (v.unknown.items) |name| {
        try warn.print("Warning: {s} is not a SqueezeFS knob (typo? `src/core/knobs.zig` is the registry) — ignored\n", .{name});
    }
    if (v.errors.items.len == 0) return null;
    var out: std.Io.Writer.Allocating = .init(gpa);
    errdefer out.deinit();
    const w = &out.writer;
    try w.writeAll("refusing to start: invalid SqueezeFS environment knob(s)\n");
    for (v.errors.items) |e| try w.print("  - {s}\n", .{e});
    try w.writeAll("  (a malformed knob is never silently defaulted — fix or unset it; empty means unset)");
    return try out.toOwnedSlice();
}

// ---------------------------------------------------------------------------
// In-process readers. The key is comptime: the row is resolved at compile
// time (an unregistered key cannot compile), the row's range is enforced
// and the row's default is THE default — a call site carries none. They
// never panic and never refuse: the gate already did. A malformed value here
// (only reachable when the gate was bypassed — e.g. a knob set after
// startup) is logged and the default kept.
// ---------------------------------------------------------------------------

fn intRange(comptime key: []const u8, comptime T: type) struct { lo: T, hi: T } {
    const r = comptime row(key);
    comptime if (r.kind != .int) @compileError(key ++ " is not an integer row");
    return comptime .{ .lo = @intCast(r.kind.int.lo), .hi = @intCast(r.kind.int.hi) };
}

/// A Bool row's default (`"0"`/`"1"`/`"on"`/…), typed at compile time.
pub fn boolDefault(comptime key: []const u8) bool {
    const r = comptime row(key);
    comptime if (r.kind != .bool) @compileError(key ++ " is not a Bool row");
    return comptime switch (parseBool(key, r.default)) {
        .value => |v| v,
        else => @compileError(key ++ " has no literal boolean default"),
    };
}

/// An Int row's literal default, typed and range-checked at compile time; a
/// "derived" row has none (read it through `optIntKnob`).
pub fn intDefault(comptime key: []const u8, comptime T: type) T {
    const r = comptime row(key);
    const range = comptime intRange(key, T);
    return comptime switch (parseIntIn(T, key, r.default, range.lo, range.hi)) {
        .value => |v| v,
        else => @compileError(key ++ " has no literal integer default inside its range — derived rows are read with optIntKnob"),
    };
}

pub fn boolKnob(comptime key: []const u8, env: *const std.process.Environ.Map) bool {
    const default = comptime boolDefault(key);
    return switch (parseBool(key, env.get(key))) {
        .value => |v| v,
        .absent => default,
        .invalid => |d| blk: {
            log.err("{f} — keeping the default ({})", .{ d, default });
            break :blk default;
        },
    };
}

pub fn intKnob(comptime key: []const u8, comptime T: type, env: *const std.process.Environ.Map) T {
    const default = comptime intDefault(key, T);
    const range = comptime intRange(key, T);
    return switch (parseIntIn(T, key, env.get(key), range.lo, range.hi)) {
        .value => |v| v,
        .absent => default,
        .invalid => |d| blk: {
            log.err("{f} — keeping the default ({d})", .{ d, default });
            break :blk default;
        },
    };
}

/// A derived-default Int: null = absent (derive it); an out-of-range or
/// malformed value is logged and ignored (derive it), never clamped.
pub fn optIntKnob(comptime key: []const u8, comptime T: type, env: *const std.process.Environ.Map) ?T {
    const range = comptime intRange(key, T);
    return switch (parseIntIn(T, key, env.get(key), range.lo, range.hi)) {
        .value => |v| v,
        .absent => null,
        .invalid => |d| blk: {
            log.err("{f} — ignoring the override", .{d});
            break :blk null;
        },
    };
}

pub fn enumKnob(comptime key: []const u8, env: *const std.process.Environ.Map) []const u8 {
    const r = comptime row(key);
    comptime if (r.kind != .enumeration) @compileError(key ++ " is not an enumeration row");
    const allowed = r.kind.enumeration.allowed;
    const default = comptime switch (parseEnum(key, r.default, allowed)) {
        .value => |v| v,
        else => @compileError(key ++ " has no default among its allowed values"),
    };
    return switch (parseEnum(key, env.get(key), allowed)) {
        .value => |v| v,
        .absent => default,
        .invalid => |d| blk: {
            log.err("{f} — keeping the default ({s})", .{ d, default });
            break :blk default;
        },
    };
}

// ---------------------------------------------------------------------------
// Tests: the contract of the convention.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn wantOf(comptime T: type, parsed: Parsed(T)) ![]const u8 {
    return switch (parsed) {
        .invalid => |d| std.fmt.bufPrint(&want_buf, "{f}", .{d.want}),
        else => error.TestExpectedInvalid,
    };
}
var want_buf: [160]u8 = undefined;

fn rawOf(comptime T: type, parsed: Parsed(T)) ![]const u8 {
    return switch (parsed) {
        .invalid => |d| d.raw,
        else => error.TestExpectedInvalid,
    };
}

test "absent: unset, empty, whitespace-only are all absent; values are trimmed" {
    try testing.expectEqual(@as(?[]const u8, null), present(null));
    try testing.expectEqual(@as(?[]const u8, null), present(""));
    try testing.expectEqual(@as(?[]const u8, null), present("   "));
    try testing.expectEqual(@as(?[]const u8, null), present("\t\n"));
    try testing.expectEqualStrings("7", present(" 7 ").?);
    try testing.expectEqual(Parsed(bool).absent, parseBool("SQUEEZEFS_X", "  "));
    try testing.expectEqual(Parsed(i64).absent, parseIntIn(i64, "SQUEEZEFS_X", "", 0, 10));
}

test "booleans: all eight spellings, case-insensitive; =0 disables; junk refuses with its diagnostic" {
    for (bool_true) |t| try testing.expectEqual(Parsed(bool){ .value = true }, parseBool("SQUEEZEFS_X", t));
    for (bool_false) |f| try testing.expectEqual(Parsed(bool){ .value = false }, parseBool("SQUEEZEFS_X", f));
    try testing.expectEqual(Parsed(bool){ .value = true }, parseBool("SQUEEZEFS_X", "YES"));
    try testing.expectEqual(Parsed(bool){ .value = true }, parseBool("SQUEEZEFS_X", "On"));
    try testing.expectEqual(Parsed(bool){ .value = false }, parseBool("SQUEEZEFS_X", "FALSE"));
    const bad = parseBool("SQUEEZEFS_X", "yess");
    try testing.expectEqualStrings(bool_want, try wantOf(bool, bad));
    try testing.expectEqualStrings("yess", try rawOf(bool, bad));
}

test "integers: range inclusive; out-of-range refuses naming the range, never clamps" {
    try testing.expectEqual(Parsed(u32){ .value = 1 }, parseIntIn(u32, "SQUEEZEFS_X", "1", 1, 32));
    try testing.expectEqual(Parsed(u32){ .value = 32 }, parseIntIn(u32, "SQUEEZEFS_X", "32", 1, 32));
    try testing.expectEqualStrings("expected an integer in 1..=32", try wantOf(u32, parseIntIn(u32, "SQUEEZEFS_X", "64", 1, 32)));
    try testing.expect(parseIntIn(u32, "SQUEEZEFS_X", "0", 1, 32) == .invalid);
    try testing.expect(parseIntIn(u32, "SQUEEZEFS_X", "4 2", 1, 32) == .invalid);
    try testing.expect(parseIntIn(u32, "SQUEEZEFS_X", "1e6", 1, 32) == .invalid);
    try testing.expect(parseIntIn(u32, "SQUEEZEFS_X", "5s", 1, 600_000) == .invalid);
    try testing.expect(parseIntIn(u64, "SQUEEZEFS_X", "-1", 0, 10) == .invalid);
    // A percentage knob refuses a fraction and refuses > 100: never clamped, never parsed as a float.
    try testing.expect(parseIntIn(i128, "SQUEEZEFS_X", "12.5", 1, 100) == .invalid);
    try testing.expectEqualStrings("expected an integer in 1..=100", try wantOf(i128, parseIntIn(i128, "SQUEEZEFS_X", "250", 1, 100)));
}

test "enums: case-insensitive, canonical spelling returned; miss names the set" {
    const allowed = [_][]const u8{ "first-touch", "second-touch" };
    try testing.expectEqualStrings("second-touch", parseEnum("SQUEEZEFS_X", "SECOND-TOUCH", &allowed).value);
    try testing.expectEqualStrings("expected one of first-touch|second-touch", try wantOf([]const u8, parseEnum("SQUEEZEFS_X", "third", &allowed)));
}

test "Diagnostic formats as key='raw' is invalid: want — a value, no thread-local, two diagnostics coexist" {
    const a = Diagnostic{ .key = "SQUEEZEFS_X", .raw = "zz", .want = .{ .int_range = .{ .lo = 1, .hi = 4 } } };
    const b = Diagnostic{ .key = "SQUEEZEFS_N", .raw = "q", .want = .boolean };
    var buf: [256]u8 = undefined;
    try testing.expectEqualStrings("SQUEEZEFS_X='zz' is invalid: expected an integer in 1..=4", try std.fmt.bufPrint(&buf, "{f}", .{a}));
    try testing.expectEqualStrings("SQUEEZEFS_N='q' is invalid: " ++ bool_want, try std.fmt.bufPrint(&buf, "{f}", .{b}));
    // The first diagnostic is intact after the second was formatted.
    try testing.expectEqualStrings("SQUEEZEFS_X='zz' is invalid: expected an integer in 1..=4", try std.fmt.bufPrint(&buf, "{f}", .{a}));
}

test "registry hygiene: unique keys, non-empty default and doc, our namespace" {
    for (registry, 0..) |a, i| {
        try testing.expect(isOurs(a.key));
        try testing.expect(a.default.len > 0);
        try testing.expect(a.doc.len > 10);
        for (registry[i + 1 ..]) |b| try testing.expect(!std.mem.eql(u8, a.key, b.key));
    }
}

test "registry collision law: a live key that strictly prefixes another must be a Bool switch" {
    for (registry) |a| {
        switch (a.kind) {
            .retired, .harness => continue,
            else => {},
        }
        for (registry) |b| {
            if (a.key.len >= b.key.len) continue;
            if (!std.mem.startsWith(u8, b.key, a.key)) continue;
            switch (b.kind) {
                .retired, .harness => continue,
                else => {},
            }
            try testing.expect(a.kind == .bool);
        }
    }
}

test "gate: every offender at once, sorted; unknown announced not refused" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    try env.put("SQUEEZEFS_TEST_META_BUFFERED", "yess");
    try env.put("SQUEEZEFS_URING_FS_WORKERS", "65");
    try env.put("SQUEEZEFS_TRANSPORT_MEM_PCT", "250");
    try env.put("SQUEEZEFS_NOT_A_KNOB", "1");
    try env.put("PATH", "/usr/bin");
    try env.put("SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", "   ");

    var v = try validateVars(gpa, &env);
    defer v.deinit(gpa);
    try testing.expectEqual(@as(usize, 3), v.errors.items.len);
    try testing.expectEqualStrings("SQUEEZEFS_TEST_META_BUFFERED='yess' is invalid: " ++ bool_want, v.errors.items[0]);
    try testing.expectEqualStrings("SQUEEZEFS_TRANSPORT_MEM_PCT='250' is invalid: expected an integer in 1..=100", v.errors.items[1]);
    try testing.expectEqualStrings("SQUEEZEFS_URING_FS_WORKERS='65' is invalid: expected an integer in 1..=64", v.errors.items[2]);
    try testing.expectEqual(@as(usize, 1), v.unknown.items.len);
    try testing.expectEqualStrings("SQUEEZEFS_NOT_A_KNOB", v.unknown.items[0]);
    try testing.expect(!v.isClean());

    var warn_buf: [512]u8 = undefined;
    var warn: std.Io.Writer = .fixed(&warn_buf);
    const report = (try refusalReport(gpa, &env, &warn)).?;
    defer gpa.free(report);
    try testing.expect(std.mem.startsWith(u8, report, "refusing to start: invalid SqueezeFS environment knob(s)\n  - SQUEEZEFS_TEST_META_BUFFERED="));
    try testing.expect(std.mem.endsWith(u8, report, "(a malformed knob is never silently defaulted — fix or unset it; empty means unset)"));
    try testing.expect(std.mem.indexOf(u8, warn.buffered(), "Warning: SQUEEZEFS_NOT_A_KNOB is not a SqueezeFS knob (typo?") != null);
}

test "gate: a clean environment yields no report; an unknown alone is not a refusal" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    try env.put("SQUEEZEFS_TYPO_KNOB", "1");
    try env.put("SQUEEZEFS_TEST_META_BUFFERED", "on");
    var warn_buf: [256]u8 = undefined;
    var warn: std.Io.Writer = .fixed(&warn_buf);
    try testing.expectEqual(@as(?[]u8, null), try refusalReport(gpa, &env, &warn));
    try testing.expect(warn.buffered().len > 0);
}

test "validateOne: retired knob refuses only when present; retired enum value names why" {
    const gpa = testing.allocator;
    const retired_knob = Knob{ .key = "SQUEEZEFS_D", .kind = .{ .retired = .{ .successor = "SQUEEZEFS_E" } }, .default = "-", .class = .retired, .doc = "retired for the test" };
    try testing.expectEqual(@as(?[]u8, null), try validateOne(gpa, &retired_knob, ""));
    const line = (try validateOne(gpa, &retired_knob, "1")).?;
    defer gpa.free(line);
    try testing.expectEqualStrings("SQUEEZEFS_D='1' was RETIRED (forward-only — never a silent alias): use SQUEEZEFS_E instead", line);

    const allowed = [_][]const u8{ "a", "b" };
    const enum_knob = Knob{ .key = "SQUEEZEFS_N", .kind = .{ .enumeration = .{ .allowed = &allowed, .retired = &.{.{ .value = "legacy", .why = "the legacy arm was deleted" }} } }, .default = "a", .class = .enum_select, .doc = "enum for the test" };
    const l2 = (try validateOne(gpa, &enum_knob, "LEGACY")).?;
    defer gpa.free(l2);
    try testing.expect(std.mem.indexOf(u8, l2, "RETIRED") != null);
    try testing.expect(std.mem.indexOf(u8, l2, "expected one of") == null);
    try testing.expectEqual(@as(?[]u8, null), try validateOne(gpa, &enum_knob, "B"));
}

test "readers: the row is the default and the range; =0 disables a default-ON knob; bogus keeps the default loudly; out-of-range is ignored, never clamped" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    // The Bool row's default comes from the row ("0").
    try testing.expect(!boolKnob("SQUEEZEFS_TEST_META_BUFFERED", &env));
    for (bool_true) |t| {
        try env.put("SQUEEZEFS_TEST_META_BUFFERED", t);
        try testing.expect(boolKnob("SQUEEZEFS_TEST_META_BUFFERED", &env));
    }
    for (bool_false) |f| {
        try env.put("SQUEEZEFS_TEST_META_BUFFERED", f);
        try testing.expect(!boolKnob("SQUEEZEFS_TEST_META_BUFFERED", &env));
    }
    try env.put("SQUEEZEFS_TEST_META_BUFFERED", "bogus");
    log.testing.expectErrorLogs(1);
    try testing.expect(!boolKnob("SQUEEZEFS_TEST_META_BUFFERED", &env));
    try log.testing.expectAllFired();
    // A derived row: absent is null; the row's range (1..=64) is enforced.
    try testing.expectEqual(@as(?u32, null), optIntKnob("SQUEEZEFS_URING_FS_WORKERS", u32, &env));
    try env.put("SQUEEZEFS_URING_FS_WORKERS", "12");
    try testing.expectEqual(@as(?u32, 12), optIntKnob("SQUEEZEFS_URING_FS_WORKERS", u32, &env));
    try env.put("SQUEEZEFS_URING_FS_WORKERS", "65");
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(@as(?u32, null), optIntKnob("SQUEEZEFS_URING_FS_WORKERS", u32, &env));
    try log.testing.expectAllFired();
    // A literal-default row: the row's "1000" is THE default; out-of-range keeps it.
    try testing.expectEqual(@as(u64, 1000), intDefault("SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", u64));
    try testing.expectEqual(@as(u64, 1000), intKnob("SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", u64, &env));
    try env.put("SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", "500");
    try testing.expectEqual(@as(u64, 500), intKnob("SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", u64, &env));
    try env.put("SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", "0");
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(@as(u64, 1000), intKnob("SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", u64, &env));
    try log.testing.expectAllFired();
    try testing.expect(!boolDefault("SQUEEZEFS_TEST_META_BUFFERED"));
}

test "classes: every row is one of the charter's shapes; a seam is named SQUEEZEFS_TEST_ and default-inert; every A/B lever names its arm" {
    for (registry) |k| {
        const is_seam = std.mem.startsWith(u8, k.key, "SQUEEZEFS_TEST_");
        switch (k.class) {
            .test_seam => {
                try testing.expect(is_seam);
                // Default-inert: absent derives, or the off/zero arm.
                try testing.expect(std.mem.eql(u8, k.default, "derived") or std.mem.eql(u8, k.default, "0") or std.mem.eql(u8, k.default, "off"));
            },
            .ab_lever => try testing.expect(std.mem.indexOf(u8, k.doc, "A/B lever:") != null),
            .harness => try testing.expect(is_seam and k.kind == .harness),
            .derived_override => {
                try testing.expectEqualStrings("derived", k.default);
                try testing.expect(k.kind == .int);
            },
            .switch_, .config, .enum_select, .retired => {},
        }
    }
}
