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
const fuzzkit = @import("fuzzkit.zig");

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
    .{ .key = "SQUEEZEFS_META_APPLIERS", .kind = .{ .int = .{ .lo = 0, .hi = 64 } }, .default = "0", .class = .ab_lever, .doc = "The conveyor's applier venue (decision 0003 §AW): how many handler lanes take a window's RAM apply, encode and submit off the journal lane, each window on the lane its parent-side tree hashes to. Measured on mdtest -np 32 -u (rung-10-create-path): 4 reads file creation 1.48× and removal 1.55× against the serial pass, directory removal 1.33×, but directory creation 0.89× and a shared directory's creates 0.92× (the serial hot-leaf shapes pay the handoff and the appliers' lock handoffs); it ships off until the overlay's O(n) apply is gone (a regression is a defect). A/B lever: 0 is the control arm, shipped — the pass applies on the journal lane before it submits, as the reference ran it." },

    // ---- NVMe block device workers ----
    .{ .key = "SQUEEZEFS_NVME_READ_LANES", .kind = .{ .int = .{ .lo = 1, .hi = 1024 } }, .default = "derived", .class = .ab_lever, .doc = "Per-device READ submission fan-out lanes (derived cpus ÷ data devices). A/B lever: 1 is the single-submitter control arm (11.5 GB/s vs 16.7 GB/s at 4 lanes on the 8-namespace box)." },
    .{ .key = "SQUEEZEFS_NVME_WRITE_LANES", .kind = .{ .int = .{ .lo = 1, .hi = 1024 } }, .default = "derived", .class = .ab_lever, .doc = "Per-device DATA-WRITE submission fan-out lanes, block-offset affinity (derived cpus ÷ data devices). A/B lever: 1 is the single-submitter control arm." },

    // ---- R5 fleet share + IPC arena sizing (read by mem/) ----
    .{ .key = "SQUEEZEFS_TEST_FLEET_SHARE", .kind = .{ .int = .{ .lo = 1, .hi = 4096 } }, .default = "derived", .class = .test_seam, .doc = "Test seam: pin the fleet share — N co-located daemons sharing one machine — instead of the per-tick census of squeezefs daemons in this process's cgroup (`cgroup.procs` ∩ comm). The share divides the cgroup/RAM roots, the CPU root and the cgroup unreclaimable sample (ceil) BEFORE the ×0.8/×0.7 laws; explicit budgets and floors are never divided." },
    .{ .key = "SQUEEZEFS_TEST_MEM_SAMPLER_TICK_ON_STOP", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: the R5 sampler thread runs ONE more tick — every registered source read — when its stop latch fires, before it exits. The probe of the teardown order: a bring-up or umount that frees a component's source before stopping the sampler is caught reading it (the dangling read faults), while the correct order (stop + join first) reads live sources and exits clean." },
    .{ .key = "SQUEEZEFS_IPC_MEM_MAX", .kind = .{ .int = .{ .lo = 1, .hi = MIB_MAX } }, .default = "derived", .class = .derived_override, .doc = "ipc_session_arenas admission cap, MiB (absolute > pct > derived budget/8)." },
    .{ .key = "SQUEEZEFS_IPC_MEM_PCT", .kind = .{ .int = .{ .lo = 1, .hi = 100 } }, .default = "derived", .class = .derived_override, .doc = "ipc_session_arenas admission cap as a percentage of the R5 budget." },
    .{ .key = "SQUEEZEFS_IPC_ARENA_MB", .kind = .{ .int = .{ .lo = 1, .hi = 1 << 20 } }, .default = "derived", .class = .derived_override, .doc = "Per-session IPC payload arena, MiB (derived max(64 MiB, align_down(ipc cap / session population target, 4 MiB)), which yields on a bounded budget to the largest 4 MiB-aligned arena at which 2 × cpus sessions fit the ipc cap — never below 4 MiB; an arena under 64 MiB carries fewer slots of the shipped 64 KiB slab, never smaller slabs)." },

    // ---- the client bypass: the shm session (read by ipc/{sizing,doorbell,shm,rendezvous,client}.zig) ----
    .{ .key = "SQUEEZEFS_IPC_SERVICE_THREADS", .kind = .{ .int = .{ .lo = 1, .hi = 64 } }, .default = "derived", .class = .derived_override, .doc = "IL service threads = direct-drive shards (ONE function sizes both halves, charter §7), verbatim; derived clamp(ceil(3 × cpus / 8), 2, 64) — the counted W8→W24 width sweep (W12 +10.4 % best at 32 cpus; W24 the one regression past 1.0× the core budget). Spawned on session admission, never parked spares." },
    .{ .key = "SQUEEZEFS_IPC_MAX_OP_BYTES", .kind = .{ .int = .{ .lo = 4096, .hi = 1 << 30 } }, .default = "derived", .class = .derived_override, .doc = "The per-op payload ceiling of an IL session, bytes, verbatim (page multiple); derived = the transport's max_write desire (max(block_size, 1 MiB)) so both lanes share ONE per-op ceiling and the hybrid lane gate's rail is the same number." },
    .{ .key = "SQUEEZEFS_IPC_SOCKET_DIR", .kind = .str, .default = "-", .class = .config, .doc = "The IL rendezvous's filesystem-path socket directory (`none` disables the path rung; the abstract AF_UNIX listener always binds). Absent: /run/squeezefs for euid 0, else $XDG_RUNTIME_DIR/squeezefs, else /tmp/squeezefs-il-<uid>. A group- or world-writable directory, or one another uid owns, REFUSES the path rung loudly and the mount degrades to abstract-only — never fails." },
    .{ .key = "SQUEEZEFS_TEST_IPC_ALLOW_DIRTY", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: admit a degenerate build identity (`unknown`, `-dirty`, empty) on EITHER side of the IL HELLO (the same-commit law otherwise refuses it as Version); counted in ipc_binds_dev_override and announced once. Default-inert: a dev box's dirty tree is the only legitimate use." },
    .{ .key = "SQUEEZEFS_IPC_SPIN_US", .kind = .{ .int = .{ .lo = 0, .hi = 10_000 } }, .default = "derived", .class = .ab_lever, .doc = "IL service thread spin-before-park window, µs, STATIC and verbatim (governor off) — the doorbell park's and the ring park's alike; absent = the adaptive governor (100 µs in the churn regime, 0 otherwise; the ring park, with the lane's own CQEs in flight, spins twice its park EWMA inside a 50 µs RAM-class rail). A/B lever: 0 is the 2026-07-26 never-spin control arm (every nonzero ambient window stole runnable CPU from the sync lane: −4 % at 20 µs, −22 % at 100 µs with 8 threads); the governor is the shipped arm." },
    .{ .key = "SQUEEZEFS_IPC_SPIN_ADAPTIVE", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "The IL service thread's adaptive spin governor (engages the measured 100 µs window only in the churn regime: ≥ 2 sessions on the lane, a seeded park EWMA ≤ 200 µs, busy under 100 − lanes/cores %). A/B lever: 1 is the shipped arm (+6.7 % median at 32×8); 0 is the window-0 control arm." },
    .{ .key = "SQUEEZEFS_IPC_CQE_WAKE_LATCH", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "The completion doorbell's mark-valued collapse latch: a park era pays ≤ 1 FUTEX_WAKE (the k-th mark-passed completion; later ones collapse). A/B lever: 1 is the shipped arm (wake share ≤ 0.25 at 32×8); 0 is the wake-per-mark-passed-completion control arm." },
    .{ .key = "SQUEEZEFS_IPC_ARENA_THP", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "Transparent huge pages over the IL session's memfd (both sides map PMD-aligned; the daemon's deferred prep runs MADV_HUGEPAGE + MADV_POPULATE_WRITE + MADV_COLLAPSE, which works whatever shmem_enabled says; the client advises only). A/B lever: 1 is the shipped arm; 0 disables both halves (the 4 KiB-page control arm)." },
    .{ .key = "SQUEEZEFS_IL_SESSIONS", .kind = .{ .int = .{ .lo = 1, .hi = 16 } }, .default = "derived", .class = .derived_override, .doc = "IL sessions per (process, mount), verbatim; derived clamp(ceil(cpus/4), 2, 16) — the drain-thread saturation slope of the 2026-07-19 service sweep (the 2026-07-28 defaults mismatch: shim 4 vs daemon 8 threads left svc4..7 idle; 8 by hand recovered +44 %); floor 2 = the single-dequeue-thread plateau, ceiling 16 = the shim's static session table (32) over two bound mounts." },
    .{ .key = "SQUEEZEFS_IL_SPINS", .kind = .{ .int = .{ .lo = 0, .hi = 1 << 20 } }, .default = "derived", .class = .ab_lever, .doc = "The IL client's spin iterations before parking on a slot, PINNED verbatim for both regimes; derived = adaptive: 4096, or 256 once the session's last op parked (a parked op means daemon-side async work — the full window is CPU theft: 336 k → 605 k IOPS at 64 threads, 256 the measured best floor). A/B lever: the adaptive pair is the shipped arm; 0 parks at once, 4096 is the fixed pre-campaign arm." },
    .{ .key = "SQUEEZEFS_TEST_SUPERVISOR_PROBE_MS", .kind = .{ .int = .{ .lo = 0, .hi = 5_000 } }, .default = "0", .class = .test_seam, .doc = "Test seam: the mount supervisor's probe cadence in ms instead of PROBE_INTERVAL_NS's 5 s (0 = the constant), so a row awaiting the supervisor's next look (a lazy detach's switch to the heartbeat) drives it instead of waiting out three intervals; a probe's own answer window and the heartbeat's staleness stay the daemon watchdog tick's." },
    .{ .key = "SQUEEZEFS_TEST_IL_OP_TIMEOUT_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "0", .class = .test_seam, .doc = "Test seam: the IL client's per-op deadline in ms instead of the 30 s constant (0 = the constant), so the deadline-poison rung of the death ladder is reachable inside a test's budget." },
    .{ .key = "SQUEEZEFS_IL_KERNEL_LANE_MIN", .kind = .{ .int = .{ .lo = 0, .hi = BYTES_MAX } }, .default = "derived", .class = .derived_override, .doc = "The hybrid lane gate: an op strictly larger than this many bytes routes to kernel FUSE for throughput, verbatim (0 = off, every op rings); derived clamp(round_down(memBW × 2.3 µs / 2 copies, 4 KiB), slab, max_op) from a once-per-process memcpy probe (≈ 140–164 KiB on 125–147 GB/s field clients). Published as lane_gate_threshold_bytes." },
    .{ .key = "SQUEEZEFS_TEST_IL_MAX_RUN_SLOTS", .kind = .{ .int = .{ .lo = 0, .hi = 65_536 } }, .default = "0", .class = .test_seam, .doc = "Test seam: cap a large op's contiguous slot run at N slots (0 = uncapped), so the fragmented-run / smaller-flight paths are exercised deterministically." },

    // ---- the client bypass: the daemon host (read by ipc/host.zig, ipc/host/{direct,inval}.zig) ----
    .{ .key = "SQUEEZEFS_IPC_DD_SHARDS", .kind = .{ .int = .{ .lo = 1, .hi = 64 } }, .default = "derived", .class = .derived_override, .doc = "Direct-drive shards (one 512-entry io_uring + reaper per shard), verbatim; derived = the SAME function as the service-thread ceiling, clamp(ceil(3 × cpus / 8), 2, 64), so shards and service threads are 1:1 by construction (a wider shard set is production-dark; a narrower one shares lanes). The row's range IS the admissible range (decision 0002: the reference's registry said 1..1<<12 and clamped to 64 in code)." },
    .{ .key = "SQUEEZEFS_IPC_INVAL_WINDOW_MS", .kind = .{ .int = .{ .lo = 1, .hi = MS_MAX } }, .default = "1000", .class = .ab_lever, .doc = "KD-11's W1 limiter: one whole-inode FUSE_NOTIFY_INVAL_INODE per ring-written ino per window (attrs-only fires inside it when a write grows the file past the announced high-water). A/B lever: 1000 is the shipped arm (the kernel's own default attr TTL); a smaller window invalidates more, a larger one lets a buffered kernel reader stay stale longer." },
    .{ .key = "SQUEEZEFS_IPC_DD_COOP_TASKRUN", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "The direct-drive rings' IORING_SETUP_COOP_TASKRUN (task-work runs only when a task enters the ring — the reaper's bounded wait — instead of TWA_SIGNAL interrupting the service thread: the funnel profile's 9.5 % get_signal → task_work_run → mutex term). A/B lever: 1 is the shipped arm; 0 restores signal-delivered task-work." },
    .{ .key = "SQUEEZEFS_IPC_DD_LANE_FLUSH", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "Lane-scoped flush: a service thread enters ONLY its own direct-drive shard's ring at sweep end (counted A-B-B-A: +39.7 % at 32×8, +27.9 % at 32×32 vs the flush-all sweep, whose 12 threads racing 12 rings' uring_lock were 31.2 % of service cycles). A/B lever: 1 is the shipped arm; 0 leaves every published SQE to the shard's reaper on its 100 ms cadence." },
    .{ .key = "SQUEEZEFS_IPC_DD_INLINE_REAP", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "Reaper/drain fusion: after its flush the owning service thread try-locks its shard's CQ gate and drains its own completions inline (a userspace CQ peek, zero syscall; the dedicated reaper paid an io_uring_enter wake + a context switch per ~1.4 CQEs at the 32×8 ceiling). A/B lever: 1 is the shipped arm; 0 is the reaper-only control arm." },
    .{ .key = "SQUEEZEFS_IPC_DD_RING_PARK", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "The owner's RING PARK: a service thread with ops in flight on its direct-drive shard waits for their CQEs and its doorbells in ONE io_uring_enter (a FUTEX_WAITV SQE over the doorbell words + GETEVENTS, 5 ms bound) and drains the CQ itself on return, while the reaper backs off to a full-CQ/100 ms backstop — instead of a futex park that left every CQE to the reaper thread (a wake + a context switch per op, racing the owner for the CQ gate) or a userspace spin under which COOP_TASKRUN strands the owner's own CQEs. A/B lever: 1 is the shipped arm; 0 is the rung-7 posture (doorbell park, reaper per CQE)." },
    .{ .key = "SQUEEZEFS_IL_DIRECT", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "The client bypass's DIRECT ARM on the client side (charter §7 'The direct arm'): when the daemon granted it at HELLO (`-o interception=direct` and this uid can open every data device node), the shim/SDK reads the data device itself under a layout lease — one pread on the device from the caller's thread, the stripe's epoch rechecked with the bytes in hand — instead of ringing the daemon. A/B lever: 1 is the shipped arm; 0 keeps a granted session on the ring (the control arm)." },
    .{ .key = "SQUEEZEFS_IL_DIRECT_WRITE_ARM", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "The direct arm's WRITE half on the client side (decision 0003 §AY): when the daemon granted it at HELLO (every data device node writable by this uid), the shim/SDK opens the nodes O_RDWR and writes aligned, in-size, single-block ranges of a write-leased file itself — one pwrite on the device under the recall protocol, the postlude a batched note. A/B lever: 1 is the shipped arm; 0 keeps the session's writes on the ring (reads stay direct)." },
    .{ .key = "SQUEEZEFS_IL_DIRECT_WRITE", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "The direct-drive WRITE lane: a W1-shaped ring write (4 KiB-aligned, ≤ PATCH_MAX, single block, non-extending, not stream-adjacent, a sole-owned passthrough block) serves INLINE on the service thread's shard under a try-locked block stripe — device-true, no handler handoff (the handler lanes took 77.8 % of CPU on the engaged rand-4k row before it). A/B lever: 1 is the shipped arm; 0 routes every ring write through sever → handoff." },
    .{ .key = "SQUEEZEFS_IL_DIRECT_READ", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "The direct-drive READ lane: a ring read every tier missed whose shape the direct device leg admits (passthrough, 4 KiB-aligned, single block, cold, no write-side custody) DMAs from the device STRAIGHT INTO THE ARENA on the service thread's own shard — the FUSE worker's zero-copy direct leg without the kernel round trip and without the lane handoff (a fiber spawn, a lane wake and the full handler per cold 4 KiB read); the post-DMA recheck (incarnation ∧ binding) hands a moved block to the handler. A/B lever: 1 is the shipped arm; 0 routes every miss through the handler handoff (the rung-7 control arm)." },

    // ---- the client bypass: the LD_PRELOAD shim (read by shim/aio.zig; the shim never refuses its host over a value — it announces once and keeps the default, the charter's shim asymmetry) ----
    .{ .key = "SQUEEZEFS_IL_REAP_PARK_MAX", .kind = .{ .int = .{ .lo = 0, .hi = 4096 } }, .default = "24", .class = .ab_lever, .doc = "The libaio reap's deep/sparse regime boundary: more ring ops pending than this park on the batch mark k = clamp(pending/4, 2, pending); at or below it, 4 sweeps of ticket probes then a k = 1 park. A/B lever: 24 is the shipped arm (re-counted A-B-B-A: write 32×8 +14 % at 24 vs 2); 0 is the always-batch arm, 4096 the never-batch control." },
    .{ .key = "SQUEEZEFS_IL_REAP_QUANTUM_US", .kind = .{ .int = .{ .lo = 1, .hi = 1000 } }, .default = "50", .class = .ab_lever, .doc = "The libaio reap's park age bound in µs when a batch mark > 1 is armed (the mark may never be reached: the bound restores liveness). A/B lever: 50 is the shipped blind-sleep posture; 1000 is the lazy control arm (0 would busy-spin and is outside the range)." },
    .{ .key = "SQUEEZEFS_IL_SPARSE_BATCH_MARKS", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "The libaio reap's sparse regime parks with the derived batch mark k (clamp(pending/4, 2, pending)) instead of k = 1. A/B lever: 1 is the shipped arm; 0 is the flat event-park control arm (one wake per completion)." },

    // ---- the client bypass: the native SDK (read by sdk/session.zig) — @anchor:sdk_knobs ----
    .{ .key = "SQUEEZEFS_TEST_SDK_IL0_BLOB", .kind = .str, .default = "off", .class = .test_seam, .doc = "Test seam: a file holding the 312 B `user.squeezefs.il0` blob `sqz_session_open` reads INSTEAD of the mount's xattr (the statfs magic screen is skipped; the mount path is still the credential directory), so the SDK's unit tests and its C smoke establish against the in-process stub host with no daemon and no FUSE mount." },

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
    .{ .key = "SQUEEZEFS_TEST_DATA_DIR", .kind = .harness, .default = "/build/tmp", .class = .harness, .doc = "Harness: the directory the smoke puts its DATA volume files and cache dir in (the byte-heavy artifacts); the metadata volume, mountpoints and logs stay under /build/tmp. Point it at a capacity-class disk (the venue mounts an external volume at /build/ext) for large runs — a virtiofs-backed HFS+/exFAT disk has no sparse files and caches reads on the host, so crash-consistency rows stay on /build." },
    .{ .key = "SQUEEZEFS_TEST_RECOVERY_RACE_LOOPS", .kind = .harness, .default = "-", .class = .harness, .doc = "Harness: the looped recovery-under-maintenance stage (meta/sym/membership/volume_bind.zig) — a wire joiner with a multi-leaf slot tree dies and the manager recovers it while a racer compacts the recovered tree's leaves, this many passes, each checking every acked record, the budget and one custodian per extent; unset ⇒ the test skips. Run it with the unit filter UNDER THE MANAGER'S MAINTENANCE." },
    .{ .key = "SQUEEZEFS_TEST_SMOKE_STAGE_LOOPS", .kind = .harness, .default = "-", .class = .harness, .doc = "Harness: the smoke's looped single-stage repro — the cache-path 4 KiB overwrite stage and the 8-way storm stage run this many times each on ONE mount (distinct source pages per pass, `cmp` against the mirror, the must-stay-0 gauges after every pass); unset ⇒ the test skips. `scripts/venue \"SQUEEZEFS_TEST_SMOKE_STAGE_LOOPS=24 zig build --cache-dir /build/zig-cache --prefix zig-out-linux smoke -Dtest-filter=LOOPED\"`." },
    .{ .key = "SQUEEZEFS_TEST_FIDELITY_ROOT", .kind = .harness, .default = "/build/fideli", .class = .harness, .doc = "Harness: the nvmeof fidelity tier's root (backings, the relocated share ledger under <root>/state, mountpoints, logs, transcripts) — ext4 on the venue's cache volume for real block semantics and sparse files. Never read by product code." },

    // ---- the FUSE library: transport geometry (read by fuse/geometry.zig) ----
    .{ .key = "SQUEEZEFS_FUSE_MAX_WRITE", .kind = .{ .int = .{ .lo = 4096, .hi = std.math.maxInt(u32) } }, .default = "derived", .class = .derived_override, .doc = "INIT max_write DESIRE, bytes, verbatim (derived max(block_size, 1 MiB)); the plan still clamps it to the kernel's max_pages_limit sysctl and degrades it under the payload-buffer cap, so the published transport_max_write is what the kernel runs." },
    .{ .key = "SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH", .kind = .{ .int = .{ .lo = 1, .hi = 32 } }, .default = "derived", .class = .derived_override, .doc = "FUSE-over-io_uring per-queue ring depth, verbatim (derived clamp(cap / (nqueues × payload_sz), 4, 32): 32 the measured-best, 4 the shipped floor); explicit bypasses the budget ladder. Ceiling 32 = the kernel's per-queue entry population the measured-best posture was taken at." },
    .{ .key = "SQUEEZEFS_TEST_FUSE_REQUEST_TIMEOUT_S", .kind = .{ .int = .{ .lo = 15, .hi = std.math.maxInt(u16) } }, .default = "derived", .class = .test_seam, .doc = "Test seam: the INIT reply's FUSE request timeout, seconds, instead of the fail-stop lattice's T_owner + T_park_max (135 s; decision 0003 §Z), so the frozen-daemon row's host-wide sync returns inside a test's budget. Floor 15: the kernel's check period (FUSE_TIMEOUT_TIMER_FREQ), which it raises any shorter timeout to; ceiling: the u16 field." },
    .{ .key = "SQUEEZEFS_TEST_FUSE_QUEUES", .kind = .{ .int = .{ .lo = 1, .hi = 512 } }, .default = "derived", .class = .test_seam, .doc = "Test seam: FUSE-over-io_uring queue count instead of the kernel's possible CPUs. Fewer queues than possible CPUs never becomes ready (the kernel routes to the ring only once every queue holds an entry) — a product mount never sets it." },
    .{ .key = "SQUEEZEFS_FUSE_DRAIN_GROUP", .kind = .{ .int = .{ .lo = 1, .hi = 512 } }, .default = "derived", .class = .derived_override, .doc = "Drain-group width (member queues per sqz-fuse-ur worker), verbatim within a NUMA node — node containment is structural, an explicit width never spans a node (derived max(node_cpus / 4, 1); 1 under kmbuf/zc where registration is per ring per queue)." },
    .{ .key = "SQUEEZEFS_FUSE_RINGS_PER_QUEUE", .kind = .{ .int = .{ .lo = 1, .hi = 4 } }, .default = "derived", .class = .ab_lever, .doc = "io_urings — each a sqz-fuse-ur<qid> worker thread with its own ring and its share (ceil or floor of depth / rings) of the queue's entries — registered on EVERY kernel FUSE queue (read by fuse/transport/session.zig; rung-10-workers-per-queue). 3 and 4 are the A/B room of rung-10-fuse-boundary.md (a cold 4 KiB read costs a worker ≈ 6.4 µs, so two workers cap one submitter at ≈ 310 k reads/s; the 4k randread QD32×4 row sits at that cap while fio alone reaches 2–3 M against the boundary ruler). Derived 2 where the kernel's FEATURES probe reads FUSE_URING_FEAT_RINGS_PER_QUEUE (series patch 0030) on a kmbuf/zc session, else 1 (the default since 2026-10-19: its train reds pinned and the fsck C4 orphan fixed, rung-10-workers-per-queue.md; 1 is the A/B control arm): the kernel queues a request on the submitting CPU's queue and picks its entry LIFO, so a second ring's entries serve only once the first ring's are all in userspace — one submitter's QD32 spreads over two workers instead of saturating one (4k randread QD32×1 1.4–1.7×, QD32×4 1.1–1.4×, the §1.4 read bar cleared), and an idle second worker costs nothing; the open defect of the arm is 4k randwrite QD32×4 at 0.85–0.93× of one ring (a half-loaded worker parks per CQE: 2.5 enters per write against 1.2). An explicit 2 on a kernel whose probe reads absent FAILS THE MOUNT LOUD (the kernel would accept the second ring and recycle its buffers into the first's). A/B lever: 1 is the one-worker-per-queue control arm." },

    // ---- the FUSE library: kernel TTL classes (read by fuse/mount_options.zig) ----
    .{ .key = "SQUEEZEFS_FUSE_ATTR_TTL_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "1000", .class = .ab_lever, .doc = "Kernel attribute-cache TTL, ms (GETATTR/SETATTR replies and the attr half of every entry reply); `-o attr_timeout=` overrides it. A/B lever: 1000 is the shipped arm on a mount with a peer writer; an ISOLATED mount derives an unset class to T_owner (decision 0003 §AB.2: every change its kernel did not make is invalidated by name or object before its ack); 0 the no-cache control arm (every stat reaches the daemon)." },
    .{ .key = "SQUEEZEFS_FUSE_ENTRY_TTL_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "1000", .class = .ab_lever, .doc = "Kernel dentry TTL for non-directory entries, ms; `-o entry_timeout=` overrides it. A/B lever: 1000 is the shipped arm on a mount with a peer writer; an ISOLATED mount derives an unset class to T_owner (decision 0003 §AB.2); 0 the no-cache control arm (every path walk reaches the daemon)." },
    .{ .key = "SQUEEZEFS_FUSE_DIR_ENTRY_TTL_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "1000", .class = .ab_lever, .doc = "Kernel dentry TTL for directory entries, ms (a directory dentry's invalidation covers a subtree, hence its own class); `-o dir_entry_timeout=` overrides it. A/B lever: 1000 is the shipped arm on a mount with a peer writer; an ISOLATED mount derives an unset class to T_owner (decision 0003 §AB.2); 0 the no-cache control arm." },
    .{ .key = "SQUEEZEFS_FUSE_NEGATIVE_TTL_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "1000", .class = .ab_lever, .doc = "Kernel negative-dentry TTL, ms: a LOOKUP miss replies a cacheable negative entry (nodeid 0 + entry_valid) for this long; 0 restores the bare ENOENT. `-o negative_timeout=` overrides it. A/B lever: 1000 is the shipped arm on a mount with a peer writer; an ISOLATED mount derives an unset class to T_owner (decision 0003 §AB.2); 0 the bare-ENOENT control arm." },

    // ---- the executor (read by exec/placement.zig, exec/sqz_io.zig, exec/lane.zig) ----
    .{ .key = "SQUEEZEFS_FUSE_PIN_SCOPE", .kind = .{ .enumeration = .{ .allowed = &.{ "node", "core" } } }, .default = "node", .class = .enum_select, .doc = "Handler-lane and transport-worker affinity scope: `node` (the lane may run on any CPU of its home NUMA node — the serve-latency decomposition convicted the 1-CPU pin: queue_wait 1.55 + dispatch_lag 1.70 ms/op on cold reads, ~99 % collapse with widening) or `core` (the pre-campaign hard pin, the control arm of that measurement)." },
    .{ .key = "SQUEEZEFS_NUMA", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "NUMA placement actions (lane affinity by node, node-grouped dispatch). A/B lever: 0 makes no affinity call and ignores node groups while the node map and gauges stay live — the single-node-posture control arm; structurally inert on single-node hosts." },
    .{ .key = "SQUEEZEFS_FUSE_SAME_LANE_DISPATCH", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "A spawn issued from a lane thread lands on THAT lane instead of the fuse-lane rotation (deletes one cross-thread wake per op; the rotation measured 412 µs dispatch_lag at 256 in flight). A/B lever: 0 is the rotation control arm." },
    .{ .key = "SQUEEZEFS_TEST_EXEC_DROP_WAKE", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: every lane drops the FIRST cross-thread eventfd wake write after it starts, so the park tick's lost-wake rescue (exec_lane_tick_rescues) is exercised deterministically." },

    // ---- the FUSE handler layer (read by fuse/handler/{op_trace,op_guard}.zig) ----
    .{ .key = "SQUEEZEFS_OP_PROFILE", .kind = .bool, .default = "0", .class = .ab_lever, .doc = "Per-op phase histograms (fuse_op_phase_ns[class][handler_to_backend|backend|backend_to_reply|total], exact-sum) armed at mount: three extra CLOCK_MONOTONIC reads per op. A/B lever: 0 is the shipped arm — the prologue is ONE relaxed load of the armed word, no clock read, no allocation; the `.stats` surface of a disarmed mount is byte-identical." },
    .{ .key = "SQUEEZEFS_FUSE_NO_OPEN", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "Zero-message opens for regular files (decision 0003 §AF; read by cli/mount.zig): the daemon answers OPEN with ENOSYS, the kernel latches `no_open` for the connection and every open and close of a regular file is local (no OPEN, no RELEASE); the write-back drain that RELEASE carried rides the synchronous FLUSH of every close(2) (answered on the drain worker when the file holds no write custody, else drained on the data lane), INIT drops FUSE_ATOMIC_O_TRUNC so an O_TRUNC open is a SETATTR(size 0) under the size setter's guard, and custody no close drains (a mapping or an async write outliving its fd) drains once idle at the reclaim cadence. OPENDIR / RELEASEDIR stay. A/B lever: 0 is the control arm — OPEN and RELEASE per open, FOPEN_NOFLUSH, the drain at RELEASE." },
    .{ .key = "SQUEEZEFS_TEST_WRITE_POSTLUDE_HOLD", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam (read by cli/mount.zig): a striped write parks inside its postlude's `I{ino}` section (the 4a guard held, its stamp and size claim not yet parked) until the test releases it — the window the rangelock pins hold (a write to a disjoint range of the same file must not wait on it; a truncate and the times drain must)." },
    .{ .key = "SQUEEZEFS_TEST_PROMOTE_KILL_AFTER_COMMIT", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam (read by cli/mount.zig): the daemon SIGKILLs itself right after the write path's promotion of a staged file to its own block COMMITTED its striped layout and before it released the file's staging-ring record — the kill window THE KILL MATRIX's storm kill lands in under load: the ring keeps a record no layout names (fsck [C4] staging-custody orphan once its inode is destroyed)." },
    .{ .key = "SQUEEZEFS_TEST_FUSE_OWED_IDLE_MS", .kind = .{ .int = .{ .lo = 0, .hi = 600_000 } }, .default = "derived", .class = .test_seam, .doc = "Test seam (read by cli/mount.zig): the age in ms at which owed write custody drains — under the `close` write-back posture the idle horizon after which the reclaim cadence drains a no-open ino's owed custody (decision 0003 §AF; derived: the rewrite epoch's TTI/10 = 30 s; 0 = at the next reclaim tick), under `timer` the write-back timer's age bound (decision 0003 §AJ; derived: one flush cadence) — so a row holding custody no close covers observes the drain inside its budget, and a row that must see custody parked holds the timer away." },
    .{ .key = "SQUEEZEFS_WRITEBACK_POSTURE", .kind = .{ .enumeration = .{ .allowed = &.{ "timer", "close" } } }, .default = "timer", .class = .ab_lever, .doc = "Where an acknowledged write's custody (the accumulation table, the W2 overlays, the device overlay records, the rewrite epochs, the pipeline's uploads) becomes durable when no fsync, fdatasync, syncfs or unmount asks first (decision 0003 §AJ; read by cli/mount.zig): `timer` — close is no durability point (the daemon latches the kernel's `no_flush` with ONE FLUSH answered ENOSYS at ready, a CREATE answers FOPEN_NOFLUSH, RELEASE drains nothing: every close(2) is local) and a write-back timer on the flush cadence lands every custody once it is as old as the checkpoint's cadence ceiling (SQUEEZEFS_CHECKPOINT_MAX_AGE_MS), so an acked unsynced write leaves daemon memory within T_wb = age + period + the pass's drain — 1,050 ms + the drain shipped, the metadata leaves' landing ceiling (writeback_bound_ns, writeback_age_ns, writeback_residency_ns); A/B lever: `close` is the control arm — the drain at the close the kernel reports (the no-open FLUSH, coalesced per reclaim tick; the control open arm's RELEASE), idle custody at TTI/10." },
    .{ .key = "SQUEEZEFS_WRITE_SHARED", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "Shared-mode inode admission for cache-resident striped writes (charter §6.2: a WRITE whose range the resident caches prove within-EOF takes the per-inode op guard SHARED; readers run concurrently). A/B lever: 1 is the shipped arm (+7–8 % rand-4k IOPS, 523/530 k vs 489/491 k A-B-B-A); 0 is the exclusive control arm — every write classifies EntireOp/MetaPrepOnly and the 1.12 ms/op scheduler-wake convoy at 32 × qd32 returns." },

    // ---- the FUSE library: the ring transport (read by fuse/transport/session.zig) ----
    .{ .key = "SQUEEZEFS_FUSE_HANDLERS_ON_WORKER", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "Each FUSE drain worker DRIVES a handler lane of its own and every namespace or attribute request's handler runs on the worker's thread: no cross-thread handoff on delivery, a reply is a same-thread commit cell (decision 0003 §AB.3); a HELD zero-copy WRITE (the W1 hold gate's slot: its one DMA is the worker's own WRITE_FIXED bridge) runs there too, subject to SQUEEZEFS_FUSE_WRITE_OWN_DEPTH; the rest of the data class (READ, an extracted WRITE, the syncs, RELEASE, a truncate) stays on the CPU's fuse lane, its device waits park a thread. A/B lever: 0 is the control arm — every handler on the CPU's fuse lane thread, one handoff each way per request." },
    .{ .key = "SQUEEZEFS_FUSE_OWED_SPIN", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "A FUSE drain worker's pass with nothing in hand and a PAGE-SIZED device completion owed on its ring (a 4 KiB held write's store, a 4 KiB read's direct leg; a bigger DMA is a park's, and never sampled) SPINS for it instead of parking per CQE (read by fuse/transport/session.zig; worker.owedSpinBudget; rung-10-workers-per-queue 'the write row'). The budget is derived, never a constant: W = what a slept park's push → reap round trip adds over a spin-caught one (two RFC 6298 predictors per ring), the spin at most 2·W and only while the landing (born + the caught srtt) is predicted inside it, until one wake past the landing; no measurement ⇒ park, so a saturated worker (W → 0) and a slow device (an SSD's 80 µs against a 5 µs wake) spin never. Measured on the workstation (RAM NVMe, a 4 µs wake): neutral within ±5 % on every 4k row on one and two rings — the two-ring write row's 0.85× had been the host's thermal throttle, not the park — kept as the lever for a venue whose wake is dearer (a VM's). A/B lever: 0 is the park-per-CQE control arm." },
    .{ .key = "SQUEEZEFS_FUSE_WRITE_OWN_DEPTH", .kind = .{ .int = .{ .lo = 0, .hi = 1 << 16 } }, .default = "8", .class = .ab_lever, .doc = "How many handlers may be live on a QUEUE's workers' own lanes for a HELD WRITE to be dispatched there (read by fuse/transport/session.zig; rung-10-write-lane) — split ceil-wise over the queue's rings (SQUEEZEFS_FUSE_RINGS_PER_QUEUE: 8 is 4 per ring under two; worker.ownDepthShare); past it the write takes the CPU's fuse lane (transport_write_spills), the two-thread arm. One io_uring submitter's whole queue depth lands on ONE FUSE queue (the kernel picks it by the submitter's CPU), so with every held write on the worker that queue's worker is its one saturated CPU: the measured knee on the workstation is 8 — depth 0 (unbounded) halves the daemon CPU per 4 KiB write (19 → 10 µs; four submitters 1.15×) but reads 0.76–0.78× at QD32 from one thread in two series; depth 8 reads QD1 2.08×, QD32×1 0.99×, four submitters 1.06× at 0.95× the cores; depth 16 spills 44–48 % at QD32 (one leg: QD32×1 219 k at 1.54 cores, QD32×4 371 k at 6.07). The ruling (2026-10-19): a regression is a defect, so 8 ships. Lever 5 (two rings per kernel queue, series patch 0030 + SQUEEZEFS_FUSE_RINGS_PER_QUEUE, decision 0003 §AB.6) removed the one-queue saturation and depth 0 STILL reads 0.83× on that row with both workers at 0.83 cores (rung-10-workers-per-queue): an all-own write crosses the worker's pass boundary twice and a half-idle worker wakes per CQE (enters per write 1.2 → 2.5), so the bound is the pass structure, not the CPU — 8 stays until that lever lands. A/B lever: 0 is the all-own arm." },
    .{ .key = "SQUEEZEFS_FUSE_STREAM_LANES", .kind = .{ .enumeration = .{ .allowed = &.{ "cpu", "node", "node_only" } } }, .default = "cpu", .class = .ab_lever, .doc = "Where a HELD write of the overlay class (> block/8: the streaming write, charter §6.2 item 1) runs when it leaves its drain worker (read by fuse/transport/session.zig; worker.Group.dispatch; rung-10-seq-parity). `cpu`: the queue's CPU lane like every other data-class request — the shipped arm; `node`: round-robin over the home node's handler lanes (transport_stream_spreads); `node_only`: the same and never the worker's own lane (SQUEEZEFS_FUSE_WRITE_OWN_DEPTH does not apply to the class). The hypothesis: one io_uring submitter's whole depth lands on ONE FUSE queue, whose spilled writes all went to ONE lane — 16 × ~70 µs of handler per round against a ≈ 0.5 ms device phase. Measured on the workstation (1M seq write ×4, RAM NVMe, one session per arm): cpu 49.1 GB/s at 6.0 daemon cores, node 47.7 at 6.3 (+0.4 cores of sqz-fuse-ln: the spread handlers' cross-CPU misses, no shorter round), node_only 46.7 at 7.4 (every write two hops each way; the device phase, push → CQE ≈ 490 µs, did not move with the worker freed of handler work) — the lane was not the serialization. A/B lever: `node` and `node_only` are the measured arms." },
    .{ .key = "SQUEEZEFS_FUSE_READ_FUSION", .kind = .bool, .default = "0", .class = .ab_lever, .doc = "READ dispatch law arm (b): on a zero-copy session a READ at or under the fusion ceiling (payload/8, floor 4096) is polled on the worker's own fused lane instead of a handler lane. A/B lever: 0 is the shipped arm — composed behind the inline warm-ladder serve the fusion read −6.5 % IOPS (481k vs 514k), p50 +70 µs, p99 1.5 vs 4.3 ms: the two levers converge on the reap thread's per-op serialization; standing alone it won +15.5 %. Inert on a UserEnts (stock) session." },
    .{ .key = "SQUEEZEFS_FUSE_SQPOLL", .kind = .bool, .default = "0", .class = .ab_lever, .doc = "Build each transport worker's ring with IORING_SETUP_SQPOLL (a kernel submission poller per ring; never composed with DEFER_TASKRUN, which the poller's task-work ownership forbids). A/B lever: 0 is the shipped arm — SQPOLL was measured, not recommended (M10): the worker already carries every SQE in the ONE loop-bottom enter it needs for the wait, so the poller buys a core and saves no syscall on the saturated path." },
    .{ .key = "SQUEEZEFS_TEST_FUSE_REGISTER_REFUSE", .kind = .{ .int = .{ .lo = 0, .hi = 1 << 20 } }, .default = "0", .class = .test_seam, .doc = "Test seam: the first N REGISTER pushes across the session's workers are answered as refused (EBUSY) without reaching the kernel, so the backoff ladder (1 ms doubling to 128 ms, retirement on the 8th) and the arm barrier's submission count are exercised deterministically." },
    .{ .key = "SQUEEZEFS_TEST_FUSE_FAIL_AFTER_INIT_REPLY", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: the transport's bring-up fails (`InjectedStartupFailure`) right after the INIT reply and the filesystem's init, before any worker starts — the kernel created the connection's ring and no queue ever registers, so every request (a host-wide sync's FUSE_SYNCFS included) waits in fuse_get_req, outside the request timeout's reach (decision 0003 §AN)." },
    .{ .key = "SQUEEZEFS_TEST_FUSE_STARTUP_FAIL_HOLD", .kind = .str, .default = "off", .class = .test_seam, .doc = "Test seam: a FIFO path. A bring-up failure after mount(2) holds at the head of its failure path — before the fail-stop ends the connection and before any teardown step — until a writer opens the FIFO, so a row can park a host-wide sync on the connection first (decision 0003 §AN)." },
    .{ .key = "SQUEEZEFS_TEST_READ_ZERO_FILL", .kind = .{ .enumeration = .{ .allowed = &.{ "off", "prefilled", "bounce" } } }, .default = "off", .class = .test_seam, .doc = "Test seam (read by fuse/transport/session.zig): the FUSE boundary RULER — every O_DIRECT ring READ is answered at its delivery CQE on the worker with no filesystem (a buffered READ keeps the law: the harness reads `.stats` through the same path, and the ruler's row is O_DIRECT): `prefilled` commits a header claiming the requested length and moves no bytes (the app's pages keep their content: the FUSE-over-io_uring round trip alone), `bounce` zero-fills the slot's bounce window and bridges it into the app's pages (the round trip plus one memory-source READ_FIXED). What any FUSE filesystem on this kernel could serve at most (docs/evidence/rung-10-fuse-boundary.md); the reader gets zeros or stale bytes, so never on a mount anyone reads for content. `off` = the READ dispatch law." },

    // ---- the FUSE library: the buffer-mode lattice (read by fuse/transport/{session,probes}.zig) ----
    .{ .key = "SQUEEZEFS_FUSE_BUFFER_MODE", .kind = .{ .enumeration = .{ .allowed = &.{ "auto", "user", "bufring", "zc" } } }, .default = "auto", .class = .enum_select, .doc = "The buffer-mode lattice rung (charter §4): `auto` takes the highest rung the PROBES confirm (ZeroCopy on a full sqz kernel with CAP_SYS_ADMIN, BufRing with kmbuf alone, UserEnts on a stock kernel); `user` / `bufring` / `zc` pin a rung for an A/B measurement. An explicit rung the probes refuse FAILS THE MOUNT LOUD naming the probe — the probe is the only degrade point, never a silent downgrade (charter §3)." },
    .{ .key = "SQUEEZEFS_READ_ZC_SERVE", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "On a zero-copy session the filesystem's whole-block fill pool is ONE MAP_SHARED memfd slab and a warm READ serve bridges READ_FIXED(pool fd @ off → slot) with zero daemon passes (`replyFdBody`); the session tells the filesystem through InitCaps.read_zc_serve. A/B lever: 1 is the shipped arm (measured daemon CPU/GiB −24 % cold / −25 % warm, passes/byte 2.0 → 1.0 warm); 0 is the control arm — the serve is one copy into the bounce window, bridged from there." },
    .{ .key = "SQUEEZEFS_TEST_KMBUF_ABSENT", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: the kmbuf ladder reads absent — a stock distro kernel's stratum (no BufRing, so no zero-copy and no retention: `auto` mounts UserEnts, an explicit `bufring`/`zc` is refused loud) — judged on a venue kernel that has kmbuf." },
    .{ .key = "SQUEEZEFS_TEST_FUSE_CAP_SYS_ADMIN_ABSENT", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: the zero-copy probe reads CapEff as lacking CAP_SYS_ADMIN, so the lattice's refusal arms (explicit `zc` fails the mount loud; `auto` settles on BufRing) are exercised on a venue that holds the capability." },
    .{ .key = "SQUEEZEFS_TEST_KMBUF_REGISTER_ENOMEM", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: every kmbuf buffer-ring registration answers ENOMEM without reaching the kernel (the region's free pages short — the kernel's bulk order-0 fallback does not reclaim — or its RLIMIT_MEMLOCK charge), so the bounded retry ladder, the typed `KmbufRingEnomem` refusal naming its levers, and the daemon's late bring-up teardown are exercised deterministically on a kernel whose probe confirmed kmbuf." },
    .{ .key = "SQUEEZEFS_TEST_ZC_BRIDGE_CEILING", .kind = .{ .int = .{ .lo = 0, .hi = 1 << 30 } }, .default = "0", .class = .test_seam, .doc = "Test seam: every bridge fd's NOWAIT ceiling is this many bytes (read by fuse/transport/session.zig into worker.NowaitCeilings.force) instead of the probed sysfs facts — the pieces path over a memfd stand-in; 0 = probe." },
    .{ .key = "SQUEEZEFS_TEST_ZC_BRIDGE_STALL", .kind = .{ .int = .{ .lo = 0, .hi = 1 << 20 } }, .default = "0", .class = .test_seam, .doc = "Test seam: the next N reply bridges target a pipe with no writer instead of their source fd, so the bridge never completes and the 30 s deadline ladder (AsyncCancel once, the cancel-CQE table, the header-only -EIO commit) is exercised on a real ring." },
    .{ .key = "SQUEEZEFS_TEST_ZC_BRIDGE_DEADLINE_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "0", .class = .test_seam, .doc = "Test seam: the bridge deadline in ms instead of the 30 s constant (0 = the constant), so the stall seam resolves within a test's budget." },

    // ---- the data plane: layouts, packing, staging (read by data/{layout,pack,staging}.zig) ----
    .{ .key = "SQUEEZEFS_INLINE_MAX_BYTES", .kind = .{ .int = .{ .lo = 4096, .hi = 65_536 - 4096 } }, .default = "derived", .class = .ab_lever, .doc = "The inline layout ceiling, bytes, verbatim up to THIS set's format bound min(64 KiB, node_size/4) − 4 KiB (an override above the bound REFUSES the mount naming the bound — never clamped); derived = one page. A/B lever: the page is the shipped arm — 16 KiB measured −34 % files/s, 2.0× fsync µs, 59× journal bytes/file; 32 KiB −68 %, 117× and a fail-stopped 1 GiB metadata volume. Published as inline_max_bytes." },
    .{ .key = "SQUEEZEFS_SMALL_FILE_PACKING", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "GPFS-style small-file packing: a promoted staged image whose 4 KiB slot is ≤ pack_max_slot lands as a tenant of the open pack block instead of its own 4 MiB block. A/B lever: on is the shipped arm (96,000 fsync-promoted 16 KiB files = 375 blocks instead of 96,000; dismount 20,000 → 148; files/s +0.5 %); 0 is the one-block-per-file control arm (the read funnel is lever-independent)." },
    .{ .key = "SQUEEZEFS_PACK_MAX_SLOT_BYTES", .kind = .{ .int = .{ .lo = 4096, .hi = 4 << 20 } }, .default = "derived", .class = .ab_lever, .doc = "The own-block threshold, bytes: a slot above it takes its own block, at or below it packs; also the compaction victim line (a pack block whose live bytes are ≤ it is a victim). Derived CHUNK/2 — the break-even: an own block wastes CHUNK − s forever, a packed tenant costs one compaction copy of s, so packing wins iff CHUNK − s ≥ s. A/B lever: CHUNK/2 is the shipped arm of the two-faced law; a lower line is the more-own-blocks control arm. Published as pack_max_slot_bytes." },
    .{ .key = "SQUEEZEFS_FSYNC_PROMOTE_STAGED", .kind = .bool, .default = "off", .class = .ab_lever, .doc = "The fsync law's alternative posture for a staged-layout file: fsync PROMOTES it (the bytes leave the staging ring for the data volume — cross-client visibility at fsync instead of at the writer's clean unmount; placed before the data barrier so the barrier covers the block) instead of syncing its ring record in place. A/B lever: off is the shipped arm — the record stays staged and ONE ranged sync of the segment file, coalesced across concurrent fsyncs, makes it power-cut durable (charter §1 item 10, 2026-10-19); on measured −16.7 % files/s (338 → 425 µs, p99.9 +122 %) on the fsync-promote row; a promotion failure never fails the fsync (the record stays resident and is synced)." },
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

    // ---- fsck (read by ops/fsck/runner.zig) — @anchor:ops_fsck_knobs ----
    .{ .key = "SQUEEZEFS_TEST_FSCK_INJECT", .kind = .{ .enumeration = .{ .allowed = &.{ "off", "stray-ref", "orphan-inode" } } }, .default = "off", .class = .test_seam, .doc = "Test seam: `fsck --offline` plants ONE defect under its D0-guarded open BEFORE its census — `stray-ref` a durable block-reference record no layout justifies (C8, released by `--repair --apply`), `orphan-inode` the root's first regular file's dentry dropped with its record kept (C9, destroyed by `--repair --apply`) — the e2e's red → finding → repair → clean row over a real set. `off` = no injection." },
    .{ .key = "SQUEEZEFS_FSCK_SETTLE_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "2000", .class = .ab_lever, .doc = "The online fsck's settle window, ms: the §5.6 step-2 wait between the nominations and the re-check (a write in flight at the census publishes inside it; the allocation epoch is bumped after it so every later mint reads younger than the scan). A/B lever: 2000 is the shipped arm; 0 is the no-settle crucible (the epoch map, the in-flight registry and the fresh re-reads must clear every transient on their own)." },
    .{ .key = "SQUEEZEFS_TEST_RECLAIM_STALL_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "0", .class = .test_seam, .doc = "Test seam: stall each reclaim batch by this many ms before its first device command (deterministic slow-device schedules for the valve-liveness and room-edge contracts)." },
    .{ .key = "SQUEEZEFS_TEST_SHIP_EVERY_FREE", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: a NON-holder ships EVERY release of a block to the allocation holder, the nonterminal ones included (the reference's shape, 0002 row 500) — the holder cannot read the references a live lessee's slot trees carry and frees the block under its live tenants. The fleet's merged block census must REPORT such a block; the shipped default (off) releases locally while the writer's own count covers another reference." },
    .{ .key = "SQUEEZEFS_TEST_CORPSE_SWEEP_FAIL_DESTROY", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: the corpse sweep's destroy rider refuses every chunk after the releases were planned (the released-but-undestroyed state the release witness must make harmless: a retry is a leak-free no-op)." },
    .{ .key = "SQUEEZEFS_TEST_DESTROY_CHUNK_STOP_AFTER", .kind = .{ .int = .{ .lo = 0, .hi = std.math.maxInt(u32) } }, .default = "0", .class = .test_seam, .doc = "Test seam: stop a chunked single-ino destroy after N committed entries (the crash-between-chunks state the next sweep must converge on)." },

    // ---- the data plane: transforms and the volume-set config (read by data/transform.zig, cli/format.zig, cli/mount.zig) ----
    .{ .key = "SQUEEZEFS_ENCRYPT_KEY_FILE", .kind = .str, .default = "-", .class = .config, .doc = "Key resolution rung 2 (charter §6.5): the path of the operator key FILE (≥ 32 B of material, mode 0600, owned by the invoking user or root, never a symlink) when `--encrypt-key` was not given; rung 3 is /etc/squeezefs/keys/<key_id>.key. The file's derived key id must equal the set's recorded one or the mount refuses loud." },
    .{ .key = "SQUEEZEFS_TEST_TRANSFORM_FORCE_RAW", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: every transformed block takes the store-raw arm regardless of compressibility, so the raw flag, the chunk headroom and the widest image are exercised with compressible data." },

    // ---- the data plane: the read path (read by data/{read_path,hot_tier,admission,prefetch,warm_ladder}.zig) ----
    .{ .key = "SQUEEZEFS_READ_TIER_ADMISSION", .kind = .{ .enumeration = .{ .allowed = &.{ "always", "second-touch", "never" } } }, .default = "second-touch", .class = .enum_select, .doc = "R1b scan-resistant admission for fills > 256 KiB: `second-touch` publishes to the NVMe read tier only on a ghost-table second miss (first touch ⇒ hot-tier probation, publish skipped — the 16.9 GiB-per-16 GiB tier-write tax deleted); `always` is the pre-R1b control arm (every fill publishes); `never` disables the disk tier's publishes. A hot budget of 0 degrades second-touch to always loudly." },
    .{ .key = "SQUEEZEFS_READ_MEM_CACHE_MB", .kind = .{ .int = .{ .lo = 1, .hi = MIB_MAX } }, .default = "derived", .class = .derived_override, .doc = "The read RAM cache ceiling (`--read-mem-cache-size`), MiB, verbatim: the ≤ 256 KiB read LRU's budget and the base of the hot-tier default (derived min(RAM / 10, budget / 8))." },
    .{ .key = "SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", .kind = .{ .int = .{ .lo = 0, .hi = MIB_MAX } }, .default = "derived", .class = .ab_lever, .doc = "R4 hot-block tier budget, MiB, verbatim; derived max(2 × block, read_mem / 4) (2 blocks = one stream's consume-behind window). A/B lever: the derivation is the shipped arm; 0 is the tier-off control arm (every whole-block serve is a hold/NVMe/device serve; admission degrades to `always`)." },
    .{ .key = "SQUEEZEFS_READ_DISK_CACHE_MB", .kind = .{ .int = .{ .lo = 1, .hi = MIB_MAX } }, .default = "derived", .class = .derived_override, .doc = "The NVMe read cache budget (`--read-disk-cache-size`), MiB, verbatim, split evenly per cache device; derived aggregate cache capacity / 4." },
    .{ .key = "SQUEEZEFS_READ_ADMISSION_FILL_PCT", .kind = .{ .int = .{ .lo = 0, .hi = 100 } }, .default = "5", .class = .ab_lever, .doc = "The admission governor's bounded-waste share: under the clamp (wasted ≥ 2 blocks ∧ 2 × wasted ≥ evicted over the previous 2 s epoch) ghost escalations and stream admissions may spend this percentage of the previous epoch's foreground device bytes, by one token CAS each. A/B lever: 5 is the shipped arm (`wasted ÷ device reads ≤ 5 %` under churn); 100 is the unclamped control arm; 0 denies every clamped admission." },
    .{ .key = "SQUEEZEFS_READ_PREFETCH_WINDOW", .kind = .{ .int = .{ .lo = 0, .hi = 1 << 20 } }, .default = "derived", .class = .ab_lever, .doc = "R2 prefetch window CAP per lane, blocks, verbatim up to 4096 (an override above it applies as 4096); derived clamp(share% × hot budget / block, 4, 4096). A/B lever: the derivation is the shipped arm; 0 is the kill switch (no speculative issue — the demand-only control arm)." },
    .{ .key = "SQUEEZEFS_READ_PREFETCH_SHARE_PCT", .kind = .{ .int = .{ .lo = 1, .hi = 100 } }, .default = "50", .class = .ab_lever, .doc = "The prefetch pipeline's share of the hot-tier budget (the resident-share denominator per active stream). A/B lever: 50 is the shipped arm; 100 the whole-tier arm, 1 the one-block arm." },
    .{ .key = "SQUEEZEFS_READ_RANGED_THRESHOLD", .kind = .{ .int = .{ .lo = 0, .hi = BYTES_MAX } }, .default = "derived", .class = .ab_lever, .doc = "R3: a cold request at or under this many bytes on a passthrough volume whose block is larger, not stream-classified, every tier probe missed ⇒ a RANGED 4 KiB-outward DMA (never published, never single-flighted) instead of a whole-block fill. Derived: `block − 4 KiB` — EVERY strictly sub-block request (a request whose window is the whole block fetches it whole: byte-identical, and the fill buys tier residency); a random reader has proven no contiguity and only the ghost's second touch proves reuse, so a whole-block fetch for a 1 MiB random read was 4× device bytes for nothing (decision 0003 §K; the reference's 256 KiB ceiling was the read-LRU population bound borrowed as a DMA policy). A/B lever: 262144 is the reference's arm; 0 is the whole-block-only control arm." },
    .{ .key = "SQUEEZEFS_READ_DEST_LEASE", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "The dest lease: a ranged window that equals the request and lands 4 KiB-aligned DMAs straight into the registered reply window (zero daemon passes). A/B lever: 1 is the shipped arm (+28.6 % GB/s, daemon CPU/byte −50 %); 0 is the fill-into-bounce + serve-copy control arm." },
    .{ .key = "SQUEEZEFS_READ_LANE", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "The read lane: the hold (cohort-stability RAM store) + the probe-governed ahead issue. A/B lever: 1 is the shipped arm (qd8 +12.8 %, qd32 +14.5 %, read_amp 1.41 → 1.11); 0 is the A0 control arm — no hold, no ahead fetches, the R2 declines boundary at share == 0." },
    .{ .key = "SQUEEZEFS_READ_LANE_DEPTH", .kind = .{ .int = .{ .lo = 0, .hi = 1 << 16 } }, .default = "derived", .class = .ab_lever, .doc = "The ahead lane's depth, blocks, PINNED verbatim; derived = the closed-loop probe governor (both open-loop derivations — pure BDP and the AIMD window — were falsified by counted brackets and deleted). A/B lever: the governor is the shipped arm; 0 is the hold-only arm; a pin is the fixed-depth bracket." },
    .{ .key = "SQUEEZEFS_DIRECT_DEVICE_TRUE", .kind = .bool, .default = "0", .class = .ab_lever, .doc = "The measurement RULER (`-o direct_device_true`): an O_DIRECT read skips the classifier, every tier serve probe, the ghost, the escalation and the single-flight — N diagnostic readers = N device reads; every request size on a passthrough volume is a ranged window. Overlay probes stay (RYW is never diagnostic-optional). A/B lever: 0 is the shipped hybrid-I/O arm (O_DIRECT is NOT an admission class); 1 is the device-true ruler." },
    .{ .key = "SQUEEZEFS_TEST_READ_REBIND_LADDER_CAP", .kind = .{ .int = .{ .lo = 0, .hi = 24 } }, .default = "0", .class = .test_seam, .doc = "Test seam: the non-escalating binding-rebind ladder cap instead of MAX_REBINDS = 24 (0 = the constant), so exhaustion → the serialized settle is reachable without a 24-win schedule lottery." },
    .{ .key = "SQUEEZEFS_TEST_READ_ADMISSION_EPOCH_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "0", .class = .test_seam, .doc = "Test seam: the admission governor's epoch in ms instead of the 2 s house epoch (0 = the constant), so the roll, the clamp and the token mint are observable within a test's budget." },
    .{ .key = "SQUEEZEFS_TEST_STAGED_READ_HOLD", .kind = .str, .default = "off", .class = .test_seam, .doc = "Test seam: a FIFO path. A read of a STAGED file parks between its identity resolve and its ring lookup until a writer opens the FIFO — the window a promotion's layout commit and ring release land in (the lost-arm repro: fstests generic/127 under the promote-at-fsync lever). Read once at the plane's open; `off` = no hold." },

    // ---- the data plane: the write path (read by data/write_admit.zig through its modules' resolvers) ----
    .{ .key = "SQUEEZEFS_PATCH_MAX_BYTES", .kind = .{ .int = .{ .lo = 0, .hi = BYTES_MAX } }, .default = "derived", .class = .ab_lever, .doc = "W1's ceiling and the device overlay's length floor, bytes, verbatim; derived block/8 (512 KiB at 4 MiB — the natural sub-block denominator; above it a fresh CoW destination beats patching in place under a crash window). A/B lever: the derivation is the shipped arm (rand-4k 354–397 → 61.5–66.7 k IOPS); 0 is the patch-OFF control arm (every length overlay-eligible)." },
    .{ .key = "SQUEEZEFS_PARKED_BUFFERS", .kind = .{ .int = .{ .lo = 1, .hi = 1 << 24 } }, .default = "derived", .class = .ab_lever, .doc = "The parked-write buffer budget, 4 MiB-class buffers, verbatim; derived max(256, mem_budget/16 ÷ block) — R5 already gauges and sheds parked RAM; 256 = the shipped floor, never regress. Red halves it. A/B lever: the derivation is the shipped arm." },
    .{ .key = "SQUEEZEFS_DEVICE_OVERLAY", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "Arm B of the write admission: zero-copy device overlay stores for page-aligned pieces above the patch ceiling (WRITE_FIXED straight from the registered slot or ONE DMA from an owned buffer; reader-visible before the ack; ONE tx at completion). A/B lever: on is the shipped arm (aligned overwrite ingest 31.0 → 32.1 GiB/s at half the daemon CPU, merge share 1.000 → 0.001); 0 is the accumulation control arm." },
    .{ .key = "SQUEEZEFS_OVERLAY_OVERWRITE", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "The overlay's OVERWRITE shape (a record over a mapped block captures the old binding in ONE meta-stripe section and FEEDS the rewrite epoch at completion). A/B lever: on is the shipped arm; 0 declines overwrites to accumulation — the venue split recorded: a device-bound local zram-tcp prefers 0 (1149–1161 vs 783–940 MiB/s at qd4)." },
    .{ .key = "SQUEEZEFS_OVERLAY_CLOSE_BARRIER", .kind = .bool, .default = "off", .class = .ab_lever, .doc = "A data barrier before a coverage-complete overlay close on a write-back namespace (the stronger posture for the OW-8 window: power loss after an unbarriered non-fsync save). A/B lever: off is the shipped arm (the inherited DUR-2 class is disclosed, not paid per block); 1 is the barrier-per-close bracket." },
    .{ .key = "SQUEEZEFS_OVERLAY_DEPTH_GOVERNOR", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "Overlay stores take the write-pipeline depth permit (ONE gauge, ONE target, ONE gate for both vehicles; the permit is taken after the registry and before the claim). A/B lever: on is the shipped arm; 0 is the ungoverned control arm (an overlay burst past the BDP target)." },
    .{ .key = "SQUEEZEFS_GAP_SEED_RANGED", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "The overlay settle seeds each uncovered gap of an overwrite by ONE ranged read of exactly its bytes (when Σ gaps < the block window) instead of one whole old-image read. A/B lever: on is the shipped arm; 0 is the whole-image control arm." },
    .{ .key = "SQUEEZEFS_ZC_ACK_EARLY", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "ACK-early: a page-cache-sound held zero-copy payload under COMMIT_RETAIN acks before its overlay DMA lands (the detached continuation retries until it does; the fence latch is the ONE legal drop). A/B lever: on is the shipped arm; 0 acks every store after its CQE." },
    .{ .key = "SQUEEZEFS_ZC_ACK_EARLY_ODIRECT", .kind = .bool, .default = "off", .class = .switch_, .doc = "Opt-in: ACK-early for the O_DIRECT/GUP class as a SNAPSHOT (the pages are not stable after the ack — the 2026-08-09 dd oflag=direct aliasing; NFS UNSTABLE semantics). Off = the O_DIRECT class acks after its CQE." },
    .{ .key = "SQUEEZEFS_ZC_HOLD_OVERLAY", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "Zero-copy big writes (charter §6.2 item 1): a page-aligned single-block WRITE above PATCH_MAX (the overlay class) is HELD in its zero-copy slot at delivery and lands by WRITE_FIXED(device ← slot) from the app's pages — zero daemon copies; both payload classes, the O_DIRECT slot stored synchronously before its ack. A/B lever: on is the shipped arm; 0 is the extract-at-delivery control arm (WRITE_FIXED(slot → bounce) on io-wq, then the overlay's DMA from the bounce: 0.77 ms of io-wq per MiB, half the daemon's cores on 1M seq write ×4)." },
    .{ .key = "SQUEEZEFS_REWRITE_SHADOW", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "The rewrite epoch: an overwrite's displaced keys park and ONE whole-tx save swaps them at close (full coverage / fsync / release / shape change / ENOSPC / idle TTI/10 / fence / unmount). A/B lever: on is the shipped arm (rewrite_amp 1.0000); 0 is the per-block durable-publish control arm (`rewrite_shadow_fallbacks`)." },
    .{ .key = "SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS", .kind = .{ .int = .{ .lo = 0, .hi = 1 << 20 } }, .default = "derived", .class = .ab_lever, .doc = "The write-pipeline depth: unset = the adaptive BDP governor (per-lane bw × latency floor × 3, probed up BBR-style, Red-clamped to the measured drain, capped budget/4). A/B lever: the governor is the shipped arm; 0 = synchronous inline uploads (the pre-campaign control arm); N = pinned at N blocks (the fixed-depth bracket: forced 64 measured 13.7 GB/s vs pure-BDP 11.6 vs raw 16.6)." },
    .{ .key = "SQUEEZEFS_FSYNC_TOUCHED_NAMESPACES", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "fsync barriers only the data namespaces the ino TOUCHED (the per-ino stripe word stamped at every layout publish and in-place DMA; write-through namespaces skip). A/B lever: on is the shipped arm; 0 barriers every volatile namespace (the pre-economy control arm)." },
    .{ .key = "SQUEEZEFS_FSYNC_PARALLEL_LEGS", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "fsync's independent data-barrier legs (≥ 2 namespaces) run joined and ALL awaited — the fsync pays the slowest leg, not the sum; never cancels a sibling. A/B lever: on is the shipped arm; 0 runs the legs sequentially." },

    // ---- the data plane: the block-map tree (read by data/kvmap.zig, data/plane.zig) ----
    .{ .key = "SQUEEZEFS_MAP_MIGRATE_CHUNK", .kind = .{ .int = .{ .lo = 64, .hi = 1024 } }, .default = "512", .class = .ab_lever, .doc = "Record ops per block-map-tree crossing / sweep transaction, verbatim; derived floor_pow2((128 KiB / 2) / (map record frame + ref record frame)) = 512 — half the whole-entry cap for the records, the head and inode Puts in the other half (charter §5.6 'chunked Puts (512)'). The sweep threshold is chunk × 64 blocks, so this is the one lever behind both. The range top keeps a mis-set value inside the 128 KiB whole-entry admission. A/B lever: 512 is the shipped arm; 64 is the many-small-txs bracket." },
    .{ .key = "SQUEEZEFS_TEST_OWN_BLOCK_POISON", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: a promotion's own block is written whole with 0xAA before its image grain lands — the bytes a recycled block's previous owner leaves where a block device's discard is elided (the file-backed rigs punch every free, so the stale-bytes kill row plants them; decision 0003 §AL.5)." },
    .{ .key = "SQUEEZEFS_TEST_KVMAP_CROSSING_KILL_AFTER", .kind = .{ .int = .{ .lo = 0, .hi = std.math.maxInt(u32) } }, .default = "0", .class = .test_seam, .doc = "Test seam: the daemon SIGKILLs itself after the N-th committed chunk of a block-map-tree crossing, before the flip (0 = never) — the mid-crossing crash the remount's ratchet must resume from." },

    // ---- the D0 single-writer guard: the per-mount NVMe host identity (read by meta/guard/pr.zig) ----
    .{ .key = "SQUEEZEFS_HOSTNQN", .kind = .str, .default = "-", .class = .config, .doc = "The NVMe host NQN this daemon's fabric associations present and whose registrations the PR primitive judges as its own; PAIR-OR-NEITHER with SQUEEZEFS_HOSTID (exactly one configured refuses the mount: a hostnqn under the host's default hostid is how registrants alias on the device). Absent = /etc/nvme/hostnqn, read-only — never minted or written by a mount (quirk Q7). `-o hostnqn=` wins over the env." },
    .{ .key = "SQUEEZEFS_HOSTID", .kind = .str, .default = "-", .class = .config, .doc = "The NVMe host identifier (uuid) paired with SQUEEZEFS_HOSTNQN; pair-or-neither. Absent = /etc/nvme/hostid, read-only. Re-read every heartbeat: a change under a live PR mount fail-stops the volume (writer_guard_identity_mismatch). `-o hostid=` wins over the env." },
    .{ .key = "SQUEEZEFS_PR_REGISTRANT_CAP", .kind = .{ .int = .{ .lo = 1, .hi = 65_535 } }, .default = "derived", .class = .derived_override, .doc = "A DECLARED registrant cap for a vendor array whose Reservation Report refuses past N registrants (charter §5.5: declared or LEARNED only from a repeated device-answered command-specific status); absent = learn. The kernel nvmet target is unbounded by construction (pr_registrant_cap_refusals stays 0 there). Read by meta/sym/membership/fencing.zig." },

    // ---- NVMe-oF target management + the initiator (read by ops/nvmet/{ledger,verbs}.zig) ----
    .{ .key = "SQUEEZEFS_NVMEOF_STATE_DIR", .kind = .str, .default = "-", .class = .config, .doc = "The share ledger's directory (absent = /var/lib/squeezefs/nvmeof): `shares.sqzl`, its `.lock` and `.tmp`. A RELOCATION seam (the fidelity tier points it under its own root), never a behavior fork; host-scoped because a target host serves backings of any number of sets and `restore` runs at boot before any set is mountable." },
    .{ .key = "SQUEEZEFS_NVMET_PORT_ID_BASE", .kind = .{ .int = .{ .lo = 1, .hi = 64_536 } }, .default = "derived", .class = .derived_override, .doc = "The base of the reserved nvmet port-id range [base, base + 999] (absent = 53000, disjoint from the reference dev-substrate's 52026 and rig 52470/52471): the first candidate is base + hash64(trtype:ip:port) % 900, then a linear probe through the whole range; ownership of an existing port is ledger membership, never a prefix. Port ids are u16 (the kernel's kstrtou16), hence base ≤ 64536; the fidelity tier sets 54000." },
    // Derived from the single-writer guard's clock (claim.zig): reconnect_delay = the beat (10 s), fast_io_fail_tmo = the staleness TTL (45 s), ctrl_loss_tmo = 2 × TTL (90 s) — multipath.deriveConnectTimeouts.
    .{ .key = "SQUEEZEFS_FABRIC_FAST_IO_FAIL_TMO_S", .kind = .{ .int = .{ .lo = 1, .hi = 600 } }, .default = "derived", .class = .derived_override, .doc = "The `fast_io_fail_tmo` of every daemon-owned fabric connect (the data volumes' paths) and of `squeezefs nvmeof connect`, seconds, verbatim: with EVERY path of a multipath head down, the head's I/O is held until each path's fail-fast expires and then fails (EIO — the journal/data fail-stop); a single path's loss fails over at once regardless. Derived = the writer claim's staleness TTL (45 s): a holder whose every path is down that long is preemptable, so its held I/O must fail THEN, well under the FUSE 135 s request timeout. The kernel's own default is OFF (I/O held for ctrl_loss_tmo, 600 s). The range ceiling is MS_MAX in seconds." },
    .{ .key = "SQUEEZEFS_FABRIC_CTRL_LOSS_TMO_S", .kind = .{ .int = .{ .lo = 1, .hi = 3_600 } }, .default = "derived", .class = .derived_override, .doc = "The `ctrl_loss_tmo` of every daemon-owned fabric connect and of `squeezefs nvmeof connect`, seconds, verbatim: how long a lost path reconnects before the kernel deletes its controller. Derived = 2 × the claim's staleness TTL (90 s): one more window past fail-fast, so an outage that spans fail-fast still brings the association back for the FAILED daemon's teardown to release — never the kernel's 600 s default, which pins a dead association for ten minutes. An hour is the ceiling an operator may ask for; infinite (-1) is refused by range." },
    .{ .key = "SQUEEZEFS_FABRIC_RECONNECT_DELAY_S", .kind = .{ .int = .{ .lo = 1, .hi = 600 } }, .default = "derived", .class = .derived_override, .doc = "The `reconnect_delay` of every daemon-owned fabric connect and of `squeezefs nvmeof connect`, seconds, verbatim: the pause between a lost path's reconnect attempts. Derived = the claim's heartbeat (10 s, the kernel's own default too): a path retries as often as the claim beats, so a path back within one beat is seen before the claim ages. The range ceiling is MS_MAX in seconds." },
    // Default round-robin (owner ruling 2026-10-18, two 200 GbE links, aggregate bandwidth): numa pins a single-socket client to one link; queue-depth left a lone sequential stream 100 % on one path (131072 / 0 sectors) and skewed four parallel writers 409600 / 114688; round-robin split every measured workload within 5 %.
    .{ .key = "SQUEEZEFS_FABRIC_IOPOLICY", .kind = .{ .enumeration = .{ .allowed = &.{ "numa", "round-robin", "queue-depth" } } }, .default = "round-robin", .class = .enum_select, .doc = "The kernel NVMe multipath I/O policy the daemon writes to `/sys/class/nvme-subsystem/<subsys>/iopolicy` for every subsystem it owns paths of (a data volume's `fabric_endpoint` record with several paths = several controllers under ONE host identity merged into one head, and the metadata head the operator connected; read by ops/nvmet/mount_fabric.zig). `numa` (the kernel default) sends every I/O of a NUMA node down one path — a single-socket client never uses its second link; `round-robin` alternates per request, so every stream uses every link (owner ruling 2026-10-18: the fabric is two 200 GbE links and the goal is aggregate bandwidth; measured on 7.2.8-sqz over two loopback paths, every workload split within 5 %: 4 parallel 64 MiB writers 254160 / 270128 sectors, their cold read by 4 readers 262144 / 262144); `queue-depth` picks the path with the fewest requests in flight (kernel ≥ 6.11) — ties go to the first path, so a lone sequential stream stayed 100 % on one link (131072 / 0) and four parallel writers split 409600 / 114688; it stays selectable for a fabric whose paths differ in latency. A kernel without the policy keeps its own: the mismatch read back is a warning + fabric_iopolicy_failures, never a refusal." },

    // ---- the zcrx read lane (read by ops/zcrx/probe.zig) ----
    .{ .key = "SQUEEZEFS_ZCRX_LANE", .kind = .bool, .default = "0", .class = .ab_lever, .doc = "The zcrx read lane (charter §6.3): a second userspace NVMe/TCP host association per nvme-tcp data device serving COLD read fills over io_uring zero-copy receive; the arm is probe-gated regardless (RECV_ZC, an HDS-capable NIC with ≥ 4 RX queues, ntuple on) and every refusal is loud with `zcrx_lane_absent_reason` set. A/B lever: 0 is the shipped control arm (the kernel path; the reference's engagement campaign parked default-on on economics — 0.05 % engagement could not pay the RSS rent); 1 is the measured arm (nvme-tcp RX copy 55 % of client cycles at 46.6 GB/s deleted; 4 queues at matched 24.79 GB/s: 3.32 → 1.06 busy cores). Logged at mount when on." },
    .{ .key = "SQUEEZEFS_TEST_ZCRX_AREA_SIM", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: arm the lane's area backend over plain RECV into granted area chunks (the venue correctness leg — everything above the opcode is the product code) at gate 4 of the probe ladder, before any NIC gate; requires SQUEEZEFS_ZCRX_LANE=1 and a tcp target." },
    .{ .key = "SQUEEZEFS_TEST_ZCRX_SIM_CHUNK", .kind = .{ .int = .{ .lo = 64, .hi = 2 << 20 } }, .default = "derived", .class = .test_seam, .doc = "Test seam: the sim leg's recv chunk grain in bytes, rounded up to a power of two inside [64 B, 2 MiB] (derived 64 KiB); a tiny grain forces PDU headers to straddle chunk seams." },
    .{ .key = "SQUEEZEFS_TEST_ZCRX_TARGET", .kind = .str, .default = "derived", .class = .test_seam, .doc = "Test seam: the lane's target instead of the sysfs probe (absent = derived from /sys/class/nvme) — `traddr,trsvcid,subnqn,nsid,lba_shift,max_xfer[,io_queues,queue_depth]`; a malformed value is loud and the lane stays Absent (never a guessed coordinate)." },

    // ---- the job fabric (read by ops/jobs/coordinator.zig) — @anchor:ops_jobs_knobs ----
    .{ .key = "SQUEEZEFS_JOB_CPU_LIMIT", .kind = .{ .int = .{ .lo = 1, .hi = 100 } }, .default = "100", .class = .ab_lever, .doc = "The default duty-cycle pct of a job submitted without a throttle (KD-3: a worker sleeps t × (100 − pct)/pct after a task of t, in 50 ms slices re-reading the live word). A/B lever: 100 is the unthrottled control arm; `job throttle` retunes a running job live." },
    .{ .key = "SQUEEZEFS_JOB_WORKERS", .kind = .{ .int = .{ .lo = 1, .hi = 64 } }, .default = "derived", .class = .derived_override, .doc = "The local job worker pool, verbatim; derived clamp(cpus/4, 2, 8) — the L4 service posture's slope, floored so a small box runs a coordinator pair, ceilinged so a big one does not spawn a worker farm for a duty-cycled plane. Sizes the sheddable job_copy_buffers component (64 blocks per worker)." },
    .{ .key = "SQUEEZEFS_JOB_WIRE_VERIFY_PERMILLE", .kind = .{ .int = .{ .lo = 1, .hi = 1000 } }, .default = "1000", .class = .ab_lever, .doc = "The coordinator's verify-read sampling of a remote worker's proposed destinations, honoured ONLY under CA-pinned mTLS; on a plaintext channel 1000 is pinned (the session MAC authenticates, it does not make the bytes that crossed the wire trustworthy). A/B lever: 1000 is the control arm." },
    .{ .key = "SQUEEZEFS_FLEET_JOBS", .kind = .bool, .default = "1", .class = .switch_, .doc = "The fleet halves of the fabric: this mount enrols as a remote worker of volume 0's manager once it has joined the membership plane, and (as the coordinator) ships fleet READ shards to enrolled read-capable members. 0 opts out of both halves; a single-writer mount's jobs run locally either way." },
    .{ .key = "SQUEEZEFS_TEST_JOB_PARK_DELAY_MS", .kind = .{ .int = .{ .lo = 0, .hi = 60_000 } }, .default = "0", .class = .test_seam, .doc = "Test seam: hold the paused-park window (between the durable Paused checkpoint and settle_park's intent re-check) for this long — the lost-resume repro's only honest observable is the park entry; the window is a Notify deadline wait, never a sleep." },
    .{ .key = "SQUEEZEFS_TEST_JOB_HOLD_SUBMISSION", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: a remote worker parks before its result_submit until released (the SIGSTOP-zombie window: paused past the TTL, reassigned, its late submit refused stale)." },
    .{ .key = "SQUEEZEFS_TEST_MOVER_PRE_PUBLISH_HOLD", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: the ONE mover parks after the copy and before the publish lock until released (the supersession / crash-injection window of the kill matrix)." },

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
    .{ .key = "SQUEEZEFS_TEST_KVDA_PAGE_SKIP", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: the allocation holder's checkpoint cycle skips its KVDA page writes (the bitmap's deltas still ride ring 0, but every checkpoint moves the tail past them), read once at the manager's open — the kill row of a holder whose pages never landed: its successor's fold has nothing, the predecessor's pages are stale, and the live writers' windows are re-learnt from their declarations (`block_grant_relearned_from_windows` reads the delta instead of 0)." },
    .{ .key = "SQUEEZEFS_TEST_BLOCK_GRANT_LEDGER_CAP", .kind = .{ .int = .{ .lo = 2, .hi = 1024 } }, .default = "derived", .class = .test_seam, .doc = "Test seam: the allocation holder's grant ledger capacity (OPEN grants per data volume) instead of the derived 1,024, read once at the holder's arm — the joiner-side grant ledger row drives a joiner past the cap at a smoke-sized workload (every consumed grant must retire at the writer's renewal declaration, or the table fills and every top-up answers Deferred: rung 9 F17's class on the wire). Absent = the derived capacity." },

    // ---- the cluster wire (read by wire/listener.zig, wire/tls.zig) ----
    .{ .key = "SQUEEZEFS_WIRE_CHANNEL", .kind = .{ .enumeration = .{ .allowed = &.{ "plaintext", "mutual_tls" } } }, .default = "plaintext", .class = .switch_, .doc = "The cluster wire's channel class for this daemon and the verbs that dial from this host: `plaintext` = the storage-trust HMAC ladder alone (authenticated, not confidential); `mutual_tls` = the same ladder inside TLS 1.3 under the set's own CA (authenticated AND confidential: the per-frame MAC key is bound to the channel's exporter; the job fabric's verify-read sampling below 100 % is admissible). Every member of a set runs one class: a peer of the other class is refused loud at the handshake and counted (cluster_wire_plaintext_refused / cluster_wire_tls_refused_*). Under mutual_tls a missing or unreadable CA REFUSES the mount — never a downgrade." },
    .{ .key = "SQUEEZEFS_WIRE_TLS_CA", .kind = .str, .default = "-", .class = .config, .doc = "The set's CA certificate (PEM, Ed25519) every peer's leaf must verify under; absent = /etc/squeezefs/tls/ca.pem. Opened O_NOFOLLOW: a regular file owned by the invoking user or root that nobody else may write. Minted by `squeezefs key tls-init`." },
    .{ .key = "SQUEEZEFS_WIRE_TLS_CERT", .kind = .str, .default = "-", .class = .config, .doc = "This node's certificate (PEM, Ed25519, issued under the CA by `squeezefs key tls-issue`); absent = /etc/squeezefs/tls/node.pem. The same file law as the CA. Its CN is the node name peers log." },
    .{ .key = "SQUEEZEFS_WIRE_TLS_KEY", .kind = .str, .default = "-", .class = .config, .doc = "This node's private key (PEM PKCS#8 Ed25519); absent = /etc/squeezefs/tls/node.key. The key-file law of charter §6.5: O_NOFOLLOW, a regular file, mode 0600, owned by the invoking user or root; a key that is not the certificate's refuses the mount." },
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
    .{ .key = "SQUEEZEFS_TEST_HANDOVER_HOLD", .kind = .{ .enumeration = .{ .allowed = &.{ "off", "after_page", "after_tree0", "park_after_page", "park_mid_install" } } }, .default = "off", .class = .test_seam, .doc = "Test seam: the flush-then-transfer stops after a named durable step — `after_page` (the page names the slot Releasing, tree 0 still Leased: crash row 5), `after_tree0` (tree 0 Unleased, the page still names the slot: row 6) — or parks after the page (`park_after_page`: the door contract's window), or a joiner's wire-grant install parks between its two halves (`park_mid_install`: the tree and the lease word), until the harness releases it. Read once at the plane's open; default-inert." },

    // ---- GPFS-strict read tokens: the holder's seams (read ONCE at the plane's arm by meta/sym/tokens/token_table.zig; never process statics) ----
    .{ .key = "SQUEEZEFS_TEST_TOKEN_RECALL_DEADLINE_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "0", .class = .test_seam, .doc = "Test seam: the token recall deadline in ms instead of T_owner = 45 000 (0 = the constant), so the live-timeout contract (a recall unacked by a LIVE member past the TTL: `dlm_token_recall_timeouts_live`, the token retired, the commit proceeding) completes inside a test's budget. The product deadline is the membership lease TTL by derivation — never a p99, never a lever." },
    .{ .key = "SQUEEZEFS_TEST_TOKEN_CONVEYOR_HOLD_POST_RECALL", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: the holder's pass HOLDS after its A1½ recall completed, before it proceeds to ring admission, until the plane's hold is released — the exact window a first-touch grant on an object of the union must PARK in (register-then-check) and then serve the committed records." },
    .{ .key = "SQUEEZEFS_TEST_MEMBERSHIP_LEAVE_LOST", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: a joiner's clean stop sends no membership leave (the leave lost on the wire), so the home shard keeps its row for the successor's roster — the `-o ro` row's remount through a manager's remount (decision 0003 §AG.5)." },
    .{ .key = "SQUEEZEFS_TEST_ONLINE_ARM_HOLD", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: the manager's online `volume add-data` HOLDS its grant-plane arm of the new volume (after the record's commit, before the allocation lease) until the holder has answered a data-plane request naming a volume it has not armed, bounded by T_owner — the window in which a joiner that learnt the volume early asks the holder for it (decision 0003 §AI.3)." },
    .{ .key = "SQUEEZEFS_TEST_TOKEN_EVICT_AFTER_SERVE", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: every foreign serve's token entry is evicted under the room law right after it is served (a kernel-pinned entry is dropped without a release, a cold one released — decision 0003 §AG.2), so a reader holds no token entry when the next change to the object lands." },
    .{ .key = "SQUEEZEFS_TEST_TOKEN_GRANT_HOLD_AFTER_CHECK", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: a grant HOLDS between its gate check and its record read until the plane's hold is released — the window in which a pass takes the object in flight DURING the read, so the re-park + re-read arm is exercised deterministically." },

    // ---- cross-owner intents + directory striping: the seams (read ONCE at the plane's init by meta/sym/xv/ops.zig and stripe.zig; never process statics) and the one striping lever ----
    .{ .key = "SQUEEZEFS_SYM_DIR_STRIPES", .kind = .{ .int = .{ .lo = 1, .hi = 64 } }, .default = "derived", .class = .ab_lever, .doc = "The directory striping width K, verbatim; derived MINT_SPREAD = 64 (the shipped granularity constant: 64 stripe inos per directory is cheap; a directory is never re-striped). A/B lever: derived is the shipped arm; 1 = striping OFF — the control arm: no flip ever runs; an existing map still routes." },
    .{ .key = "SQUEEZEFS_TEST_XV_SHIP_FAIL_AT", .kind = .{ .int = .{ .lo = 0, .hi = 1 << 20 } }, .default = "0", .class = .test_seam, .doc = "Test seam: the N-th shipped cross-owner step of this mount fails as a lost connection (0 = never), so the intent stays OPEN and the roll-forward cadence's completion is exercised deterministically." },
    .{ .key = "SQUEEZEFS_TEST_XV_CRASH_AFTER_STEP", .kind = .{ .int = .{ .lo = 0, .hi = 1 << 20 } }, .default = "derived", .class = .test_seam, .doc = "Test seam: the intent executor stops dead right after step N committed (0 = after step 0 + the intent; ≥ the plan length = before the retirement) — the kill -9 window the next mount's roll-forward must complete to exactly one outcome. Absent = never." },
    .{ .key = "SQUEEZEFS_TEST_XV_SERVE_SLOT_MOVED_ONCE", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: the holder answers the next served cross-owner step `SlotMoved` once, so the initiator's re-resolve + re-dispatch arm (≤ 2 retries) is exercised deterministically." },

    // ---- the gate's test kits (read by core/testkit.zig, core/fuzzkit.zig; charter §11 static rails + fuzz) ----
    .{ .key = "SQUEEZEFS_TEST_SKIP_LEDGER", .kind = .harness, .default = "-", .class = .harness, .doc = "Harness: the file every `testkit.skip` appends its row to (`class<TAB>file:line<TAB>verdict<TAB>detail`), so a gate run's environment skips are counted by class; unset = stderr only." },
    .{ .key = "SQUEEZEFS_TEST_RELEASE_GATE", .kind = .harness, .default = "0", .class = .harness, .doc = "Harness: release-gate mode (`zig build gate -Drelease-gate=true` sets it on every test run step) — a `testkit.skip` FAILS the test except for the `needs_reference_hardware` and `other_step` classes." },
    .{ .key = "SQUEEZEFS_TEST_FUZZ_REPLAY", .kind = .harness, .default = "-", .class = .harness, .doc = "Harness: a file whose bytes every `fuzzkit.fuzz` test replays through its body before the corpus (the campaign's crash artefact and its minimizer's candidates); unset = no replay." },
    .{ .key = "SQUEEZEFS_TEST_FUZZ_SECONDS", .kind = .harness, .default = "60", .class = .harness, .doc = "Harness: the nightly fuzz campaign's time budget per fuzz target in seconds (`zig build fuzz`); the campaign total is targets × budget." },
    .{ .key = "SQUEEZEFS_TEST_BENCH_RIG", .kind = .harness, .default = "-", .class = .harness, .doc = "Harness: set by the `bench-baseline` / `bench-compare` build steps on their run steps; the baseline rig's tests run only when it is set (the unit run, which finds the installed binary too, skips them as other_step)." },
    .{ .key = "SQUEEZEFS_TEST_FUZZ_CORPUS_DIR", .kind = .harness, .default = "/build/fuzz-corpus", .class = .harness, .doc = "Harness: the campaign's persistent corpus root — one directory per fuzz target (`<root>/<target slug>/`, the build cache's `f/<hash>` dir symlinked onto it) with the saved crash artefacts under `crashes/`; the venue default is /build/fuzz-corpus, elsewhere `fuzz-corpus/` under the repo (gitignored)." },

    // ---- membership, the death ledger and recovery: the venue legs' seam (read ONCE at the shard's arm by meta/sym/membership/shard.zig; never a process static) ----
    .{ .key = "SQUEEZEFS_TEST_MEMBERSHIP_T_OWNER_MS", .kind = .{ .int = .{ .lo = 1000, .hi = MS_MAX } }, .default = "derived", .class = .test_seam, .doc = "Test seam: the membership lease's T_owner in ms instead of the ONE staleness TTL (45,000 ms), so the two-daemon legs (kill -9 a joiner ⇒ the home shard evicts ⇒ RecordDeath ⇒ recovery ⇒ its slots Unleased; a dead manager ⇒ park-and-reclaim) complete inside a test's budget. Every derived clock (skew_max, T_self, renew, grace, the retirement at 2 × T_owner, the custody quarantine) follows it; absent = the constant." },
    .{ .key = "SQUEEZEFS_TEST_RECOVERY_HOLD", .kind = .{ .enumeration = .{ .allowed = &.{ "off", "park_after_flush" } } }, .default = "off", .class = .test_seam, .doc = "Test seam: a dead appender's recovery parks after its flush cycles (§5.9 step 6: the recovered slot trees installed, replayed and clean; tree 0 still Leased) until the harness releases it — the window in which the manager's own SMOs (the checkpoint task's maintenance, the D4 arm) retire images of the dead grant before its census runs. Read once at the volume binding's create (meta/sym/membership/volume_bind.zig); `off` = no hold." },
};

pub fn lookup(key: []const u8) ?*const Knob {
    for (&registry) |*k| if (std.mem.eql(u8, k.key, key)) return k;
    return null;
}

/// The row of a key known at compile time: an unregistered key is a compile
/// error, so a reader cannot exist without its row.
pub fn row(comptime key: []const u8) *const Knob {
    return comptime lookupComptime(key) orelse @compileError(key ++ " is not a registry row (src/core/knobs.zig)");
}

/// `lookup` for comptime callers: one key comparison per row, each a byte
/// loop — the default 1000 branches ran out once the registry passed ~100
/// rows.
fn lookupComptime(comptime key: []const u8) ?*const Knob {
    @setEvalBranchQuota(registry.len * 200);
    for (&registry) |*k| if (std.mem.eql(u8, k.key, key)) return k;
    return null;
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

/// The ONE parser is total on arbitrary environment text: every kind
/// returns `absent`, a value or a typed `invalid` — never a panic — and a
/// value that parses re-parses to itself (the canonical spelling is a
/// fixpoint).
fn parsersTotalFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [256]u8 = undefined;
    const len = smith.slice(&buf);
    const raw = buf[0..len];
    _ = parseBool("SQUEEZEFS_RAIL_FIXTURE_A", raw);
    switch (parseIntIn(i64, "SQUEEZEFS_RAIL_FIXTURE_A", raw, -7, 1 << 40)) {
        .value => |v| try std.testing.expect(v >= -7 and v <= 1 << 40),
        .absent, .invalid => {},
    }
    switch (parseEnum("SQUEEZEFS_RAIL_FIXTURE_A", raw, &.{ "auto", "off", "zc" })) {
        .value => |canonical| try std.testing.expectEqualStrings(canonical, parseEnum("SQUEEZEFS_RAIL_FIXTURE_A", canonical, &.{ "auto", "off", "zc" }).value),
        .absent, .invalid => {},
    }
    if (parseStr(raw)) |s| try std.testing.expect(s.len != 0 and s.len <= raw.len);
}

test "fuzz: the knob parsers are total on arbitrary environment text and canonical spellings are fixpoints" {
    try fuzzkit.fuzz({}, parsersTotalFuzz, .{ .corpus = &.{ fuzzkit.slice("  yes "), fuzzkit.slice("-7"), fuzzkit.slice("1099511627777"), fuzzkit.slice("ZC") } });
}
