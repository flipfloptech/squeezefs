//! FUSE-over-io_uring transport geometry — the ONE pure derivation of the
//! queue count, ring depth, `max_write`/`max_pages`/`payload_sz`,
//! `max_background`/`congestion_threshold`, the rings per queue and the
//! per-ring SQ size (charter §4 "Geometry" + "Rings per queue";
//! `docs/porting/fuse_abi_session.md` §3, `fuse_uring_transport.md` §1.7,
//! §12, §14). Resolved ONCE, before the INIT reply is serialized, and
//! published as the `transport_*` level gauges when the session arms.
//!
//! Every input is injected (`Inputs`): the kernel's possible CPU count, the
//! `/proc/sys/fs/fuse/max_pages_limit` sysctl (the Linux reader lives behind
//! `builtin.os.tag == .linux`; the fallback 256 is the compiled-in default of
//! kernels without the sysctl), the page size, the payload-buffer cap
//! (`mem/sizing.resolveTransportBufferCap` = `SQUEEZEFS_TRANSPORT_MEM_MAX` >
//! `_PCT` > budget / 8), the knob overrides. The plan is total: it never
//! refuses (the startup gate already did) and every tie row of the specs is
//! a test below.
//!
//! The kernel mirror being matched (`fuse_uring_create` / `process_init_reply`):
//!   fc->max_pages   = min(limit, max(arg->max_pages, 1))
//!   fc->max_write   = max(4096, arg->max_write)
//!   ring->max_payload_sz = max(FUSE_MIN_READ_BUFFER, fc->max_write, fc->max_pages × PAGE)
//!   ADD_BUFPOOL splits the pool into len / ring->max_payload_sz buffers
//! so `payload_sz == max_payload_sz` makes the pool's buffer `k` sit at
//! `k × payload_sz` (fuse_mainline_pools.md §2; the pre-fix planner pinned
//! 256 pages while advertising `max_pages = u16::MAX`: every user-ents
//! REGISTER refused on a raised-sysctl box, mount failed).

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const builtin = @import("builtin");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const counters = @import("../core/counters.zig");
const sizing = @import("../mem/sizing.zig");
const abi = @import("abi.zig");

const MiB = derive.MiB;
const GiB = derive.GiB;

/// Measured-best per-queue depth: QD32 + max_background 256 took the
/// decomposition box from 44 k to 316 k IOPS (7.2×); depth 16 alone was
/// +2.2×, mb 256 alone +0 — both together 316 k.
pub const Q_DEPTH_DESIRED: u32 = 32;
/// The pre-L1 shipped posture; at the floor the arena is exactly yesterday's
/// `queues × 4 × payload_sz` (never regress below shipped).
pub const Q_DEPTH_FLOOR: u32 = @intCast(sizing.transport_q_depth_floor);
/// The pre-4 MiB-campaign shipped ent = 256 × 4 KiB; `max_write` degrades
/// toward it under a small budget and never below it.
pub const PAYLOAD_BASE: u64 = sizing.transport_payload_base;
/// The historical dead-letter `max_background=64` option's intent.
pub const MAX_BACKGROUND_FLOOR: u32 = 64;
/// `fuse_init_out.max_background` is a u16: a protocol bound, not a policy
/// constant (the fixed 256 was retired 2026-08-04).
pub const MAX_BACKGROUND_CEILING: u32 = std.math.maxInt(u16);
/// The compiled-in default of kernels without the `max_pages_limit` sysctl.
pub const MAX_PAGES_LIMIT_FALLBACK: u32 = 256;
/// `fuse_init_out.max_pages` is a u16.
pub const MAX_PAGES_LIMIT_CEILING: u32 = std.math.maxInt(u16);
/// The queue-count rail of the testing seam (`fuse_uring_create` sizes by
/// possible CPUs; 512 is the largest box the reference planned for).
pub const MAX_QUEUES: u32 = 512;
/// Smallest page the plan admits (a sysconf lie below the 512 B sector
/// floor would make `pages_for` explode).
pub const MIN_PAGE: u32 = 512;
/// The kernel's `max(4096, arg->max_write)`.
pub const MIN_MAX_WRITE: u32 = 4096;

comptime {
    std.debug.assert(Q_DEPTH_FLOOR == 4);
    std.debug.assert(PAYLOAD_BASE == 1 * MiB);
    std.debug.assert(MAX_BACKGROUND_FLOOR <= MAX_BACKGROUND_CEILING);
}

/// Everything `plan` keys on; every field injected so a test states any box.
pub const Inputs = struct {
    /// Kernel possible CPUs (`derive.Facts.possible_cpus`): qid == CPU id is
    /// what NUMA placement relies on; fewer queues never becomes ready.
    kernel_nqueues: u32,
    /// `SQUEEZEFS_TEST_FUSE_QUEUES` (testing only).
    env_queues: ?u32 = null,
    /// `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH`: wins verbatim, bypasses the ladder.
    env_depth: ?u32 = null,
    /// The daemon's DESIRE (`desiredMaxWrite`).
    desired_max_write: u32,
    /// `/proc/sys/fs/fuse/max_pages_limit`; null = unreadable ⇒ the fallback.
    max_pages_limit: ?u32,
    page_size: u32,
    /// The payload-arena cap in bytes (`sizing.resolveTransportBufferCap`).
    buffer_cap: u64,
    /// `-o max_background=` / `-o congestion_threshold=` (> 0 only).
    max_background_override: ?u16 = null,
    congestion_override: ?u16 = null,
};

pub const Plan = struct {
    nqueues: u32,
    depth: u32,
    max_write: u32,
    /// Describes `max_write` EXACTLY (`pages_for(max_write)`).
    max_pages: u16,
    /// The kernel's `ring->max_payload_sz` mirror.
    payload_sz: u32,
    max_background: u16,
    congestion_threshold: u16,
    /// The page and the limit the plan was made with (for the log line and
    /// the kernel-mirror check).
    page: u32,
    max_pages_limit: u32,

    pub fn payloadBuffers(self: Plan) u64 {
        return @as(u64, self.nqueues) * self.depth;
    }

    /// `nqueues × depth × payload_sz`: the pinned arena.
    pub fn totalPayloadBytes(self: Plan) u64 {
        return self.payloadBuffers() * self.payload_sz;
    }
};

/// `max(page_sz, 512)`.
fn pageOf(page_sz: u32) u32 {
    return @max(page_sz, MIN_PAGE);
}

/// `clamp(limit ?? 256, 1, 65535)`.
pub fn pagesLimit(max_pages_limit: ?u32) u32 {
    return @intCast(derive.clamp(max_pages_limit orelse MAX_PAGES_LIMIT_FALLBACK, 1, MAX_PAGES_LIMIT_CEILING));
}

/// `clamp(ceil(mw / page), 1, limit)`.
pub fn pagesFor(max_write: u64, page: u32, limit: u32) u32 {
    return @intCast(derive.clamp(derive.ceilDiv(max_write, page), 1, limit));
}

/// `max(8192, mw, pages_for(mw) × page)` — the kernel's `ring->max_payload_sz`.
pub fn payloadFor(max_write: u64, page: u32, limit: u32) u64 {
    return @max(@as(u64, abi.FUSE_MIN_READ_BUFFER), @max(max_write, @as(u64, pagesFor(max_write, page, limit)) * page));
}

/// What the kernel computes from an INIT reply `(max_write, max_pages)`
/// under `limit` — the acceptance bound every planned geometry must equal.
pub fn kernelRingMaxPayloadSz(limit: u32, max_write: u32, max_pages: u16, page: u32) u64 {
    const fc_max_pages: u64 = @min(limit, @max(@as(u32, max_pages), 1));
    const fc_max_write: u64 = @max(MIN_MAX_WRITE, max_write);
    return @max(@as(u64, abi.FUSE_MIN_READ_BUFFER), @max(fc_max_write, fc_max_pages * page));
}

/// `clamp(cap / (nqueues × payload_for(mw)), 4, 32)`.
fn depthAt(cap: u64, nqueues: u32, max_write: u64, page: u32, limit: u32) u32 {
    const per_depth = @as(u64, nqueues) * payloadFor(max_write, page, limit);
    return @intCast(derive.clamp(cap / per_depth, Q_DEPTH_FLOOR, Q_DEPTH_DESIRED));
}

/// THE plan (fuse_over_uring.rs `TransportGeometry::plan`, line for line).
pub fn plan(in: Inputs) Plan {
    const page = pageOf(in.page_size);
    const limit = pagesLimit(in.max_pages_limit);
    // The kernel clamps max_write to [4096, limit × page]; a desire outside
    // that band is a desire the kernel would silently correct — plan the
    // corrected value so every published gauge is what the kernel runs.
    const target: u64 = derive.clamp(in.desired_max_write, @max(page, MIN_MAX_WRITE), @as(u64, limit) * page);
    const nqueues: u32 = @intCast(derive.clamp(in.env_queues orelse in.kernel_nqueues, 1, MAX_QUEUES));
    const cap = in.buffer_cap;

    var max_write: u64 = target;
    var depth: u32 = undefined;
    if (in.env_depth) |d| {
        // Explicit wins verbatim and bypasses BOTH ladder legs.
        depth = @intCast(derive.clamp(d, 1, Q_DEPTH_DESIRED));
    } else {
        depth = depthAt(cap, nqueues, target, page, limit);
        const floor_arena = @as(u64, nqueues) * Q_DEPTH_FLOOR * payloadFor(target, page, limit);
        const base = @min(target, PAYLOAD_BASE);
        // The payload leg engages only when depth sits AT the floor, the
        // floor arena still overflows the cap, and there is room to shrink
        // toward the 1 MiB base: degrade max_write page-aligned, never
        // below the base, and re-derive the depth it affords.
        if (!(depth > Q_DEPTH_FLOOR or floor_arena <= cap or target <= base)) {
            const fit = cap / (@as(u64, nqueues) * Q_DEPTH_FLOOR);
            max_write = derive.clamp(fit / page * page, base, target);
            depth = depthAt(cap, nqueues, max_write, page, limit);
        }
    }

    const max_pages = pagesFor(max_write, page, limit);
    const payload_sz = payloadFor(max_write, page, limit);
    const mb: u32 = if (in.max_background_override) |o| (if (o > 0) o else policyMaxBackground(nqueues, depth)) else policyMaxBackground(nqueues, depth);
    const ct: u32 = if (in.congestion_override) |o| (if (o > 0) o else mb / 4 * 3) else mb / 4 * 3;
    return .{
        .nqueues = nqueues,
        .depth = depth,
        .max_write = @intCast(max_write),
        .max_pages = @intCast(max_pages),
        .payload_sz = @intCast(payload_sz),
        .max_background = @intCast(mb),
        .congestion_threshold = @intCast(ct),
        .page = page,
        .max_pages_limit = limit,
    };
}

/// `clamp(nqueues × depth, 64, u16::MAX)`: each uring queue's background
/// share (`max_background / nr_queues` under the sqz per-queue accounting)
/// is then exactly its ent count.
pub fn policyMaxBackground(nqueues: u32, depth: u32) u32 {
    return @intCast(derive.clamp(@as(u64, nqueues) * depth, MAX_BACKGROUND_FLOOR, MAX_BACKGROUND_CEILING));
}

/// The INIT `max_write` DESIRE: `SQUEEZEFS_FUSE_MAX_WRITE` verbatim, else
/// `max(block_size, 1 MiB)` clamped to `[4096, u32::MAX]` — still
/// sysctl-gated by `plan`.
pub fn desiredMaxWrite(block_size: u64, explicit: ?u32) u32 {
    if (explicit) |e| return e;
    return @intCast(derive.clamp(@max(block_size, PAYLOAD_BASE), MIN_MAX_WRITE, std.math.maxInt(u32)));
}

/// A `/proc/sys/fs/fuse/*` sysctl's text → value; null on junk (the caller
/// then takes the fallback, exactly as for an absent sysctl).
pub fn parseSysctlU32(text: []const u8) ?u32 {
    return std.fmt.parseInt(u32, std.mem.trim(u8, text, " \t\r\n"), 10) catch null;
}

pub const max_pages_limit_path = "/proc/sys/fs/fuse/max_pages_limit";

/// The sysctl on this kernel; null off-Linux or when unreadable (startup
/// control path: through the `Io` like every non-hot-path read).
pub fn readMaxPagesLimit(io: std.Io) ?u32 {
    if (builtin.os.tag != .linux) return null;
    var buf: [64]u8 = undefined;
    const text = std.Io.Dir.cwd().readFile(io, max_pages_limit_path, &buf) catch return null;
    return parseSysctlU32(text);
}

// ---------------------------------------------------------------------------
// The request timeout (decision 0003 §Z): the INIT reply's `request_timeout`.
// A request the kernel has waited on longer ends the connection
// (`fuse_check_timeout` → `fuse_abort_conn`: ENOTCONN to every caller, the
// mount fail-stopped), so a stopped or wedged daemon never holds the host's
// `sync` (FUSE_SYNCFS reaches every FUSE superblock) or any caller forever —
// the kernel's half of the supervisor's abort rung, armed with or without a
// supervisor.
// ---------------------------------------------------------------------------

/// The kernel's check period (`FUSE_TIMEOUT_TIMER_FREQ`, fs/fuse/dev.c): the
/// timeout work runs every 15 s and `init_server_timeout` raises a shorter
/// timeout to it — a request is aborted between T and T + 15 s of its
/// creation.
pub const REQUEST_TIMEOUT_CHECK_S: u16 = 15;

/// `T_req = T_owner + T_park_max` (ms → whole seconds rounded up; the u16
/// field saturates). The longest a HEALTHY daemon holds a request is a wait
/// on a peer the lattice is giving up: up to `T_owner` for the set to declare
/// it dead (a member's own park starts at `T_self < T_owner`), then the park
/// that follows, `T_park_max` (failover + grace), past which the op answers
/// the terminal EIO (§X.1). Every other bounded wait a handler can sit in is
/// shorter: the journal's FAILED latch (3 × `PARK_ESCALATION_NS` = 90 s), the
/// coherence hold (20 s), a token recall (≤ one lease TTL), the zc bridges
/// (30 s). A request older than that waits on nothing the lattice sanctions.
pub fn requestTimeoutS(t_owner_ms: u64, t_park_max_ms: u64) u16 {
    const s = (t_owner_ms +| t_park_max_ms) / 1000 + @intFromBool((t_owner_ms +| t_park_max_ms) % 1000 != 0);
    return @intCast(@min(s, std.math.maxInt(u16)));
}

/// `SQUEEZEFS_TEST_FUSE_REQUEST_TIMEOUT_S` (the frozen-daemon row's window).
pub fn requestTimeoutOverride(env: *const std.process.Environ.Map) ?u16 {
    return knobs.optIntKnob("SQUEEZEFS_TEST_FUSE_REQUEST_TIMEOUT_S", u16, env);
}

/// explicit > derived.
pub fn resolveRequestTimeoutS(explicit: ?u16, derived: u16) u16 {
    return explicit orelse derived;
}

/// The timeout the kernel applies to a nonzero request
/// (`init_server_timeout`): the `max_request_timeout` sysctl caps it (0 =
/// no cap), then the check period floors it.
pub fn kernelRequestTimeoutS(requested: u16, max_sysctl: ?u32) u16 {
    std.debug.assert(requested != 0);
    const cap = max_sysctl orelse 0;
    const capped: u32 = if (cap != 0) @min(requested, cap) else requested;
    return @intCast(@max(capped, REQUEST_TIMEOUT_CHECK_S));
}

/// THE STALL ANSWER's bound (charter §1.8: the daemon answers, never holds;
/// decision 0003 §BQ): how long the data plane holds a write behind a plane
/// that refuses every flush before it answers ENOSPC/EIO. The kernel ends
/// the WHOLE connection for one request older than its timeout, judged at
/// its next check (`fuse_check_timeout`, every `REQUEST_TIMEOUT_CHECK_S`)
/// from the request's creation — before the daemon saw it — so the answer
/// lands one check period ahead of the earliest abort: `T − 15 s`. `T` is
/// the timeout the kernel applies (`applied_s`), else — a kernel without
/// FUSE_REQUEST_TIMEOUT — the one this mount asked for: the lattice's
/// deadline binds the daemon either way.
pub fn stallAnswerNs(asked_s: u16, applied_s: u16) u64 {
    const t: u64 = if (applied_s != 0) applied_s else asked_s;
    return (t -| REQUEST_TIMEOUT_CHECK_S) * std.time.ns_per_s;
}

pub const max_request_timeout_path = "/proc/sys/fs/fuse/max_request_timeout";

/// The host's cap on every server's request timeout; null off-Linux or on a
/// kernel without the sysctl.
pub fn readMaxRequestTimeout(io: std.Io) ?u32 {
    if (builtin.os.tag != .linux) return null;
    var buf: [64]u8 = undefined;
    const text = std.Io.Dir.cwd().readFile(io, max_request_timeout_path, &buf) catch return null;
    return parseSysctlU32(text);
}

// ---------------------------------------------------------------------------
// Knob faces: the rows' ranges are enforced by the readers; the plan never
// refuses.
// ---------------------------------------------------------------------------

/// `Inputs` from the machine facts, the environment, the volume's block size
/// and the resolved memory budget (the cap through `sizing`).
pub fn resolveInputs(facts: derive.Facts, env: *const std.process.Environ.Map, block_size: u64, mem_budget: u64, max_pages_limit: ?u32, max_background_override: ?u16, congestion_override: ?u16) Inputs {
    return .{
        .kernel_nqueues = facts.possible_cpus,
        .env_queues = knobs.optIntKnob("SQUEEZEFS_TEST_FUSE_QUEUES", u32, env),
        .env_depth = knobs.optIntKnob("SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH", u32, env),
        .desired_max_write = desiredMaxWrite(block_size, knobs.optIntKnob("SQUEEZEFS_FUSE_MAX_WRITE", u32, env)),
        .max_pages_limit = max_pages_limit,
        .page_size = facts.page_size,
        .buffer_cap = sizing.transportBufferCapFromEnv(env, mem_budget),
        .max_background_override = max_background_override,
        .congestion_override = congestion_override,
    };
}

// ---------------------------------------------------------------------------
// Published gauges (LEVELS: zero until a session arms, stored once).
// ---------------------------------------------------------------------------

pub const Gauges = struct {
    /// `transport_queues`
    queues: counters.Gauge = .{},
    /// `transport_q_depth`
    q_depth: counters.Gauge = .{},
    /// `transport_payload_buffer_bytes` = queues × depth × payload_sz
    payload_buffer_bytes: counters.Gauge = .{},
    /// `transport_payload_buffers` = queues × depth
    payload_buffers: counters.Gauge = .{},
    /// `transport_max_background`
    max_background: counters.Gauge = .{},
    /// `transport_max_write`
    max_write: counters.Gauge = .{},
    /// `transport_max_pages`
    max_pages: counters.Gauge = .{},
    /// `transport_max_readahead`: the kernel's, echoed verbatim and never
    /// consulted (the prefetch window is independent of it).
    max_readahead: counters.Gauge = .{},
    /// `fuse_uring_rings_per_queue`: the rings (workers) registered on each
    /// kernel queue, 1 or `MAX_RINGS_PER_QUEUE`.
    rings_per_queue: counters.Gauge = .{},

    pub fn publish(self: *Gauges, p: Plan, max_readahead: u32) void {
        self.queues.set(p.nqueues);
        self.q_depth.set(p.depth);
        self.payload_buffer_bytes.set(p.totalPayloadBytes());
        self.payload_buffers.set(p.payloadBuffers());
        self.max_background.set(p.max_background);
        self.max_write.set(p.max_write);
        self.max_pages.set(p.max_pages);
        self.max_readahead.set(max_readahead);
    }

    pub fn publishRingsPerQueue(self: *Gauges, rings: u32) void {
        self.rings_per_queue.set(rings);
    }
};

/// The process-wide `transport_*` levels.
pub var gauges: Gauges = .{};

// ---------------------------------------------------------------------------
// Rings per queue (rung-10-workers-per-queue)
// ---------------------------------------------------------------------------

/// The most io_urings (each a `sqz-fuse-ur<qid>` worker with its own ring)
/// the transport registers on one kernel queue — `SQUEEZEFS_FUSE_RINGS_PER_QUEUE`'s
/// ceiling and what the executor reserves driven lanes and the pass census
/// stamps for; the derivation stays at `RINGS_PER_QUEUE_DERIVED`. 4 = the
/// A/B room the 4k randread QD32×4 row asked for (`rung-10-fuse-boundary.md`):
/// a cold 4 KiB read costs the worker ≈ 6.4 µs, so two workers per queue cap
/// one submitter at ≈ 310 k reads/s and the row at 1.25 M where fio alone
/// reaches 2–3 M against the boundary ruler; a third and fourth worker serve
/// a queue whose first two are saturated, which that row's are.
pub const MAX_RINGS_PER_QUEUE: u32 = 4;

/// The derived count: the workers ONE submitter keeps busy. The kernel picks a request's queue by
/// the submitting task's CPU (`task_cpu`), so one submitter's whole depth
/// lands on one queue, and its entry pick is LIFO over the queue's available
/// entries, so a later ring's entries are taken only once the earlier rings'
/// are all in userspace: an idle ring costs nothing but its thread. The
/// count is the measured ratio of the daemon's per-op cost to the
/// submitter's across the FUSE boundary — a 4 KiB write ≈ 7 µs of worker
/// against fio's ≈ 2.5 µs of issue and completion, a read 4.5 against 2.5
/// (`rung-10-write-iops.md` wave 2, `rung-10-read-view.md`) — rounded up:
/// 3. Measured with every held write on the worker's own lane: the third
/// ring took 4k randwrite QD32×4 from 1.15 M to 1.40 M at the two-ring
/// arm's cores and 4k randread QD32×4 from 1.74 M to 1.90 M (96 % of ext4,
/// fio-bound at 96 %); the fourth paid +7 % on the write row for +27 %
/// cores and gave back 5 % on the read row (its lock contention: fio and
/// four workers on one `queue->lock`) — the A/B room above. A static cores
/// ÷ queues cannot say it: the queues ARE the CPUs (charter §4), so the
/// ratio is 1 on every host; the two per-op costs scale together. Measured
/// on the 7.2-sqz kmbuf series (its patch 0030 made a ring per source); the
/// buffer pool needs no patch — the pool is user memory every ring of the
/// queue registers at the same index and the kernel keys a buffer by its
/// offset, not by a ring — and the per-op cost ratio is the same daemon
/// against the same submitter, so the count carries over, to be re-measured
/// on 7.3 (`rung-10-fuse-mainline-uapi.md`'s plan). Two rings became the
/// default 2026-10-19 once their three train reds were pinned
/// (`rung-10-workers-per-queue.md`: the shared-hot-dir row's by-design
/// wait; the kill matrix's umount2 racing the kernel's release of the
/// SIGKILLed daemon, waited out; the fsck C4 orphan — a ring record a kill
/// left between a promotion's commit and its ring release, judged at
/// dismount since `rung-10-dismount-corpse.md`).
pub const RINGS_PER_QUEUE_DERIVED: u32 = 3;

/// Explicit wins verbatim over the derivation (the registry's range is
/// 1..MAX_RINGS_PER_QUEUE).
pub fn resolveRingsPerQueue(explicit: ?u32, derived: u32) u32 {
    return explicit orelse derived;
}

/// One ring's share of a queue's depth: `base` is the queue-level slot index
/// its entry 0 carries (the session addresses a slot by `(qid, base + ent)`;
/// the kernel's `buf_index` stays the ring-local ent), `depth` its entry
/// count.
pub const RingShare = struct { base: u16, depth: u32 };

/// The split of `depth` over `rings` as HALVINGS: start from the whole
/// depth and, until there are `rings` shares, split the largest into
/// ceil/floor halves (the shape the measured 3-ring rows ran: chosen when
/// each ring pinned a power-of-two kmbuf region of its share; the pool
/// holds `depth` buffers per queue whatever the split). Ring 0 takes the
/// largest share (the kernel's LIFO entry pick fills it first: it is the
/// hot ring), every ring at least one entry while `depth ≥ rings` (a ring
/// with no entry would never REGISTER and the arm barrier would never
/// complete), the queue's depth unchanged by the count. Two rings: ring 0
/// ceil, ring 1 the rest, as before; 32 over three: 16 / 8 / 8; over four:
/// 8 / 8 / 8 / 8.
pub fn ringShare(depth: u32, rings: u32, ring: u32) RingShare {
    std.debug.assert(rings >= 1 and ring < rings and depth >= rings);
    var shares: [MAX_RINGS_PER_QUEUE]u32 = @splat(0);
    shares[0] = depth;
    var n: usize = 1;
    while (n < rings) : (n += 1) {
        // The list is kept sorted descending: the largest share is first.
        const e = shares[0];
        shares[0] = e - e / 2;
        shares[n] = e / 2;
        std.mem.sort(u32, shares[0 .. n + 1], {}, std.sort.desc(u32));
    }
    var base: u32 = 0;
    for (shares[0..ring]) |x| base += x;
    return .{ .base = @intCast(base), .depth = shares[ring] };
}

/// Per-ring SQ entries: `max(next_pow2(depth × (1 + bridge_pieces) + 8), 16)`
/// — every entry's REGISTER or commit plus the wake PollAdd, and under zero
/// copy every entry's bridge in up to `bridge_pieces` SQEs
/// (`zc.BRIDGE_PIECES_MAX`; 0 on a session without bridges); the CQ is
/// sized `sq × 2` by the ring binding. Clamped to the kernel's
/// `IORING_MAX_ENTRIES`.
pub fn sqEntries(depth: u32, bridge_pieces: u32) u32 {
    const want = derive.nextPow2(@as(u64, depth) * (1 + @as(u64, bridge_pieces)) + 8);
    return @intCast(@min(@max(want, 16), 32768)); // IORING_MAX_ENTRIES: the kernel's SQ ceiling
}

// ---------------------------------------------------------------------------
// Tie tests (fuse_abi_session.md §3; fuse_uring_transport.md §1.7, §3, §12).
// Every planned geometry satisfies `payload_sz == kernelRingMaxPayloadSz`.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn in32(desired: u32, limit: u32, cap: u64) Inputs {
    return .{ .kernel_nqueues = 32, .desired_max_write = desired, .max_pages_limit = limit, .page_size = 4096, .buffer_cap = cap };
}

fn expectMirror(p: Plan) !void {
    try testing.expectEqual(@as(u64, p.payload_sz), kernelRingMaxPayloadSz(p.max_pages_limit, p.max_write, p.max_pages, p.page));
    try testing.expectEqual(pagesFor(p.max_write, p.page, p.max_pages_limit), @as(u32, p.max_pages));
    try testing.expect(p.depth >= 1 and p.depth <= Q_DEPTH_DESIRED);
}

test "default shape is byte-identical: (32q, 1 MiB, limit 256, cap 2 GiB) → depth 32, payload 1 MiB, max_pages 256, mb 1024, ct 768, arena 1 GiB" {
    const p = plan(in32(1 * MiB, 256, 2 * GiB));
    try testing.expectEqual(@as(u32, 32), p.nqueues);
    try testing.expectEqual(Q_DEPTH_DESIRED, p.depth);
    try testing.expectEqual(@as(u32, 1 * MiB), p.max_write);
    try testing.expectEqual(@as(u32, 1 * MiB), p.payload_sz);
    try testing.expectEqual(@as(u16, 256), p.max_pages);
    try testing.expectEqual(@as(u16, 1024), p.max_background);
    try testing.expectEqual(@as(u16, 768), p.congestion_threshold);
    try testing.expectEqual(1 * GiB, p.totalPayloadBytes());
    try testing.expectEqual(@as(u64, 1024), p.payloadBuffers());
    try expectMirror(p);
    // The formula restated: depth = clamp(cap / (q × payload), 4, 32); mb = clamp(q × d, 64, 65535).
    try testing.expectEqual(derive.clamp(2 * GiB / (32 * MiB), 4, 32), @as(u64, p.depth));
    try testing.expectEqual(derive.clamp(32 * 32, 64, 65535), @as(u64, p.max_background));
    try testing.expectEqual(@as(u64, p.max_background) / 4 * 3, @as(u64, p.congestion_threshold));
}

test "raised sysctl: limit ∈ {512, 1024, 4096, 65535} with a 1 MiB desire keeps payload 1 MiB and max_pages 256 (the REGISTER bound holds)" {
    for ([_]u32{ 512, 1024, 4096, 65535 }) |limit| {
        const p = plan(in32(1 * MiB, limit, 2 * GiB));
        try testing.expectEqual(@as(u32, 1 * MiB), p.payload_sz);
        try testing.expectEqual(@as(u16, 256), p.max_pages);
        try testing.expectEqual(@as(u32, 1 * MiB), p.max_write);
        try expectMirror(p);
    }
}

test "4 MiB max_write is sysctl-gated: (4 MiB, limit 1024, 2 GiB) → mw 4 MiB, pages 1024, depth 16, mb 512, arena 2 GiB; (4 MiB, limit 256) → mw 1 MiB" {
    const p = plan(in32(4 * MiB, 1024, 2 * GiB));
    try testing.expectEqual(@as(u32, 4 * MiB), p.max_write);
    try testing.expectEqual(@as(u16, 1024), p.max_pages);
    try testing.expectEqual(@as(u32, 16), p.depth);
    try testing.expectEqual(@as(u16, 512), p.max_background);
    try testing.expectEqual(2 * GiB, p.totalPayloadBytes());
    try expectMirror(p);
    const q = plan(in32(4 * MiB, 256, 2 * GiB));
    try testing.expectEqual(@as(u32, 1 * MiB), q.max_write);
    try testing.expectEqual(@as(u16, 256), q.max_pages);
    try expectMirror(q);
}

test "a small sysctl stops the inflation: (1 MiB, limit 64) → mw 256 KiB, pages 64" {
    const p = plan(in32(1 * MiB, 64, 2 * GiB));
    try testing.expectEqual(@as(u32, 64 * 4096), p.max_write);
    try testing.expectEqual(@as(u16, 64), p.max_pages);
    try testing.expectEqual(@as(u32, 64 * 4096), p.payload_sz);
    try expectMirror(p);
}

test "budget ladder degrades payload before violating the depth floor: 819 MiB → depth 6 @ 4 MiB; 256 MiB → depth 4 @ 2 MiB/512 pages; 64 MiB and cap 0 → depth 4 @ 1 MiB" {
    const a = plan(in32(4 * MiB, 1024, 819 * MiB));
    try testing.expectEqual(@as(u32, 6), a.depth);
    try testing.expectEqual(@as(u32, 4 * MiB), a.max_write);
    try expectMirror(a);
    const b = plan(in32(4 * MiB, 1024, 256 * MiB));
    try testing.expectEqual(Q_DEPTH_FLOOR, b.depth);
    try testing.expectEqual(@as(u32, 2 * MiB), b.max_write);
    try testing.expectEqual(@as(u16, 512), b.max_pages);
    try testing.expect(b.totalPayloadBytes() <= 256 * MiB);
    try expectMirror(b);
    const c = plan(in32(4 * MiB, 1024, 64 * MiB));
    try testing.expectEqual(Q_DEPTH_FLOOR, c.depth);
    try testing.expectEqual(@as(u32, 1 * MiB), c.max_write);
    try expectMirror(c);
    const d = plan(in32(4 * MiB, 1024, 0));
    try testing.expectEqual(Q_DEPTH_FLOOR, d.depth);
    try testing.expectEqual(@as(u32, 1 * MiB), d.max_write);
    try expectMirror(d);
    // Never below the 1 MiB base: the floor arena at cap 0 is the shipped posture.
    try testing.expectEqual(32 * 4 * 1 * MiB, d.totalPayloadBytes());
}

test "a 4096 max_write does not inflate to 256 pages: (8q, 4096, limit 256, 32 MiB) → mw 4096, pages 1, payload 8192, depth 32" {
    const p = plan(.{ .kernel_nqueues = 8, .desired_max_write = 4096, .max_pages_limit = 256, .page_size = 4096, .buffer_cap = 32 * MiB });
    try testing.expectEqual(@as(u32, 4096), p.max_write);
    try testing.expectEqual(@as(u16, 1), p.max_pages);
    try testing.expectEqual(abi.FUSE_MIN_READ_BUFFER, p.payload_sz);
    try testing.expectEqual(Q_DEPTH_DESIRED, p.depth);
    try expectMirror(p);
}

test "an explicit depth bypasses the payload ladder: env depth 8 with cap 64 MiB → depth 8, mw 4 MiB; env 64 → 32; env 0 → 1" {
    var in = in32(4 * MiB, 1024, 64 * MiB);
    in.env_depth = 8;
    const p = plan(in);
    try testing.expectEqual(@as(u32, 8), p.depth);
    try testing.expectEqual(@as(u32, 4 * MiB), p.max_write);
    try expectMirror(p);
    in.env_depth = 64;
    try testing.expectEqual(Q_DEPTH_DESIRED, plan(in).depth);
    in.env_depth = 0;
    try testing.expectEqual(@as(u32, 1), plan(in).depth);
    // The row refuses both at the gate; the plan is total regardless.
    try testing.expectEqual(@as(i128, 1), knobs.row("SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH").kind.int.lo);
    try testing.expectEqual(@as(i128, 32), knobs.row("SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH").kind.int.hi);
}

test "the budget sweep of the measured class: 819 MiB → depth 25, mb 800; 128 MiB → depth 4, mb 128, ct 96; (4q, 179 MiB) → depth 32, mb 128; (256q, 2 GiB) → depth 8, mb 2048; (256q, 9.5 GiB) → depth 32, arena 8 GiB; (1q, depth 4) → mb 64, ct 48" {
    const a = plan(in32(1 * MiB, 256, 819 * MiB));
    try testing.expectEqual(@as(u32, 25), a.depth);
    try testing.expectEqual(@as(u16, 800), a.max_background);
    const b = plan(in32(1 * MiB, 256, 128 * MiB));
    try testing.expectEqual(@as(u32, 4), b.depth);
    try testing.expectEqual(@as(u16, 128), b.max_background);
    try testing.expectEqual(@as(u16, 96), b.congestion_threshold);
    const c = plan(.{ .kernel_nqueues = 4, .desired_max_write = 1 * MiB, .max_pages_limit = 256, .page_size = 4096, .buffer_cap = 179 * MiB });
    try testing.expectEqual(@as(u32, 32), c.depth);
    try testing.expectEqual(@as(u16, 128), c.max_background);
    const d = plan(.{ .kernel_nqueues = 256, .desired_max_write = 1 * MiB, .max_pages_limit = 256, .page_size = 4096, .buffer_cap = 2 * GiB });
    try testing.expectEqual(@as(u32, 8), d.depth);
    try testing.expectEqual(@as(u16, 2048), d.max_background);
    const e = plan(.{ .kernel_nqueues = 256, .desired_max_write = 1 * MiB, .max_pages_limit = 256, .page_size = 4096, .buffer_cap = 9 * GiB + 512 * MiB });
    try testing.expectEqual(@as(u32, 32), e.depth);
    try testing.expectEqual(8 * GiB, e.totalPayloadBytes());
    const f = plan(.{ .kernel_nqueues = 1, .desired_max_write = 1 * MiB, .max_pages_limit = 256, .page_size = 4096, .buffer_cap = 0 });
    try testing.expectEqual(@as(u32, 4), f.depth);
    try testing.expectEqual(@as(u16, 64), f.max_background);
    try testing.expectEqual(@as(u16, 48), f.congestion_threshold);
    for ([_]Plan{ a, b, c, d, e, f }) |p| try expectMirror(p);
}

test "max_background is bounded by the u16 wire field, never a fixed 256: (512q × 32) → 16384; (65535/32 + 1 queues…) saturates at 65535 only through the clamp" {
    const p = plan(.{ .kernel_nqueues = 512, .desired_max_write = 1 * MiB, .max_pages_limit = 256, .page_size = 4096, .buffer_cap = 1 << 40 });
    try testing.expectEqual(@as(u16, 16384), p.max_background);
    try testing.expectEqual(@as(u32, 65535), policyMaxBackground(4096, 32));
    try testing.expectEqual(@as(u32, 64), policyMaxBackground(1, 1));
    try testing.expectEqual(MAX_BACKGROUND_CEILING, @as(u32, std.math.maxInt(u16)));
}

test "queue count: env wins (clamped 1..=512 — the row refuses outside), else the kernel's possible CPUs; 4096 → 512, 0 → 1" {
    var in = in32(1 * MiB, 256, 2 * GiB);
    try testing.expectEqual(@as(u32, 32), plan(in).nqueues);
    in.env_queues = 4096;
    try testing.expectEqual(@as(u32, 512), plan(in).nqueues);
    in.env_queues = 0;
    try testing.expectEqual(@as(u32, 1), plan(in).nqueues);
    in.env_queues = 8;
    try testing.expectEqual(@as(u32, 8), plan(in).nqueues);
    try testing.expectEqual(@as(i128, 512), knobs.row("SQUEEZEFS_TEST_FUSE_QUEUES").kind.int.hi);
}

test "mount-option overrides: mb/ct 96/80 verbatim; mb 100 alone → ct 75; 0/0 → policy" {
    var in = in32(1 * MiB, 256, 2 * GiB);
    in.max_background_override = 96;
    in.congestion_override = 80;
    var p = plan(in);
    try testing.expectEqual(@as(u16, 96), p.max_background);
    try testing.expectEqual(@as(u16, 80), p.congestion_threshold);
    in.congestion_override = null;
    in.max_background_override = 100;
    p = plan(in);
    try testing.expectEqual(@as(u16, 100), p.max_background);
    try testing.expectEqual(@as(u16, 75), p.congestion_threshold);
    in.max_background_override = 0;
    in.congestion_override = 0;
    p = plan(in);
    try testing.expectEqual(@as(u16, 1024), p.max_background);
    try testing.expectEqual(@as(u16, 768), p.congestion_threshold);
}

test "request timeout (decision 0003 §Z): T_owner + T_park_max in whole seconds rounded up — 135 s at the lattice's constants (45 s + 45 s failover + 45 s grace); the field saturates; the seam wins; the kernel caps by its sysctl (0 = none) and floors at its 15 s check period" {
    try testing.expectEqual(@as(u16, 135), requestTimeoutS(45_000, 45_000 + 45_000));
    try testing.expectEqual(@as(u16, 136), requestTimeoutS(45_000, 90_001));
    try testing.expectEqual(@as(u16, 1), requestTimeoutS(0, 1));
    try testing.expectEqual(@as(u16, 0), requestTimeoutS(0, 0));
    try testing.expectEqual(@as(u16, std.math.maxInt(u16)), requestTimeoutS(std.math.maxInt(u64), 1));
    try testing.expectEqual(@as(u16, 15), resolveRequestTimeoutS(15, 135));
    try testing.expectEqual(@as(u16, 135), resolveRequestTimeoutS(null, 135));
    try testing.expectEqual(@as(u16, 135), kernelRequestTimeoutS(135, null));
    try testing.expectEqual(@as(u16, 135), kernelRequestTimeoutS(135, 0));
    try testing.expectEqual(@as(u16, 60), kernelRequestTimeoutS(135, 60));
    try testing.expectEqual(@as(u16, 135), kernelRequestTimeoutS(135, 1000));
    try testing.expectEqual(REQUEST_TIMEOUT_CHECK_S, kernelRequestTimeoutS(135, 5));
    try testing.expectEqual(REQUEST_TIMEOUT_CHECK_S, kernelRequestTimeoutS(1, null));
    try testing.expectEqual(@as(u16, 15), REQUEST_TIMEOUT_CHECK_S);
}

test "the stall answer's bound (decision 0003 §BQ): the applied request timeout less one check period — 120 s at the lattice's 135 s; the asked one when the kernel applies none; a 15 s timeout (the kernel's floor) answers at once" {
    // T − REQUEST_TIMEOUT_CHECK_S, T = applied else asked.
    try testing.expectEqual(@as(u64, 120 * std.time.ns_per_s), stallAnswerNs(135, 135));
    try testing.expectEqual(@as(u64, 45 * std.time.ns_per_s), stallAnswerNs(135, 60));
    try testing.expectEqual(@as(u64, 120 * std.time.ns_per_s), stallAnswerNs(135, 0));
    try testing.expectEqual(@as(u64, 15 * std.time.ns_per_s), stallAnswerNs(30, 30));
    try testing.expectEqual(@as(u64, 0), stallAnswerNs(135, REQUEST_TIMEOUT_CHECK_S));
    try testing.expectEqual(@as(u64, 0), stallAnswerNs(0, 0));
    for ([_]u16{ 15, 16, 30, 135, 1000, std.math.maxInt(u16) }) |t| {
        try testing.expectEqual(@as(u64, t - REQUEST_TIMEOUT_CHECK_S) * std.time.ns_per_s, stallAnswerNs(135, kernelRequestTimeoutS(t, null)));
    }
}

test "page and sysctl inputs: a sub-512 page is floored, the sysctl fallback is 256 and clamps to [1, 65535]; the text parser is total" {
    try testing.expectEqual(@as(u32, 256), pagesLimit(null));
    try testing.expectEqual(@as(u32, 1), pagesLimit(0));
    try testing.expectEqual(@as(u32, 65535), pagesLimit(1 << 20));
    try testing.expectEqual(@as(u32, 1024), pagesLimit(1024));
    try testing.expectEqual(@as(?u32, 256), parseSysctlU32("256\n"));
    try testing.expectEqual(@as(?u32, 65535), parseSysctlU32(" 65535 "));
    try testing.expectEqual(@as(?u32, null), parseSysctlU32(""));
    try testing.expectEqual(@as(?u32, null), parseSysctlU32("lots"));
    try testing.expectEqual(@as(?u32, null), parseSysctlU32("-1"));
    const p = plan(.{ .kernel_nqueues = 8, .desired_max_write = 1 * MiB, .max_pages_limit = null, .page_size = 64, .buffer_cap = 1 * GiB });
    try testing.expectEqual(MIN_PAGE, p.page);
    try testing.expectEqual(@as(u32, 256), p.max_pages_limit);
    // With a 512 B page the 256-page limit caps max_write at 128 KiB.
    try testing.expectEqual(@as(u32, 256 * 512), p.max_write);
    try expectMirror(p);
    // The kernel mirror stated directly.
    try testing.expectEqual(@as(u64, 1 * MiB), kernelRingMaxPayloadSz(256, 1 * MiB, 256, 4096));
    try testing.expectEqual(@as(u64, 8192), kernelRingMaxPayloadSz(256, 4096, 1, 4096));
    try testing.expectEqual(@as(u64, 1 * MiB), kernelRingMaxPayloadSz(256, 4096, 65535, 4096));
}

test "desired max_write: explicit verbatim, else max(block, 1 MiB) in [4096, u32::MAX] (a 64 KiB block keeps the 1 MiB floor; a 4 MiB block asks 4 MiB)" {
    try testing.expectEqual(@as(u32, 1 * MiB), desiredMaxWrite(64 * 1024, null));
    try testing.expectEqual(@as(u32, 4 * MiB), desiredMaxWrite(4 * MiB, null));
    try testing.expectEqual(@as(u32, 2 * MiB), desiredMaxWrite(4 * MiB, 2 * MiB));
    try testing.expectEqual(@as(u32, 4096), desiredMaxWrite(0, 4096));
    try testing.expectEqual(@as(u32, std.math.maxInt(u32)), desiredMaxWrite(1 << 40, null));
    try testing.expectEqual(@as(i128, 4096), knobs.row("SQUEEZEFS_FUSE_MAX_WRITE").kind.int.lo);
}

test "the venue's shape (9 possible CPUs, 16 GiB RAM, limit 256): budget 70 % RAM, cap budget/8, depth 32, mb 288, ct 216, payload 1 MiB, arena 288 MiB; the 8-CPU form mb 256/ct 192" {
    const ram: u64 = 16_491_296 * 1024;
    const budget = derive.tenths(ram, 7);
    const cap = sizing.transportBufferCap(budget);
    const p = plan(.{ .kernel_nqueues = 9, .desired_max_write = desiredMaxWrite(64 * 1024, null), .max_pages_limit = 256, .page_size = 4096, .buffer_cap = cap });
    try testing.expectEqual(@as(u32, 9), p.nqueues);
    try testing.expectEqual(Q_DEPTH_DESIRED, p.depth);
    try testing.expectEqual(@as(u32, 1 * MiB), p.max_write);
    try testing.expectEqual(@as(u16, 256), p.max_pages);
    try testing.expectEqual(@as(u32, 1 * MiB), p.payload_sz);
    try testing.expectEqual(@as(u16, 288), p.max_background);
    try testing.expectEqual(@as(u16, 216), p.congestion_threshold);
    try testing.expectEqual(288 * MiB, p.totalPayloadBytes());
    try expectMirror(p);
    const q = plan(.{ .kernel_nqueues = 8, .desired_max_write = 1 * MiB, .max_pages_limit = 256, .page_size = 4096, .buffer_cap = sizing.transportBufferCap(derive.tenths(16 * GiB, 7)) });
    try testing.expectEqual(@as(u16, 256), q.max_background);
    try testing.expectEqual(@as(u16, 192), q.congestion_threshold);
    try testing.expectEqual(256 * MiB, q.totalPayloadBytes());
}

test "knob faces: Inputs read the rows through the registry; absent derives; the cap follows SQUEEZEFS_TRANSPORT_MEM_MAX/PCT" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    const facts = derive.Facts.synthetic(32, 64 * GiB);
    var in = resolveInputs(facts, &env, 64 * 1024, 8 * GiB, 256, null, null);
    try testing.expectEqual(@as(u32, 32), in.kernel_nqueues);
    try testing.expectEqual(@as(?u32, null), in.env_queues);
    try testing.expectEqual(@as(?u32, null), in.env_depth);
    try testing.expectEqual(@as(u32, 1 * MiB), in.desired_max_write);
    try testing.expectEqual(1 * GiB, in.buffer_cap);
    try env.put("SQUEEZEFS_TEST_FUSE_QUEUES", "4");
    try env.put("SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH", "8");
    try env.put("SQUEEZEFS_FUSE_MAX_WRITE", "2097152");
    try env.put("SQUEEZEFS_TRANSPORT_MEM_PCT", "50");
    in = resolveInputs(facts, &env, 64 * 1024, 8 * GiB, 1024, 96, 80);
    try testing.expectEqual(@as(?u32, 4), in.env_queues);
    try testing.expectEqual(@as(?u32, 8), in.env_depth);
    try testing.expectEqual(@as(u32, 2 * MiB), in.desired_max_write);
    try testing.expectEqual(4 * GiB, in.buffer_cap);
    try testing.expectEqual(@as(?u16, 96), in.max_background_override);
    const p = plan(in);
    try testing.expectEqual(@as(u32, 4), p.nqueues);
    try testing.expectEqual(@as(u32, 8), p.depth);
    try testing.expectEqual(@as(u32, 2 * MiB), p.max_write);
    try testing.expectEqual(@as(u16, 96), p.max_background);
}

test "gauges publish the armed plan as levels (zero before) and the rings per queue" {
    var g: Gauges = .{};
    try testing.expectEqual(@as(u64, 0), g.queues.load());
    const p = plan(in32(1 * MiB, 256, 2 * GiB));
    g.publish(p, 131072);
    try testing.expectEqual(@as(u64, 32), g.queues.load());
    try testing.expectEqual(@as(u64, 32), g.q_depth.load());
    try testing.expectEqual(1 * GiB, g.payload_buffer_bytes.load());
    try testing.expectEqual(@as(u64, 1024), g.payload_buffers.load());
    try testing.expectEqual(@as(u64, 1024), g.max_background.load());
    try testing.expectEqual(@as(u64, 1 * MiB), g.max_write.load());
    try testing.expectEqual(@as(u64, 256), g.max_pages.load());
    try testing.expectEqual(@as(u64, 131072), g.max_readahead.load());
    g.publishRingsPerQueue(RINGS_PER_QUEUE_DERIVED);
    try testing.expectEqual(@as(u64, RINGS_PER_QUEUE_DERIVED), g.rings_per_queue.load());
}

test "sq entries = max(next_pow2(depth × (1 + pieces) + 8), 16), at most the kernel's 32768" {
    // No bridges (pieces 0): the register/commit shape alone.
    try testing.expectEqual(@as(u32, 64), sqEntries(32, 0));
    try testing.expectEqual(@as(u32, 16), sqEntries(4, 0));
    try testing.expectEqual(@as(u32, 16), sqEntries(8, 0));
    try testing.expectEqual(@as(u32, 32), sqEntries(9, 0));
    try testing.expectEqual(@as(u32, 16), sqEntries(1, 0));
    // A zero-copy ring: every ent's bridge in up to 8 pieces beside its commit (16 × 9 + 8 = 152 → 256).
    try testing.expectEqual(@as(u32, 256), sqEntries(16, 8));
    try testing.expectEqual(@as(u32, 512), sqEntries(32, 8));
    // The kernel's ceiling holds at any depth.
    try testing.expectEqual(@as(u32, 32768), sqEntries(1 << 13, 8));
}

test "rings per queue: the derived 3 (the workers one submitter keeps busy: the daemon's ≈ 7 µs per op over the submitter's ≈ 2.5, rounded up); explicit wins up to the ceiling of 4; the shares split the depth by halvings with the queue's depth unchanged, ring 0 the largest, the bases contiguous" {
    try testing.expectEqual(@as(u32, 3), RINGS_PER_QUEUE_DERIVED);
    try testing.expectEqual(@as(u32, 4), MAX_RINGS_PER_QUEUE);
    try testing.expect(RINGS_PER_QUEUE_DERIVED <= MAX_RINGS_PER_QUEUE);
    try testing.expectEqual(@as(u32, 1), resolveRingsPerQueue(1, 2));
    try testing.expectEqual(@as(u32, 2), resolveRingsPerQueue(2, 1));
    try testing.expectEqual(@as(u32, 2), resolveRingsPerQueue(null, 2));
    for ([_]u32{ 4, 5, 9, 16, 17, 31, 32 }) |depth| {
        const r0 = ringShare(depth, 2, 0);
        const r1 = ringShare(depth, 2, 1);
        try testing.expectEqual(@as(u16, 0), r0.base);
        try testing.expectEqual(@as(u32, @intCast(derive.ceilDiv(depth, 2))), r0.depth);
        try testing.expectEqual(@as(u16, @intCast(r0.depth)), r1.base);
        try testing.expectEqual(depth, r0.depth + r1.depth);
        try testing.expect(r1.depth >= 1 and r1.depth <= r0.depth);
        const whole = ringShare(depth, 1, 0);
        try testing.expectEqual(@as(u16, 0), whole.base);
        try testing.expectEqual(depth, whole.depth);
    }
    try testing.expectEqual(RingShare{ .base = 16, .depth = 16 }, ringShare(32, 2, 1));
    try testing.expectEqual(RingShare{ .base = 2, .depth = 2 }, ringShare(4, 2, 1));
    try testing.expectEqual(RingShare{ .base = 3, .depth = 2 }, ringShare(5, 2, 1));
    // Three and four rings: halvings of the largest — 32 is 16/8/8 and 8/8/8/8.
    try testing.expectEqual(RingShare{ .base = 0, .depth = 16 }, ringShare(32, 3, 0));
    try testing.expectEqual(RingShare{ .base = 16, .depth = 8 }, ringShare(32, 3, 1));
    try testing.expectEqual(RingShare{ .base = 24, .depth = 8 }, ringShare(32, 3, 2));
    try testing.expectEqual(RingShare{ .base = 24, .depth = 8 }, ringShare(32, 4, 3));
    for ([_]u32{ 3, 4, 5, 9, 10, 12, 16, 17, 31, 32 }) |depth| {
        for ([_]u32{ 3, 4 }) |rings| {
            if (depth < rings) continue;
            var sum: u32 = 0;
            var prev: u32 = std.math.maxInt(u32);
            for (0..rings) |ring| {
                const s = ringShare(depth, rings, @intCast(ring));
                try testing.expectEqual(@as(u16, @intCast(sum)), s.base);
                try testing.expect(s.depth >= 1 and s.depth <= prev);
                prev = s.depth;
                sum += s.depth;
            }
            try testing.expectEqual(depth, sum);
        }
    }
}

test "the plan is total over a sweep of shapes and every result satisfies the kernel mirror" {
    var prng = std.Random.DefaultPrng.init(0x6E0);
    const r = prng.random();
    for (0..5000) |_| {
        const in: Inputs = .{
            .kernel_nqueues = r.intRangeAtMost(u32, 0, 1024),
            .env_queues = if (r.boolean()) r.intRangeAtMost(u32, 0, 1024) else null,
            .env_depth = if (r.boolean()) r.intRangeAtMost(u32, 0, 64) else null,
            .desired_max_write = r.intRangeAtMost(u32, 0, std.math.maxInt(u32)),
            .max_pages_limit = if (r.boolean()) r.intRangeAtMost(u32, 0, 1 << 20) else null,
            .page_size = @as(u32, 1) << r.intRangeAtMost(u5, 0, 16),
            .buffer_cap = r.intRangeAtMost(u64, 0, 1 << 44),
            .max_background_override = if (r.boolean()) r.int(u16) else null,
            .congestion_override = if (r.boolean()) r.int(u16) else null,
        };
        const p = plan(in);
        try expectMirror(p);
        try testing.expect(p.nqueues >= 1 and p.nqueues <= MAX_QUEUES);
        try testing.expect(p.max_write >= MIN_MAX_WRITE);
        try testing.expect(p.payload_sz >= abi.FUSE_MIN_READ_BUFFER);
        try testing.expect(p.payload_sz >= p.max_write);
        if (in.env_depth == null) try testing.expect(p.depth >= Q_DEPTH_FLOOR);
        if ((in.max_background_override orelse 0) == 0) try testing.expect(p.max_background >= MAX_BACKGROUND_FLOOR);
        if ((in.congestion_override orelse 0) == 0) try testing.expectEqual(@as(u16, p.max_background / 4 * 3), p.congestion_threshold);
    }
}

fn maxPagesFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [64]u8 = undefined;
    const len = smith.slice(&buf);
    _ = parseSysctlU32(buf[0..len]);
}

test "fuzz: the max_pages_limit sysfs parser is total on arbitrary text" {
    try fuzzkit.fuzz({}, maxPagesFuzz, .{ .corpus = &.{ fuzzkit.slice("256\n"), fuzzkit.slice("99999999999999999999") } });
}
