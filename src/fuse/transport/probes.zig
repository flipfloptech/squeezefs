//! The PROBE ladders and the buffer-mode lattice decision (charter §3
//! "Capability discipline: every kernel feature is PROBE-detected at
//! runtime … never by kernel version. Post-probe refusals fail the mount
//! loud; the probe is the only degrade point"; `docs/porting/
//! fuse_mainline_pools.md` §2, `fuse_kmbuf_zc.md` §2, §6).
//!
//! Each ladder is a PURE state machine over injected syscall results —
//! unit-tested with every verdict on every host — plus a Linux driver that
//! runs the real syscalls on a scratch ring and feeds the machine. The
//! lattice (`resolveBufferMode`) is a pure decision table over the verdicts
//! and the `SQUEEZEFS_FUSE_BUFFER_MODE` lever: `auto` takes the highest rung
//! the probes confirm; an explicit rung the probes refuse is a typed error
//! the session turns into a loud mount failure naming the probe. Never a
//! downgrade (decision 0002 ledger: the reference's doc comment promised one
//! its code did not do).
//!
//! Three strata since 2026-10-21 (charter §4 amended): the MAINLINE stratum
//! (Linux ≥ 7.3: the INIT offer carries `FUSE_HAS_IO_URING_BUFPOOL` — the
//! bit IS the probe, like TIME_LIMITS; the daemon writes `fuse.enable_uring=Y`
//! BEFORE `mount(2)` so the offer is complete on the first mount after boot
//! too), the LEGACY stratum (the 7.2-sqz kmbuf series: the opcode ladder
//! reads present) and the STOCK stratum (neither). The opcode COLLISION
//! (legacy `RELEASE_PAYLOAD`/`FEATURES` = 3/4 = mainline `ADD_QUEUE`/
//! `ADD_BUFPOOL`) is resolved HERE: the legacy command probes
//! (`legacyProbesApply`) run only on a legacy rung, so no mainline kernel
//! ever sees opcode 3 with the legacy meaning (it would create queue 0
//! without the zero-copy flag), and no legacy kernel ever sees opcode 3 with
//! the mainline meaning. A kernel offering bit 43 resolves mainline whatever
//! the ladder says (the two UAPIs cannot coexist in one `dev_uring.c`).
//!
//! What differs from the reference (0002 ledger rows): the zc capability
//! is `CapEff` bit 21 from `/proc/self/status`, not `euid == 0` (a root
//! daemon with a dropped bounding set would resolve ZeroCopy and die at
//! REGISTER); the venue's opcode pair (38/39) is probed FIRST (both orders
//! are correct by the conjunction argument; the order is a deployment
//! fact); mode resolution is pure and the caller logs.

const std = @import("std");
const testkit = @import("../../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const abi = @import("../abi.zig");
const geometry = @import("../geometry.zig");
const counters = @import("../../core/counters.zig");
const errors = @import("../../core/errors.zig");
const log = @import("../../core/log.zig");
const knobs = @import("../../core/knobs.zig");

const is_linux = builtin.os.tag == .linux;
const uring128 = if (is_linux) @import("../../io/uring128.zig") else struct {};

// ---------------------------------------------------------------------------
// Constants with their reasons.
// ---------------------------------------------------------------------------

/// The kmbuf probe's registration: a tiny VALID registration (one page per
/// buffer, 8 entries, a bgid FUSE never uses) whose side effects die with
/// the scratch ring's fd. `buf_size = page` keeps the low 12 bits zero, so
/// a foreign occupant that reads the first u16 as a command type
/// (`io_uring_bpf`'s `cmd_type == 1` import on 7.1) never matches.
pub const PROBE_ENTRIES: u32 = 8;
pub const PROBE_BGID: u16 = 7;
/// The scratch ring's SQ: one SQE in flight.
pub const SCRATCH_RING_ENTRIES: u16 = 8;
/// The retention probe's wait: a parked probe cmd is ambiguous ⇒ Absent
/// fail-safe; one second is far past any live kernel's answer (measured
/// ×5 on both sqz tracks: immediate -ENOTCONN / -ENOENT).
pub const RETENTION_PROBE_WAIT_NS: u64 = 1 * std.time.ns_per_s;
/// `CAP_SYS_ADMIN` in the `CapEff` word.
pub const CAP_SYS_ADMIN_BIT: u6 = 21;
/// `CAP_IPC_LOCK` in the `CapEff` word: without it io_uring charges its
/// regions (the kmbuf buffers included) to `RLIMIT_MEMLOCK`.
pub const CAP_IPC_LOCK_BIT: u6 = 14;

/// The opcode pairs the ladder walks, the venue's track first (0002 ledger
/// row: the order is a deployment fact; the conjunction makes either order
/// correct).
pub const ladder = [_]abi.KmbufRegisterOpcodes{ abi.KMBUF_OPCODES_7_1, abi.KMBUF_OPCODES_6_19 };

// ---------------------------------------------------------------------------
// Gauges: the published verdicts (0 = absent / not probed).
// ---------------------------------------------------------------------------

/// `transport_buffer_mode`: 0 UserEnts, 1 BufRing, 2 ZeroCopy, 3 MainlinePool.
pub var mode_gauge: counters.Gauge = .{};
/// `transport_probe_kmbuf`: the confirmed REGISTER opcode (38 or 37), 0 absent.
pub var kmbuf_gauge: counters.Gauge = .{};
/// `transport_probe_zc`: 1 when the zc surface is present (legacy: kmbuf ∧
/// cap ∧ sparse; mainline: bufpool ∧ cap ∧ sparse — the pool queues are
/// created zero-copy).
pub var zc_gauge: counters.Gauge = .{};
/// `transport_probe_bufpool`: 1 when INIT offered `FUSE_HAS_IO_URING_BUFPOOL`.
pub var bufpool_gauge: counters.Gauge = .{};
/// `transport_probe_retention`: 1 when RELEASE_PAYLOAD answered the probe shape.
pub var retention_gauge: counters.Gauge = .{};
/// `transport_probe_time_limits`: 1 when the kernel offered bit 62.
pub var time_limits_gauge: counters.Gauge = .{};
/// `transport_probe_cap_sys_admin`: 1 when CapEff bit 21 is set.
pub var cap_gauge: counters.Gauge = .{};
/// `transport_probe_rings_per_queue`: 1 when FEATURES answered with
/// `FUSE_URING_FEAT_RINGS_PER_QUEUE` (series patch 0030).
pub var rings_per_queue_gauge: counters.Gauge = .{};

// ---------------------------------------------------------------------------
// 1. The kmbuf opcode ladder (porting spec §2.1)
// ---------------------------------------------------------------------------

/// One rung's injected syscall results, in step order.
pub const RungSteps = struct {
    /// Step 1: `io_uring_register(pair.register, buf_reg)`: 0 or `-errno`.
    register: i32,
    /// Step 2: the identical register again (expected `-EEXIST`).
    repeat: i32 = 0,
    /// Step 3: the region mmap at `IORING_OFF_KMBUF_RING | (bgid << 16)`.
    mmap_ok: bool = false,
};

pub const RungVerdict = union(enum) {
    confirmed,
    /// `quiet`: EINVAL (stock dispatch / a foreign import) or ENOENT (the
    /// kmbuf UNREGISTER crossed on the other track); any other errno is loud.
    refused: struct { errno: linux.E, quiet: bool },
    /// Register succeeded but the signature did not complete: a foreign
    /// opcode accepted our bytes. Never arms; falls through.
    foreign_success,
};

fn errnoOf(res: i32) linux.E {
    if (res < 0 and res > -4096) return @fromBackingInt(@as(u16, @intCast(-res)));
    return .SUCCESS;
}

/// The rung's state machine: register 0 ∧ repeat EEXIST ∧ mmap ok ⇒
/// confirmed. A false Present is unreachable (the conjunction); a false
/// Absent is unreachable on the sqz tracks because the rung runs the exact
/// registration the queue setup performs.
pub fn rungVerdict(s: RungSteps) RungVerdict {
    if (s.register != 0) {
        const e = errnoOf(s.register);
        return .{ .refused = .{ .errno = e, .quiet = e == .INVAL or e == .NOENT } };
    }
    if (errnoOf(s.repeat) != .EXIST) return .foreign_success;
    if (!s.mmap_ok) return .foreign_success;
    return .confirmed;
}

pub const KmbufVerdict = union(enum) {
    present: abi.KmbufRegisterOpcodes,
    absent,

    pub fn opcodes(self: KmbufVerdict) ?abi.KmbufRegisterOpcodes {
        return switch (self) {
            .present => |p| p,
            .absent => null,
        };
    }
};

/// The ladder over injected rung verdicts: the first `confirmed` wins and
/// short-circuits; every other outcome falls through; exhaustion ⇒ absent.
pub fn kmbufLadder(verdicts: []const RungVerdict) KmbufVerdict {
    std.debug.assert(verdicts.len <= ladder.len);
    for (verdicts, 0..) |v, i| if (v == .confirmed) return .{ .present = ladder[i] };
    return .absent;
}

// ---------------------------------------------------------------------------
// 2. The retention probe (porting spec §2.3)
// ---------------------------------------------------------------------------

pub const Verdict = enum(u8) {
    absent = 0,
    present = 1,

    pub fn gauge(self: Verdict) u64 {
        return @backingInt(self);
    }
};

/// `RELEASE_PAYLOAD` with `commit_id = u64::MAX` on qid 0 AFTER the INIT
/// reply and BEFORE any REGISTER: `-ENOTCONN` (the opcode exists; queue 0
/// is not yet created — the pre-REGISTER shape) or `-ENOENT` (armed, id
/// unknown) ⇒ present; `-EINVAL` / `-EOPNOTSUPP` (the opcode switch's
/// default) ⇒ absent; 0, a positive result, any foreign errno, or no
/// answer within the wait (null) ⇒ absent fail-safe (an old kernel would
/// ACCEPT the retention init bit and silently ignore it — the worst class —
/// so the init bit itself can never be the probe).
pub fn retentionVerdict(res: ?i32) Verdict {
    const r = res orelse return .absent;
    return switch (errnoOf(r)) {
        .NOTCONN, .NOENT => .present,
        else => .absent,
    };
}

// ---------------------------------------------------------------------------
// 2b. The FEATURES probe (series patch 0030)
// ---------------------------------------------------------------------------

/// `FEATURES` (cmd_op 4) on the plain fuse fd after the INIT reply: a
/// non-negative result IS the `FUSE_URING_FEAT_*` mask; `-EINVAL` /
/// `-EOPNOTSUPP` (the opcode switch's default on a kernel without the
/// patch) ⇒ no features; any other errno or no answer within the wait ⇒
/// none, fail-safe (a feature read present when it is not would register
/// a second ring on a queue whose kernel recycles buffers into the wrong
/// ring — the stall class the patch exists to prevent).
pub fn featuresMask(res: ?i32) u32 {
    const r = res orelse return 0;
    if (r < 0) return 0;
    return @intCast(r);
}

/// Several rings per queue: the mask's bit.
pub fn ringsPerQueueVerdict(mask: u32) Verdict {
    return if (mask & abi.FUSE_URING_FEAT_RINGS_PER_QUEUE != 0) .present else .absent;
}

// ---------------------------------------------------------------------------
// 3. The zero-copy surface (porting spec §2.2, fixed per the quirk ledger)
// ---------------------------------------------------------------------------

/// The zc surface is a conjunction: a payload carrier the kernel zero-copies
/// beside (the legacy kmbuf ring — zc never rides without BUF_RING — or the
/// mainline buffer pool) ∧ the kernel's gate (`capable(CAP_SYS_ADMIN)` —
/// read from CapEff, never `euid`) ∧ a sparse fixed-buffer table registers
/// (5.19+: the per-ent slot shape the kernel fills at delivery).
pub fn zcVerdict(kmbuf: KmbufVerdict, bufpool: Verdict, cap_sys_admin: bool, sparse_ok: bool) Verdict {
    if (kmbuf == .absent and bufpool == .absent) return .absent;
    if (!cap_sys_admin) return .absent;
    if (!sparse_ok) return .absent;
    return .present;
}

// ---------------------------------------------------------------------------
// 3b. The mainline buffer-pool stratum — the INIT bit IS the probe
// ---------------------------------------------------------------------------

/// `FUSE_HAS_IO_URING_BUFPOOL` (bit 43) in the INIT offer: the kernel speaks
/// the mainline pool UAPI. The kernel sets it beside FUSE_OVER_IO_URING iff
/// `fuse.enable_uring` read Y when it SENT the INIT; the session writes Y
/// before `mount(2)`, so the offer is complete on every mount (the first
/// after boot included). No command probe exists that is side-effect free on
/// both strata: opcode 3 is ADD_QUEUE there (it would create queue 0) and
/// RELEASE_PAYLOAD on the legacy kernel.
pub fn bufpoolVerdict(offered_flags: u64) Verdict {
    return if (offered_flags & abi.FUSE_HAS_IO_URING_BUFPOOL != 0) .present else .absent;
}

/// Will the connection HAVE its ring when the INIT reply lands — the zc
/// surface's precondition? Since 7.2.4 the kernel creates the ring at
/// reply time iff `fuse.enable_uring` reads Y then, and the daemon writes
/// Y itself before replying (charter §3). The INIT request's
/// FUSE_OVER_IO_URING offer is the parameter's value when the kernel SENT
/// it: N on the first mount after every boot (the default), although that
/// mount arms its ring all the same — gating zc on the offer silently put
/// that mount on the slower buf_ring rung (`docs/evidence/rung-10-dev-
/// perf.md` §3.1: rand-4k 17.4 k IOPS instead of 58 k). The parameter as
/// read AFTER the daemon's write is the verdict; unreadable (a kernel
/// without FUSE_IO_URING exposes no parameter) falls back to the offer.
pub fn ringAtReply(offered_flags: u64, enable_uring_after_write: ?bool) bool {
    if (enable_uring_after_write) |on| return on;
    return offered_flags & abi.FUSE_OVER_IO_URING != 0;
}

/// `CapEff` from a `/proc/self/status` text: bit 21.
pub fn capSysAdminFromStatus(text: []const u8) ?bool {
    return capBitFromStatus(text, CAP_SYS_ADMIN_BIT);
}

/// One `CapEff` bit from a `/proc/self/status` text (null = no readable word).
pub fn capBitFromStatus(text: []const u8, bit: u6) ?bool {
    var it = std.mem.splitScalar(u8, text, '\n');
    while (it.next()) |line| {
        if (!std.mem.startsWith(u8, line, "CapEff:")) continue;
        const hex = std.mem.trim(u8, line["CapEff:".len..], " \t\r");
        const caps = std.fmt.parseInt(u64, hex, 16) catch return null;
        return caps & (@as(u64, 1) << bit) != 0;
    }
    return null;
}

// ---------------------------------------------------------------------------
// 4. FUSE_TIME_LIMITS — the init-flag bit IS the probe (porting spec §2.4)
// ---------------------------------------------------------------------------

pub fn timeLimitsVerdict(offered_flags: u64) Verdict {
    return if (offered_flags & abi.FUSE_TIME_LIMITS != 0) .present else .absent;
}

// ---------------------------------------------------------------------------
// The lattice decision (porting spec §6, as a pure table)
// ---------------------------------------------------------------------------

pub const Lever = enum { auto, user, bufring, zc, pool };

pub fn leverFromKnob(s: []const u8) Lever {
    return std.meta.stringToEnum(Lever, s) orelse .auto;
}

pub const Verdicts = struct {
    kmbuf: KmbufVerdict,
    /// The mainline pool UAPI (INIT bit 43).
    bufpool: Verdict = .absent,
    /// The zero-copy surface over whichever carrier is present.
    zc: Verdict,
};

pub const ResolveError = error{
    /// `bufring` or `zc` asked while the kmbuf ladder read absent.
    KmbufAbsent,
    /// `zc` asked while the zc surface read absent (no cap, or no sparse table).
    ZeroCopyAbsent,
    /// `pool` asked while INIT did not offer `FUSE_HAS_IO_URING_BUFPOOL`.
    BufpoolAbsent,
};

/// `auto` ⇒ the highest confirmed rung — the mainline pool wherever the
/// kernel offers it (the primary contract), else the legacy kmbuf rungs,
/// else UserEnts; an explicit rung is taken only when its probes confirm
/// it, else the typed refusal (the caller fails the mount naming it).
/// `user` is always available. The mainline rung's zero-copy arm is not a
/// rung of its own: `zc` on `Verdicts` says whether its queues are created
/// zero-copy (`poolZeroCopy`).
pub fn resolveBufferMode(v: Verdicts, lever: Lever) ResolveError!geometry.BufferMode {
    switch (lever) {
        .user => return .user_ents,
        .bufring => return if (v.kmbuf == .present and v.bufpool == .absent) .buf_ring else error.KmbufAbsent,
        .zc => {
            if (v.kmbuf == .absent or v.bufpool == .present) return error.KmbufAbsent;
            return if (v.zc == .present) .zero_copy else error.ZeroCopyAbsent;
        },
        .pool => return if (v.bufpool == .present) .mainline_pool else error.BufpoolAbsent,
        .auto => {
            if (v.bufpool == .present) return .mainline_pool;
            if (v.kmbuf == .absent) return .user_ents;
            return if (v.zc == .present) .zero_copy else .buf_ring;
        },
    }
}

/// Whether a MainlinePool session creates its queues zero-copy: the zc
/// surface (CapEff bit 21 ∧ the sparse table) under the mode.
pub fn poolZeroCopy(mode: geometry.BufferMode, v: Verdicts) bool {
    return mode == .mainline_pool and v.zc == .present;
}

/// Whether the session's slots take the app's pages (a zero-copy session
/// of either stratum): the legacy ZeroCopy rung, or MainlinePool with its
/// queues created zero-copy.
pub fn zeroCopyArmed(mode: geometry.BufferMode, v: Verdicts) bool {
    return mode == .zero_copy or poolZeroCopy(mode, v);
}

pub fn modeGauge(m: geometry.BufferMode) u64 {
    return switch (m) {
        .user_ents => 0,
        .buf_ring => 1,
        .zero_copy => 2,
        .mainline_pool => 3,
    };
}

/// The legacy kmbuf rungs (the 7.2-sqz series' UAPI).
pub fn usesKmbuf(m: geometry.BufferMode) bool {
    return m == .buf_ring or m == .zero_copy;
}

/// The legacy COMMAND probes (`RELEASE_PAYLOAD` = 3 for retention,
/// `FEATURES` = 4 for rings per queue) may be sent only on a legacy rung:
/// their opcodes are mainline's `ADD_QUEUE`/`ADD_BUFPOOL`.
pub fn legacyProbesApply(m: geometry.BufferMode) bool {
    return usesKmbuf(m);
}

/// Publish every verdict (after the mode is decided).
pub fn publish(v: Verdicts, retention: Verdict, rings_per_queue: Verdict, time_limits: Verdict, cap: bool, mode: geometry.BufferMode) void {
    kmbuf_gauge.set(if (v.kmbuf.opcodes()) |p| p.register else 0);
    bufpool_gauge.set(v.bufpool.gauge());
    zc_gauge.set(v.zc.gauge());
    retention_gauge.set(retention.gauge());
    rings_per_queue_gauge.set(rings_per_queue.gauge());
    time_limits_gauge.set(time_limits.gauge());
    cap_gauge.set(@intFromBool(cap));
    mode_gauge.set(modeGauge(mode));
}

// ---------------------------------------------------------------------------
// The Linux drivers: the real syscalls on a scratch ring.
// ---------------------------------------------------------------------------

pub const driver = if (is_linux) struct {
    /// One rung live: a FRESH scratch ring per rung (its registration dies
    /// with the fd), the register, the repeat, the region mmap.
    pub fn probeRung(pair: abi.KmbufRegisterOpcodes, page: u32) RungVerdict {
        var ring = uring128.Ring128.init(SCRATCH_RING_ENTRIES, 0) catch |e| {
            log.err("transport probe: io_uring_setup for the kmbuf ladder failed ({s})", .{@errorName(e)});
            return .{ .refused = .{ .errno = .NOSYS, .quiet = false } };
        };
        defer ring.deinit();
        const reg: uring128.BufReg = .{ .ring_addr = page, .ring_entries = PROBE_ENTRIES, .bgid = PROBE_BGID, .flags = 0, .resv = @splat(0) };
        var steps: RungSteps = .{ .register = 0 };
        ring.registerBufRing(pair.register, reg) catch |e| {
            steps.register = -@as(i32, @intCast(@backingInt(errors.toErrno(e))));
            return rungVerdict(steps);
        };
        ring.registerBufRing(pair.register, reg) catch |e| {
            steps.repeat = -@as(i32, @intCast(@backingInt(errors.toErrno(e))));
        };
        if (ring.mmapRegion(abi.IORING_OFF_KMBUF_RING | (@as(u64, PROBE_BGID) << abi.IORING_OFF_KMBUF_SHIFT), @as(usize, PROBE_ENTRIES) * page)) |region| {
            uring128.Ring128.munmapRegion(region);
            steps.mmap_ok = true;
        } else |_| {}
        return rungVerdict(steps);
    }

    /// The ladder live. Logs the resolved pair (`kmbuf_ops=38/39`) or `absent`.
    pub fn probeKmbuf(page: u32) KmbufVerdict {
        var verdicts: [ladder.len]RungVerdict = undefined;
        var n: usize = 0;
        for (ladder) |pair| {
            const v = probeRung(pair, page);
            verdicts[n] = v;
            n += 1;
            switch (v) {
                .confirmed => break,
                .refused => |r| if (!r.quiet) log.warn("transport probe: kmbuf rung {d}/{d} refused loudly ({t}) — falling through", .{ pair.register, pair.unregister, r.errno }),
                .foreign_success => log.warn("transport probe: kmbuf rung {d}/{d}: a foreign opcode accepted the registration without the kmbuf signature — never armed", .{ pair.register, pair.unregister }),
            }
        }
        const verdict = kmbufLadder(verdicts[0..n]);
        switch (verdict) {
            .present => |p| log.info("transport probe: kmbuf_ops={d}/{d}", .{ p.register, p.unregister }),
            .absent => log.info("transport probe: kmbuf absent (stock-graceful stratum)", .{}),
        }
        return verdict;
    }

    /// A sparse table of `n` slots registers on a scratch ring (5.19+).
    pub fn probeSparse(n: u32) bool {
        var ring = uring128.Ring128.init(SCRATCH_RING_ENTRIES, 0) catch return false;
        defer ring.deinit();
        ring.registerBuffersSparse(n) catch return false;
        return true;
    }

    /// `CapEff` bit 21 of this process; the test seam forces it false.
    pub fn capSysAdmin(io: Io, env: *const std.process.Environ.Map) bool {
        if (knobs.boolKnob("SQUEEZEFS_TEST_FUSE_CAP_SYS_ADMIN_ABSENT", env)) return false;
        var buf: [4096]u8 = undefined;
        const text = Io.Dir.cwd().readFile(io, "/proc/self/status", &buf) catch return false;
        return capSysAdminFromStatus(text) orelse false;
    }

    /// `CapEff` bit 14 of this process (an unreadable word reads absent:
    /// the refusal then names the memlock lever too).
    pub fn capIpcLock(io: Io) bool {
        var buf: [4096]u8 = undefined;
        const text = Io.Dir.cwd().readFile(io, "/proc/self/status", &buf) catch return false;
        return capBitFromStatus(text, CAP_IPC_LOCK_BIT) orelse false;
    }

    /// One FUSE uring command on the PLAIN fuse fd from a scratch SQE128
    /// ring (`qid` 0, `commit_id = u64::MAX`), waited ≤ 1 s: the CQE result,
    /// or null when unanswered (the fail-safe class).
    fn probeCmd(fuse_fd: linux.fd_t, cmd_op: u32) ?i32 {
        var ring = uring128.Ring128.init(SCRATCH_RING_ENTRIES, 0) catch return null;
        defer ring.deinit();
        var cmd: [uring128.cmd_len]u8 = @splat(0);
        const req: abi.fuse_uring_cmd_req = .{ .flags = 0, .commit_id = std.math.maxInt(u64), .qid = 0, .init_flags = 0, .queue_depth = 0, .padding = @splat(0) };
        cmd[0..@sizeOf(abi.fuse_uring_cmd_req)].* = abi.encode(req);
        const sqe = ring.getSqe() orelse return null;
        uring128.prepUringCmd(sqe, .{ .raw = fuse_fd }, cmd_op, &cmd);
        sqe.user_data = 1;
        _ = ring.submitAndWaitTimed(1, RETENTION_PROBE_WAIT_NS) catch return null;
        var cqes: [1]uring128.Cqe32 = undefined;
        const n = ring.copyCqes(&cqes, 0) catch return null;
        return if (n == 1 and cqes[0].user_data == 1) cqes[0].res else null;
    }

    /// The retention probe live: one `RELEASE_PAYLOAD` with an impossible id.
    pub fn probeRetention(fuse_fd: linux.fd_t) Verdict {
        const res = probeCmd(fuse_fd, abi.FUSE_IO_URING_CMD_RELEASE_PAYLOAD);
        const v = retentionVerdict(res);
        if (res) |r| log.info("transport probe: RELEASE_PAYLOAD(commit_id=MAX) → {d} ⇒ retention {t}", .{ r, v }) else log.info("transport probe: RELEASE_PAYLOAD unanswered within {d} s ⇒ retention absent (fail-safe)", .{RETENTION_PROBE_WAIT_NS / std.time.ns_per_s});
        return v;
    }

    /// The FEATURES probe live (series patch 0030): the kernel's
    /// `FUSE_URING_FEAT_*` mask, 0 on a kernel without the command.
    pub fn probeFeatures(fuse_fd: linux.fd_t) u32 {
        const res = probeCmd(fuse_fd, abi.FUSE_IO_URING_CMD_FEATURES);
        const mask = featuresMask(res);
        if (res) |r| log.info("transport probe: FEATURES → {d} ⇒ mask 0x{x} (rings per queue {t})", .{ r, mask, ringsPerQueueVerdict(mask) }) else log.info("transport probe: FEATURES unanswered within {d} s ⇒ no features (fail-safe)", .{RETENTION_PROBE_WAIT_NS / std.time.ns_per_s});
        return mask;
    }
} else struct {};

// ---------------------------------------------------------------------------
// Tests: every verdict of every ladder, the lattice table, the CapEff parse.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn neg(e: linux.E) i32 {
    return -@as(i32, @intCast(@backingInt(e)));
}

test "one kmbuf rung: register 0 ∧ repeat EEXIST ∧ mmap ⇒ confirmed; EINVAL/ENOENT quiet refusals, any other errno loud; a success without EEXIST or without the mmap is a foreign success" {
    try testing.expectEqual(RungVerdict.confirmed, rungVerdict(.{ .register = 0, .repeat = neg(.EXIST), .mmap_ok = true }));
    try testing.expectEqual(RungVerdict{ .refused = .{ .errno = .INVAL, .quiet = true } }, rungVerdict(.{ .register = neg(.INVAL) }));
    try testing.expectEqual(RungVerdict{ .refused = .{ .errno = .NOENT, .quiet = true } }, rungVerdict(.{ .register = neg(.NOENT) }));
    try testing.expectEqual(RungVerdict{ .refused = .{ .errno = .NOMEM, .quiet = false } }, rungVerdict(.{ .register = neg(.NOMEM) }));
    try testing.expectEqual(RungVerdict{ .refused = .{ .errno = .PERM, .quiet = false } }, rungVerdict(.{ .register = neg(.PERM) }));
    try testing.expectEqual(RungVerdict.foreign_success, rungVerdict(.{ .register = 0, .repeat = 0, .mmap_ok = true }));
    try testing.expectEqual(RungVerdict.foreign_success, rungVerdict(.{ .register = 0, .repeat = neg(.BUSY), .mmap_ok = true }));
    try testing.expectEqual(RungVerdict.foreign_success, rungVerdict(.{ .register = 0, .repeat = neg(.EXIST), .mmap_ok = false }));
}

test "the kmbuf ladder: the venue pair (38/39) is probed first; confirmed short-circuits; a quiet refusal then confirmed ⇒ the second pair; every rung refused ⇒ absent; a foreign success never arms" {
    try testing.expectEqual(@as(u32, 38), ladder[0].register);
    try testing.expectEqual(@as(u32, 39), ladder[0].unregister);
    try testing.expectEqual(@as(u32, 37), ladder[1].register);
    const confirmed: RungVerdict = .confirmed;
    const quiet: RungVerdict = .{ .refused = .{ .errno = .INVAL, .quiet = true } };
    const loud: RungVerdict = .{ .refused = .{ .errno = .NOMEM, .quiet = false } };
    try testing.expectEqual(abi.KMBUF_OPCODES_7_1, kmbufLadder(&.{confirmed}).present);
    try testing.expectEqual(abi.KMBUF_OPCODES_6_19, kmbufLadder(&.{ quiet, confirmed }).present);
    try testing.expectEqual(abi.KMBUF_OPCODES_6_19, kmbufLadder(&.{ loud, confirmed }).present);
    try testing.expectEqual(KmbufVerdict.absent, kmbufLadder(&.{ quiet, quiet }));
    try testing.expectEqual(KmbufVerdict.absent, kmbufLadder(&.{ .foreign_success, .foreign_success }));
    try testing.expectEqual(KmbufVerdict.absent, kmbufLadder(&.{}));
    try testing.expect(kmbufLadder(&.{quiet}).opcodes() == null);
}

test "the retention verdict table: ENOTCONN/ENOENT present; EINVAL/EOPNOTSUPP/0/positive/EBUSY/EAGAIN/no answer absent" {
    try testing.expectEqual(Verdict.present, retentionVerdict(neg(.NOTCONN)));
    try testing.expectEqual(Verdict.present, retentionVerdict(neg(.NOENT)));
    try testing.expectEqual(Verdict.absent, retentionVerdict(neg(.INVAL)));
    try testing.expectEqual(Verdict.absent, retentionVerdict(neg(.OPNOTSUPP)));
    try testing.expectEqual(Verdict.absent, retentionVerdict(0));
    try testing.expectEqual(Verdict.absent, retentionVerdict(7));
    try testing.expectEqual(Verdict.absent, retentionVerdict(neg(.BUSY)));
    try testing.expectEqual(Verdict.absent, retentionVerdict(neg(.AGAIN)));
    try testing.expectEqual(Verdict.absent, retentionVerdict(null));
    try testing.expectEqual(@as(u64, 1), Verdict.present.gauge());
}

test "the FEATURES verdict table: a non-negative result is the mask (bit 0 ⇒ rings per queue present); EINVAL/EOPNOTSUPP (no such command), any other errno and no answer read as no features" {
    try testing.expectEqual(@as(u32, 1), featuresMask(1));
    try testing.expectEqual(Verdict.present, ringsPerQueueVerdict(featuresMask(1)));
    try testing.expectEqual(Verdict.present, ringsPerQueueVerdict(featuresMask(0b11)));
    try testing.expectEqual(Verdict.absent, ringsPerQueueVerdict(featuresMask(0b10)));
    try testing.expectEqual(Verdict.absent, ringsPerQueueVerdict(featuresMask(0)));
    try testing.expectEqual(@as(u32, 0), featuresMask(neg(.INVAL)));
    try testing.expectEqual(@as(u32, 0), featuresMask(neg(.OPNOTSUPP)));
    try testing.expectEqual(@as(u32, 0), featuresMask(neg(.NOTCONN)));
    try testing.expectEqual(@as(u32, 0), featuresMask(neg(.AGAIN)));
    try testing.expectEqual(@as(u32, 0), featuresMask(null));
}

test "the zc surface is a conjunction of a carrier (kmbuf or the mainline pool), CapEff bit 21 and the sparse table; CapEff parses from /proc/self/status text" {
    const present: KmbufVerdict = .{ .present = abi.KMBUF_OPCODES_7_1 };
    try testing.expectEqual(Verdict.present, zcVerdict(present, .absent, true, true));
    try testing.expectEqual(Verdict.absent, zcVerdict(present, .absent, false, true));
    try testing.expectEqual(Verdict.absent, zcVerdict(present, .absent, true, false));
    try testing.expectEqual(Verdict.absent, zcVerdict(.absent, .absent, true, true));
    // The mainline carrier: the pool stands in for the kmbuf ring.
    try testing.expectEqual(Verdict.present, zcVerdict(.absent, .present, true, true));
    try testing.expectEqual(Verdict.absent, zcVerdict(.absent, .present, false, true));
    try testing.expectEqual(Verdict.absent, zcVerdict(.absent, .present, true, false));
    try testing.expectEqual(@as(?bool, true), capSysAdminFromStatus("Name:\tx\nCapInh:\t0000000000000000\nCapEff:\t00000000a82465fb\nCapBnd:\t0\n"));
    try testing.expectEqual(@as(?bool, false), capSysAdminFromStatus("CapEff:\t00000000a80465fb\n"));
    try testing.expectEqual(@as(?bool, true), capSysAdminFromStatus("CapEff: 0000000000200000"));
    try testing.expectEqual(@as(?bool, null), capSysAdminFromStatus("CapPrm:\t00000000a82465fb\n"));
    try testing.expectEqual(@as(?bool, null), capSysAdminFromStatus("CapEff:\tzz\n"));
    try testing.expectEqual(@as(?bool, true), capBitFromStatus("CapEff:\t0000000000004000\n", CAP_IPC_LOCK_BIT));
    try testing.expectEqual(@as(?bool, false), capBitFromStatus("CapEff:\t0000000000200000\n", CAP_IPC_LOCK_BIT));
}

test "the ring exists at reply time iff enable_uring reads Y after the daemon's write: the first mount after boot (offer absent, parameter N when INIT was sent) still arms zc; an unreadable parameter falls back to the offer" {
    try testing.expect(ringAtReply(0, true));
    try testing.expect(ringAtReply(abi.FUSE_OVER_IO_URING, true));
    try testing.expect(!ringAtReply(abi.FUSE_OVER_IO_URING, false));
    try testing.expect(!ringAtReply(0, false));
    try testing.expect(ringAtReply(abi.FUSE_OVER_IO_URING, null));
    try testing.expect(!ringAtReply(0, null));
}

test "FUSE_TIME_LIMITS: bit 62 offered is the probe" {
    try testing.expectEqual(Verdict.present, timeLimitsVerdict(abi.FUSE_TIME_LIMITS | abi.FUSE_OVER_IO_URING));
    try testing.expectEqual(Verdict.absent, timeLimitsVerdict(abi.FUSE_OVER_IO_URING));
}

test "the lattice table: auto takes the highest confirmed rung; user always; an explicit bufring/zc the probes refuse is a typed refusal naming the probe — never a downgrade" {
    const present: KmbufVerdict = .{ .present = abi.KMBUF_OPCODES_7_1 };
    const full: Verdicts = .{ .kmbuf = present, .zc = .present };
    const kmbuf_only: Verdicts = .{ .kmbuf = present, .zc = .absent };
    const stock: Verdicts = .{ .kmbuf = .absent, .zc = .absent };
    try testing.expectEqual(geometry.BufferMode.zero_copy, try resolveBufferMode(full, .auto));
    try testing.expectEqual(geometry.BufferMode.buf_ring, try resolveBufferMode(kmbuf_only, .auto));
    try testing.expectEqual(geometry.BufferMode.user_ents, try resolveBufferMode(stock, .auto));
    try testing.expectEqual(geometry.BufferMode.user_ents, try resolveBufferMode(full, .user));
    try testing.expectEqual(geometry.BufferMode.buf_ring, try resolveBufferMode(full, .bufring));
    try testing.expectEqual(geometry.BufferMode.buf_ring, try resolveBufferMode(kmbuf_only, .bufring));
    try testing.expectEqual(geometry.BufferMode.zero_copy, try resolveBufferMode(full, .zc));
    try testing.expectError(error.KmbufAbsent, resolveBufferMode(stock, .bufring));
    try testing.expectError(error.KmbufAbsent, resolveBufferMode(stock, .zc));
    try testing.expectError(error.ZeroCopyAbsent, resolveBufferMode(kmbuf_only, .zc));
    try testing.expectError(error.BufpoolAbsent, resolveBufferMode(full, .pool));
    try testing.expectError(error.BufpoolAbsent, resolveBufferMode(stock, .pool));
    try testing.expectEqual(Lever.zc, leverFromKnob("zc"));
    try testing.expectEqual(Lever.pool, leverFromKnob("pool"));
    try testing.expectEqual(Lever.auto, leverFromKnob("auto"));
    try testing.expectEqual(@as(u64, 2), modeGauge(.zero_copy));
    try testing.expectEqual(@as(u64, 3), modeGauge(.mainline_pool));
    try testing.expect(usesKmbuf(.buf_ring) and usesKmbuf(.zero_copy) and !usesKmbuf(.user_ents) and !usesKmbuf(.mainline_pool));
    try testing.expect(zeroCopyArmed(.zero_copy, full) and !zeroCopyArmed(.buf_ring, full) and !zeroCopyArmed(.user_ents, full));
}

test "the mainline stratum: INIT bit 43 is the bufpool probe; auto takes MainlinePool over every legacy rung; its zero-copy arm follows the zc surface; the legacy command probes (opcodes 3/4, mainline's ADD_QUEUE/ADD_BUFPOOL) apply only to the legacy rungs; an explicit bufring/zc on a mainline kernel is refused naming kmbuf" {
    try testing.expectEqual(Verdict.present, bufpoolVerdict(abi.FUSE_OVER_IO_URING | abi.FUSE_HAS_IO_URING_BUFPOOL));
    try testing.expectEqual(Verdict.absent, bufpoolVerdict(abi.FUSE_OVER_IO_URING));
    try testing.expectEqual(Verdict.absent, bufpoolVerdict(abi.FUSE_HAS_SYNCFS | abi.FUSE_TIME_LIMITS));
    const mainline_zc: Verdicts = .{ .kmbuf = .absent, .bufpool = .present, .zc = .present };
    const mainline_copy: Verdicts = .{ .kmbuf = .absent, .bufpool = .present, .zc = .absent };
    // A kernel offering bit 43 resolves mainline whatever the ladder says.
    const both: Verdicts = .{ .kmbuf = .{ .present = abi.KMBUF_OPCODES_7_1 }, .bufpool = .present, .zc = .present };
    try testing.expectEqual(geometry.BufferMode.mainline_pool, try resolveBufferMode(mainline_zc, .auto));
    try testing.expectEqual(geometry.BufferMode.mainline_pool, try resolveBufferMode(mainline_copy, .auto));
    try testing.expectEqual(geometry.BufferMode.mainline_pool, try resolveBufferMode(both, .auto));
    try testing.expectEqual(geometry.BufferMode.mainline_pool, try resolveBufferMode(mainline_copy, .pool));
    try testing.expectEqual(geometry.BufferMode.user_ents, try resolveBufferMode(mainline_zc, .user));
    try testing.expectError(error.KmbufAbsent, resolveBufferMode(mainline_zc, .bufring));
    try testing.expectError(error.KmbufAbsent, resolveBufferMode(mainline_zc, .zc));
    try testing.expectError(error.KmbufAbsent, resolveBufferMode(both, .zc));
    try testing.expect(poolZeroCopy(.mainline_pool, mainline_zc));
    try testing.expect(!poolZeroCopy(.mainline_pool, mainline_copy));
    try testing.expect(!poolZeroCopy(.zero_copy, mainline_zc));
    try testing.expect(zeroCopyArmed(.mainline_pool, mainline_zc) and !zeroCopyArmed(.mainline_pool, mainline_copy));
    try testing.expect(legacyProbesApply(.buf_ring) and legacyProbesApply(.zero_copy));
    try testing.expect(!legacyProbesApply(.mainline_pool) and !legacyProbesApply(.user_ents));
    try testing.expectEqual(@as(?u32, 1), geometry.forcedWidth(.mainline_pool, null));
    publish(mainline_zc, .absent, .absent, .present, true, .mainline_pool);
    try testing.expectEqual(@as(u64, 3), mode_gauge.load());
    try testing.expectEqual(@as(u64, 1), bufpool_gauge.load());
    try testing.expectEqual(@as(u64, 1), zc_gauge.load());
    try testing.expectEqual(@as(u64, 0), kmbuf_gauge.load());
    try testing.expectEqual(@as(u64, 0), retention_gauge.load());
}

test "linux: the ladders live on this kernel — kmbuf present with its pair (logged), the sparse table registers, CapEff reads this process's capability" {
    if (!is_linux) return testkit.skip(@src(), .no_linux, "probes: Linux only");
    const page: u32 = @intCast(std.heap.pageSize());
    const v = driver.probeKmbuf(page);
    switch (v) {
        .present => |p| {
            std.debug.print("\nprobe: kmbuf present, pair {d}/{d}\n", .{ p.register, p.unregister });
            try testing.expect((p.register == 38 and p.unregister == 39) or (p.register == 37 and p.unregister == 38));
        },
        .absent => std.debug.print("\nprobe: kmbuf absent (stock kernel)\n", .{}),
    }
    try testing.expect(driver.probeSparse(33));
    var env: std.process.Environ.Map = .init(testing.allocator);
    defer env.deinit();
    var buf: [4096]u8 = undefined;
    const text = try Io.Dir.cwd().readFile(testing.io, "/proc/self/status", &buf);
    try testing.expectEqual(capSysAdminFromStatus(text) orelse false, driver.capSysAdmin(testing.io, &env));
    try env.put("SQUEEZEFS_TEST_FUSE_CAP_SYS_ADMIN_ABSENT", "1");
    try testing.expect(!driver.capSysAdmin(testing.io, &env));
}
