//! Queue isolation (charter §6.3 "claim the highest-indexed RX queues
//! (`clamp(devices, queues/4, queues/2)`), shrink RSS, ntuple-steer the
//! lane's flows, verify header/data split, revert at disarm, sweep stale
//! rules"; `docs/porting/ops_zcrx.md` §7). Portable: the derivations, the
//! `NicControl` face, the split-arm state machine, the rxq arbiter and the
//! mock NIC with its `FailAt` lattice live here; `ethtool.zig` /
//! `ethtool_nl.zig` are the Linux faces.
//!
//! Why two phases: a zcrx-bound queue yields unreadable net_iov skbs, so
//! any host flow RSS-hashed onto it reads `EFAULT` (the field's "ICResp
//! read: Bad address"), and a HW flow cache latches an established flow's
//! queue across indirection changes — so phase A (RSS exclusion) runs
//! BEFORE any TCP connect, admin included, and phase B (the ntuple rules
//! for the lane's flows) after the connects (ephemeral ports exist only
//! then). A phase-B refusal rolls back ONLY its inserts and leaves the
//! guard armed: re-including the queues in RSS while ifqs are bound is
//! exactly the EFAULT window — the caller tears the ifqs down FIRST.
//!
//! The capacity gate is READ-ONLY — the lane never flips NIC features.
//! The mlx5-class advertisement LIE (`GRXCLSRLCNT` says 0, inserts
//! succeed with kernel-assigned ids) is the `kernel_assigned` class; mlx5
//! also bounces `RX_CLS_LOC_ANY` with ENOSPC without setting the special-
//! loc flag, so that class self-selects the highest free explicit loc
//! below the `GRXCLSRLALL` size (ethtool's `rxclass_find_empty_slot`
//! parity).
//!
//! All live-arm state is INSTANCE-owned (`Registry`, decision 0002 — the
//! reference's five process-wide statics); the reserved rule slots derive
//! from the NIC (`max(channels/2, 1)`: one slot per leasable queue, so
//! flows ≡ queues 1:1 always fits — QUIRK 7's unexplained 64 replaced).

const std = @import("std");
const derive = @import("../../core/derive.zig");
const log = @import("../../core/log.zig");

pub const MAX_NICS: usize = 8;
pub const MAX_QUEUES: usize = 256;
pub const MAX_FLOWS: usize = 8;
pub const MAX_SESSIONS_PER_NIC: usize = 32;
/// ethtool's "let the driver pick" location.
pub const RX_CLS_LOC_ANY: u32 = 0xFFFF_FFFF;

// ---------------------------------------------------------------------------
// §7.1 Derivations.
// ---------------------------------------------------------------------------

pub const Range = struct { lo: u32, hi: u32 };

/// `[channels − clamp(devices, channels/4, channels/2), channels)`; a
/// sub-4-queue NIC has NO pool (never dedicate ZC queues there). Floor =
/// the standing ¼ posture; ceiling = the kernel path keeps ≥ half the RSS
/// width. Field: `clamp(10, 8, 16) = 10` — every device leases one queue.
pub fn laneEligibleQueues(channels: u32, devices: u32) ?Range {
    const quarter = channels / 4;
    if (quarter == 0) return null;
    const n: u32 = @intCast(derive.clamp(devices, quarter, channels / 2));
    return .{ .lo = channels - n, .hi = channels };
}

/// The HIGHEST-indexed `want` queues of the pool.
pub fn laneQueuePicks(channels: u32, devices: u32, want: u32, out: []u32) []u32 {
    const pool = laneEligibleQueues(channels, devices) orelse return out[0..0];
    const n = @min(@min(want, pool.hi - pool.lo), out.len);
    for (0..n) |i| out[i] = pool.hi - 1 - @as(u32, @intCast(i));
    return out[0..n];
}

/// `clamp(eligible / max(devices, 1), 1, geometry_want)` (field `clamp(10/10, 1, 4) = 1`).
pub fn fairQueueWant(eligible: u32, devices: u32, geometry_want: u32) u32 {
    return @intCast(derive.clamp(eligible / @max(devices, 1), 1, geometry_want));
}

/// One reserved rule slot per leasable queue: `max(channels/2, 1)`.
pub fn reservedSlots(channels: u32) u32 {
    return @max(channels / 2, 1);
}

/// `[table − min(reserved, table), table)`; empty when the table is 0.
pub fn reservedLocRange(table: u32, channels: u32) Range {
    const r = @min(reservedSlots(channels), table);
    return .{ .lo = table - r, .hi = table };
}

/// Non-lane entries verbatim; entries pointing at a lane queue remapped
/// round-robin over the remaining queues.
pub fn restrictedRss(prior: []const u32, lane_queues: []const u32, channels: u32, out: []u32) []u32 {
    std.debug.assert(out.len >= prior.len);
    var rotor: u32 = 0;
    for (prior, 0..) |q, i| {
        if (!contains(lane_queues, q)) {
            out[i] = q;
            continue;
        }
        var tries: u32 = 0;
        while (tries < channels) : (tries += 1) {
            const cand = rotor % @max(channels, 1);
            rotor += 1;
            if (!contains(lane_queues, cand)) {
                out[i] = cand;
                break;
            }
        } else out[i] = q;
    }
    return out[0..prior.len];
}

fn contains(set: []const u32, q: u32) bool {
    for (set) |s| if (s == q) return true;
    return false;
}

// ---------------------------------------------------------------------------
// §7.3 The NIC control face.
// ---------------------------------------------------------------------------

pub const Error = error{
    /// The ioctl/netlink refused (EOPNOTSUPP, EINVAL, …).
    NicRefused,
    /// No free rule slot (ENOSPC).
    NoSpace,
    /// The driver has no such control (a loopback, a virtio NIC).
    NotSupported,
};

pub const FlowRule = struct {
    v6: bool,
    src: [16]u8,
    dst: [16]u8,
    sport: u16,
    dport: u16,
    queue: u32,
};

pub const NicControl = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        ifname: *const fn (ctx: *anyopaque) []const u8,
        combinedChannels: *const fn (ctx: *anyopaque) Error!u32,
        tcpDataSplitOn: *const fn (ctx: *anyopaque) Error!bool,
        rxfhIndir: *const fn (ctx: *anyopaque, out: []u32) Error![]u32,
        setRxfhIndir: *const fn (ctx: *anyopaque, table: []const u32) Error!void,
        ntupleEnabled: *const fn (ctx: *anyopaque) Error!bool,
        ntupleTableSize: *const fn (ctx: *anyopaque) Error!u32,
        specialLocSupported: *const fn (ctx: *anyopaque) Error!bool,
        ntupleTableSizeHint: *const fn (ctx: *anyopaque) Error!u32,
        ntupleLocs: *const fn (ctx: *anyopaque, out: []u32) Error![]u32,
        /// Returns the EFFECTIVE location the kernel echoed.
        insertNtuple: *const fn (ctx: *anyopaque, loc: u32, rule: FlowRule) Error!u32,
        deleteNtuple: *const fn (ctx: *anyopaque, loc: u32) Error!void,
    };

    pub fn ifname(self: NicControl) []const u8 {
        return self.vtable.ifname(self.ctx);
    }
};

// ---------------------------------------------------------------------------
// §7.5 The registry, the capacity gate and the split arm.
// ---------------------------------------------------------------------------

pub const RuleSlots = union(enum) {
    reserved: Range,
    kernel_assigned,
};

pub const Refusal = error{
    /// `tcp-data-split` is not ENABLED (or the probe failed).
    NoHds,
    /// ntuple filtering is OFF (`ethtool -K <if> ntuple on`).
    NtupleOff,
    /// More lanes than free reserved slots.
    ReservedFull,
    /// The NIC face refused mid-arm (rolled back).
    NicRefused,
    /// Phase B could not place a rule.
    NoRuleSlot,
    /// No flows (or too many).
    BadFlows,
    OutOfMemory,
};

const Session = struct {
    live: bool = false,
    queues: [MAX_FLOWS]u32 = @splat(0),
    nqueues: u8 = 0,
    rule_locs: [MAX_FLOWS]u32 = @splat(0),
    nrules: u8 = 0,
};

pub const NicState = struct {
    ifindex: u32,
    pristine: ?[]u32 = null,
    sessions: [MAX_SESSIONS_PER_NIC]Session = @splat(.{}),
    live_sessions: u32 = 0,
    // The rxq arbiter (§7.2): queues in use, the retired ring in release order.
    in_use: std.bit_set.ArrayBitSet(usize, MAX_QUEUES) = .empty,
    retired: [MAX_QUEUES]u32 = @splat(0),
    retired_head: u32 = 0,
    retired_len: u32 = 0,
    // The census (§6.4): structural votes against the LIVE-armed denominator.
    armed: u32 = 0,
    structural: u32 = 0,
    released: u32 = 0,
    /// Per-NIC once-latches for the loud remedies.
    noted_capacity: bool = false,
    noted_kernel_assigned: bool = false,
};

pub const Guard = struct {
    nic: u8,
    session: u8,
    rss_touched: bool,
    registered: bool,
    restored: bool = false,
};

pub const Registry = struct {
    gpa: std.mem.Allocator,
    nics: [MAX_NICS]?NicState = @splat(null),

    pub fn init(gpa: std.mem.Allocator) Registry {
        return .{ .gpa = gpa };
    }

    pub fn deinit(self: *Registry) void {
        for (&self.nics) |*n| if (n.*) |*s| if (s.pristine) |p| self.gpa.free(p);
        self.* = undefined;
    }

    /// The NIC's slot (created on first sight).
    pub fn nic(self: *Registry, ifindex: u32) ?u8 {
        for (&self.nics, 0..) |*n, i| if (n.*) |s| if (s.ifindex == ifindex) return @intCast(i);
        for (&self.nics, 0..) |*n, i| if (n.* == null) {
            n.* = .{ .ifindex = ifindex };
            return @intCast(i);
        };
        return null;
    }

    pub fn state(self: *Registry, idx: u8) *NicState {
        return &self.nics[idx].?;
    }

    fn liveLaneQueues(s: *const NicState, out: []u32) []u32 {
        var n: usize = 0;
        for (&s.sessions) |sess| if (sess.live) {
            for (sess.queues[0..sess.nqueues]) |q| if (!contains(out[0..n], q) and n < out.len) {
                out[n] = q;
                n += 1;
            };
        };
        return out[0..n];
    }

    // ---- the capacity gate (read-only) ----

    pub fn capacityGate(self: *Registry, idx: u8, ctl: NicControl, channels: u32) Refusal!RuleSlots {
        const s = self.state(idx);
        const on = ctl.vtable.ntupleEnabled(ctl.ctx) catch return error.NicRefused;
        if (!on) {
            if (!s.noted_capacity) {
                s.noted_capacity = true;
                log.warn("zcrx steering: {s}: ntuple filtering is OFF — the lane cannot isolate its flows; remedy `ethtool -K {s} ntuple on` (the lane never flips NIC features)", .{ ctl.ifname(), ctl.ifname() });
            }
            return error.NtupleOff;
        }
        const table = ctl.vtable.ntupleTableSize(ctl.ctx) catch return error.NicRefused;
        const range = reservedLocRange(table, channels);
        if (range.lo == range.hi) {
            if (!s.noted_kernel_assigned) {
                s.noted_kernel_assigned = true;
                log.info("zcrx steering: {s} advertises a 0-size ntuple table with the feature ON (the mlx5-class lie): rule ids will be kernel-assigned", .{ctl.ifname()});
            }
            return .kernel_assigned;
        }
        return .{ .reserved = range };
    }

    // ---- phase A ----

    /// Probes (HDS, channels, the gate) BEFORE any mutation; then the RSS
    /// exclusion of the UNION of live lane queues; the session registered.
    pub fn armRssExclusion(self: *Registry, idx: u8, ctl: NicControl, lane_queues: []const u32) Refusal!Guard {
        if (lane_queues.len == 0 or lane_queues.len > MAX_FLOWS) return error.BadFlows;
        const hds = ctl.vtable.tcpDataSplitOn(ctl.ctx) catch return error.NoHds;
        if (!hds) return error.NoHds;
        const channels = ctl.vtable.combinedChannels(ctl.ctx) catch return error.NicRefused;
        const slots = try self.capacityGate(idx, ctl, channels);
        const s = self.state(idx);
        if (slots == .reserved) {
            var used: u32 = 0;
            for (&s.sessions) |sess| if (sess.live) {
                used += sess.nrules;
            };
            if (lane_queues.len + used > slots.reserved.hi - slots.reserved.lo) return error.ReservedFull;
        }
        var cur_buf: [2048]u32 = undefined;
        const cur = ctl.vtable.rxfhIndir(ctl.ctx, &cur_buf) catch return error.NicRefused;
        if (s.pristine == null) s.pristine = self.gpa.dupe(u32, cur) catch return error.OutOfMemory;
        // Register first so the union below includes us.
        const sid: u8 = for (&s.sessions, 0..) |*sess, i| {
            if (!sess.live) break @intCast(i);
        } else return error.ReservedFull;
        s.sessions[sid] = .{ .live = true, .nqueues = @intCast(lane_queues.len) };
        @memcpy(s.sessions[sid].queues[0..lane_queues.len], lane_queues);
        s.live_sessions += 1;
        var union_buf: [MAX_QUEUES]u32 = undefined;
        const all = liveLaneQueues(s, &union_buf);
        var new_buf: [2048]u32 = undefined;
        const restricted = restrictedRss(s.pristine.?, all, channels, &new_buf);
        ctl.vtable.setRxfhIndir(ctl.ctx, restricted) catch {
            s.sessions[sid].live = false;
            s.live_sessions -= 1;
            return error.NicRefused;
        };
        return .{ .nic = idx, .session = sid, .rss_touched = true, .registered = true };
    }

    // ---- phase B ----

    /// The flow rules for the lane's connections; `reserved` reaps crash
    /// residue in the range first. A refusal rolls back THIS call's inserts
    /// only — the guard stays armed (the caller tears ifqs down first).
    pub fn armFlowRules(self: *Registry, idx: u8, ctl: NicControl, guard: *Guard, flows: []const FlowRule) Refusal!void {
        if (flows.len == 0 or flows.len > MAX_FLOWS) return error.BadFlows;
        const channels = ctl.vtable.combinedChannels(ctl.ctx) catch return error.NicRefused;
        const slots = try self.capacityGate(idx, ctl, channels);
        const s = self.state(idx);
        const sess = &s.sessions[guard.session];
        var inserted: [MAX_FLOWS]u32 = undefined;
        var n: u8 = 0;
        errdefer for (inserted[0..n]) |loc| ctl.vtable.deleteNtuple(ctl.ctx, loc) catch {};
        switch (slots) {
            .reserved => |r| {
                try self.sweepStaleRules(idx, ctl, r);
                var locs_buf: [2048]u32 = undefined;
                const live = ctl.vtable.ntupleLocs(ctl.ctx, &locs_buf) catch return error.NicRefused;
                var next = r.lo;
                for (flows) |f| {
                    while (next < r.hi and (contains(live, next) or contains(inserted[0..n], next))) next += 1;
                    if (next >= r.hi) return error.NoRuleSlot;
                    const got = ctl.vtable.insertNtuple(ctl.ctx, next, f) catch |e| return switch (e) {
                        error.NoSpace => error.NoRuleSlot,
                        else => error.NicRefused,
                    };
                    inserted[n] = got;
                    n += 1;
                }
            },
            .kernel_assigned => {
                const special = ctl.vtable.specialLocSupported(ctl.ctx) catch false;
                for (flows) |f| {
                    const got: u32 = if (special)
                        ctl.vtable.insertNtuple(ctl.ctx, RX_CLS_LOC_ANY, f) catch |e| switch (e) {
                            error.NoSpace => try insertTopDown(ctl, f, inserted[0..n]),
                            else => return error.NicRefused,
                        }
                    else
                        try insertTopDown(ctl, f, inserted[0..n]);
                    inserted[n] = got;
                    n += 1;
                }
            },
        }
        @memcpy(sess.rule_locs[0..n], inserted[0..n]);
        sess.nrules = n;
    }

    /// `rxclass_find_empty_slot` parity: the highest free explicit loc below
    /// the `GRXCLSRLALL` size.
    fn insertTopDown(ctl: NicControl, f: FlowRule, taken: []const u32) Refusal!u32 {
        const size = ctl.vtable.ntupleTableSizeHint(ctl.ctx) catch return error.NicRefused;
        if (size == 0) return error.NoRuleSlot;
        var locs_buf: [2048]u32 = undefined;
        const live = ctl.vtable.ntupleLocs(ctl.ctx, &locs_buf) catch return error.NicRefused;
        var loc = size;
        while (loc > 0) {
            loc -= 1;
            if (contains(live, loc) or contains(taken, loc)) continue;
            return ctl.vtable.insertNtuple(ctl.ctx, loc, f) catch |e| switch (e) {
                error.NoSpace => continue,
                else => error.NicRefused,
            };
        }
        return error.NoRuleSlot;
    }

    /// Delete every installed rule inside the reserved range that no live
    /// session owns (rules OUTSIDE the range are operator-owned, never
    /// touched).
    pub fn sweepStaleRules(self: *Registry, idx: u8, ctl: NicControl, range: Range) Refusal!void {
        const s = self.state(idx);
        var locs_buf: [2048]u32 = undefined;
        const live = ctl.vtable.ntupleLocs(ctl.ctx, &locs_buf) catch return error.NicRefused;
        for (live) |loc| {
            if (loc < range.lo or loc >= range.hi) continue;
            var owned = false;
            for (&s.sessions) |sess| if (sess.live and contains(sess.rule_locs[0..sess.nrules], loc)) {
                owned = true;
            };
            if (owned) continue;
            ctl.vtable.deleteNtuple(ctl.ctx, loc) catch return error.NicRefused;
            log.warn("zcrx steering: {s}: swept a stale rule at loc {d} inside the lane's reserved range (crash residue)", .{ ctl.ifname(), loc });
        }
    }

    // ---- restore ----

    /// Idempotent; collects every failure loud. Delete this guard's rules;
    /// deregister; RSS = pristine with the surviving PEERS' queues still
    /// excluded — the LAST restore writes the pristine table, so restores
    /// converge in ANY order.
    pub fn restore(self: *Registry, ctl: NicControl, guard: *Guard) void {
        if (guard.restored) return;
        guard.restored = true;
        const s = self.state(guard.nic);
        const sess = &s.sessions[guard.session];
        for (sess.rule_locs[0..sess.nrules]) |loc| ctl.vtable.deleteNtuple(ctl.ctx, loc) catch |e| {
            log.err("zcrx steering: {s}: delete of lane rule {d} failed ({s}) — NIC state may be leaked; `ethtool -N {s} delete {d}`", .{ ctl.ifname(), loc, @errorName(e), ctl.ifname(), loc });
        };
        sess.nrules = 0;
        if (guard.registered) {
            sess.live = false;
            s.live_sessions -= 1;
            guard.registered = false;
        }
        if (guard.rss_touched) {
            guard.rss_touched = false;
            const pristine = s.pristine orelse return;
            var union_buf: [MAX_QUEUES]u32 = undefined;
            const peers = liveLaneQueues(s, &union_buf);
            const channels = ctl.vtable.combinedChannels(ctl.ctx) catch 0;
            var new_buf: [2048]u32 = undefined;
            const table = if (peers.len == 0) pristine else restrictedRss(pristine, peers, channels, &new_buf);
            ctl.vtable.setRxfhIndir(ctl.ctx, table) catch |e| {
                log.err("zcrx steering: {s}: RSS restore failed ({s}) — the NIC's indirection table is left restricted; `ethtool -X {s} default`", .{ ctl.ifname(), @errorName(e), ctl.ifname() });
            };
        }
    }

    // ---- §7.2 the rxq arbiter ----

    pub const Lease = struct {
        nic: u8,
        queues: [MAX_FLOWS]u32 = @splat(0),
        n: u8 = 0,

        pub fn slice(self: *const Lease) []const u32 {
            return self.queues[0..self.n];
        }
    };

    pub const AcquireError = error{
        /// `want == 0`, or the pool is empty ("needs ≥ 4 queues").
        PoolEmpty,
        /// Every pool queue is in use or recently freed and none can rotate.
        Exhausted,
    };

    /// Distinct queues per session: the pure picks that are FRESH (neither
    /// in use nor recently freed), highest first; then the LEAST-recently
    /// freed (the kernel's ifq teardown is ASYNC — ring-fd close defers the
    /// unregister — so an instantly re-granted index re-registers into
    /// EEXIST). A partial grant degrades the queue count with a WARN.
    pub fn acquire(self: *Registry, idx: u8, ifname: []const u8, channels: u32, devices: u32, want: u32) AcquireError!Lease {
        const s = self.state(idx);
        if (want == 0) return error.PoolEmpty;
        const pool = laneEligibleQueues(channels, devices) orelse {
            log.warn("zcrx steering: {s} has {d} RX queue(s): the lane needs ≥ 4 to dedicate any (clamp(devices, channels/4, channels/2) is empty); remedy `ethtool -L {s} combined <N>`", .{ ifname, channels, ifname });
            return error.PoolEmpty;
        };
        const goal = @min(want, pool.hi - pool.lo);
        var lease: Lease = .{ .nic = idx };
        // The pure picks (highest-indexed) that are FRESH.
        var picks_buf: [MAX_QUEUES]u32 = undefined;
        for (laneQueuePicks(channels, devices, pool.hi - pool.lo, &picks_buf)) |q| {
            if (lease.n == goal) break;
            if (s.in_use.isSet(q) or self.isRetired(s, q)) continue;
            lease.queues[lease.n] = q;
            lease.n += 1;
        }
        while (lease.n < goal and s.retired_len > 0) {
            const r = s.retired[s.retired_head % MAX_QUEUES];
            s.retired_head += 1;
            s.retired_len -= 1;
            if (r < pool.lo or r >= pool.hi or s.in_use.isSet(r)) continue;
            lease.queues[lease.n] = r;
            lease.n += 1;
        }
        if (lease.n == 0) {
            log.warn("zcrx steering: {s}: no free lane queue — channels {d}, pool [{d}, {d}) = clamp(devices {d}, /4, /2), {d} leased, {d} wanted; remedy `ethtool -L {s} combined <N>`", .{ ifname, channels, pool.lo, pool.hi, devices, s.in_use.count(), want, ifname });
            return error.Exhausted;
        }
        if (lease.n < goal) log.warn("zcrx steering: {s}: partial grant {d} of {d} lane queues", .{ ifname, lease.n, goal });
        for (lease.slice()) |lq| s.in_use.set(lq);
        return lease;
    }

    fn isRetired(self: *Registry, s: *const NicState, q: u32) bool {
        _ = self;
        for (0..s.retired_len) |i| if (s.retired[(s.retired_head + i) % MAX_QUEUES] == q) return true;
        return false;
    }

    /// Release to the BACK of the retired ring.
    pub fn release(self: *Registry, lease: *const Lease) void {
        const s = self.state(lease.nic);
        for (lease.slice()) |q| {
            s.in_use.unset(q);
            if (s.retired_len < MAX_QUEUES) {
                s.retired[(s.retired_head + s.retired_len) % MAX_QUEUES] = q;
                s.retired_len += 1;
            }
        }
    }

    // ---- §6.4 the whole-NIC census ----

    pub fn noteArmed(self: *Registry, idx: u8) void {
        self.state(idx).armed += 1;
    }

    pub fn noteReleased(self: *Registry, idx: u8) void {
        self.state(idx).released += 1;
    }

    /// `structural × 2 ≥ max(armed − released, 1)` — the LIVE-armed
    /// denominator; the voter votes while still counted. True ⇒ the caller
    /// sweeps every other live session on the NIC (depth-one recursion: a
    /// swept peer is never structural). Round 2: 5 of 10 tore down and the
    /// survivors held ~16 % of queue width at ~0 engagement.
    pub fn noteStructural(self: *Registry, idx: u8) bool {
        const s = self.state(idx);
        s.structural += 1;
        return s.structural * 2 >= @max(s.armed -| s.released, 1);
    }
};

// ---------------------------------------------------------------------------
// The mock NIC (tests) with the `FailAt` lattice.
// ---------------------------------------------------------------------------

pub const FailAt = enum { none, channels, hds, rss_read, rss_write, ntuple_enabled, table_size, insert, delete };

pub const MockNic = struct {
    name: []const u8 = "mock0",
    channels: u32 = 32,
    hds: bool = true,
    rss: [128]u32 = undefined,
    rss_len: usize = 128,
    ntuple: bool = true,
    /// `GRXCLSRLCNT`: the advertised table size (0 = the mlx5 lie).
    table_size: u32 = 128,
    /// `GRXCLSRLALL`'s `data`.
    table_hint: u32 = 128,
    special_loc: bool = false,
    rules: [1024]?FlowRule = @splat(null),
    fail_at: FailAt = .none,
    rss_writes: u32 = 0,
    inserts: u32 = 0,
    deletes: u32 = 0,
    /// For the mlx5 lattice: `@ANY` bounces with ENOSPC.
    any_bounces: bool = false,

    pub fn init() MockNic {
        var m: MockNic = .{};
        for (&m.rss, 0..) |*e, i| e.* = @intCast(i % 32);
        return m;
    }

    pub fn control(self: *MockNic) NicControl {
        return .{ .ctx = self, .vtable = &vtable };
    }

    pub fn ruleCount(self: *const MockNic) u32 {
        var n: u32 = 0;
        for (self.rules) |r| if (r != null) {
            n += 1;
        };
        return n;
    }

    pub fn rssIsIdentity(self: *const MockNic) bool {
        for (self.rss[0..self.rss_len], 0..) |e, i| if (e != i % 32) return false;
        return true;
    }

    const vtable: NicControl.VTable = .{
        .ifname = &ifname,
        .combinedChannels = &combinedChannels,
        .tcpDataSplitOn = &tcpDataSplitOn,
        .rxfhIndir = &rxfhIndir,
        .setRxfhIndir = &setRxfhIndir,
        .ntupleEnabled = &ntupleEnabled,
        .ntupleTableSize = &ntupleTableSize,
        .specialLocSupported = &specialLocSupported,
        .ntupleTableSizeHint = &ntupleTableSizeHint,
        .ntupleLocs = &ntupleLocs,
        .insertNtuple = &insertNtuple,
        .deleteNtuple = &deleteNtuple,
    };

    fn me(ctx: *anyopaque) *MockNic {
        return @ptrCast(@alignCast(ctx));
    }
    fn ifname(ctx: *anyopaque) []const u8 {
        return me(ctx).name;
    }
    fn combinedChannels(ctx: *anyopaque) Error!u32 {
        const m = me(ctx);
        if (m.fail_at == .channels) return error.NicRefused;
        return m.channels;
    }
    fn tcpDataSplitOn(ctx: *anyopaque) Error!bool {
        const m = me(ctx);
        if (m.fail_at == .hds) return error.NicRefused;
        return m.hds;
    }
    fn rxfhIndir(ctx: *anyopaque, out: []u32) Error![]u32 {
        const m = me(ctx);
        if (m.fail_at == .rss_read) return error.NicRefused;
        @memcpy(out[0..m.rss_len], m.rss[0..m.rss_len]);
        return out[0..m.rss_len];
    }
    fn setRxfhIndir(ctx: *anyopaque, table: []const u32) Error!void {
        const m = me(ctx);
        if (m.fail_at == .rss_write) return error.NicRefused;
        @memcpy(m.rss[0..table.len], table);
        m.rss_len = table.len;
        m.rss_writes += 1;
    }
    fn ntupleEnabled(ctx: *anyopaque) Error!bool {
        const m = me(ctx);
        if (m.fail_at == .ntuple_enabled) return error.NicRefused;
        return m.ntuple;
    }
    fn ntupleTableSize(ctx: *anyopaque) Error!u32 {
        const m = me(ctx);
        if (m.fail_at == .table_size) return error.NicRefused;
        return m.table_size;
    }
    fn specialLocSupported(ctx: *anyopaque) Error!bool {
        return me(ctx).special_loc;
    }
    fn ntupleTableSizeHint(ctx: *anyopaque) Error!u32 {
        return me(ctx).table_hint;
    }
    fn ntupleLocs(ctx: *anyopaque, out: []u32) Error![]u32 {
        const m = me(ctx);
        var n: usize = 0;
        for (m.rules, 0..) |r, i| if (r != null) {
            out[n] = @intCast(i);
            n += 1;
        };
        return out[0..n];
    }
    fn insertNtuple(ctx: *anyopaque, loc: u32, rule: FlowRule) Error!u32 {
        const m = me(ctx);
        if (m.fail_at == .insert) return error.NicRefused;
        var at = loc;
        if (loc == RX_CLS_LOC_ANY) {
            if (m.any_bounces) return error.NoSpace;
            // The kernel picks the highest free id.
            at = m.table_hint;
            while (at > 0) {
                at -= 1;
                if (m.rules[at] == null) break;
            } else return error.NoSpace;
        }
        if (at >= m.rules.len or m.rules[at] != null) return error.NoSpace;
        m.rules[at] = rule;
        m.inserts += 1;
        return at;
    }
    fn deleteNtuple(ctx: *anyopaque, loc: u32) Error!void {
        const m = me(ctx);
        if (m.fail_at == .delete) return error.NicRefused;
        if (loc >= m.rules.len or m.rules[loc] == null) return error.NicRefused;
        m.rules[loc] = null;
        m.deletes += 1;
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn flow(q: u32, port: u16) FlowRule {
    return .{ .v6 = false, .src = @splat(0), .dst = @splat(0), .sport = port, .dport = 4420, .queue = q };
}

test "eligible pool ties: (32,10)→10, (32,5)→8, (32,0)→8, (32,100)→16, (64,20)→20, (3,10)→empty; picks are the highest-indexed; the fair want; the reserved range" {
    try testing.expectEqual(Range{ .lo = 22, .hi = 32 }, laneEligibleQueues(32, 10).?);
    try testing.expectEqual(Range{ .lo = 24, .hi = 32 }, laneEligibleQueues(32, 5).?);
    try testing.expectEqual(Range{ .lo = 24, .hi = 32 }, laneEligibleQueues(32, 0).?);
    try testing.expectEqual(Range{ .lo = 16, .hi = 32 }, laneEligibleQueues(32, 100).?);
    try testing.expectEqual(Range{ .lo = 44, .hi = 64 }, laneEligibleQueues(64, 20).?);
    try testing.expect(laneEligibleQueues(3, 10) == null);
    try testing.expect(laneEligibleQueues(1, 1) == null);
    var buf: [8]u32 = undefined;
    try testing.expectEqualSlices(u32, &.{ 31, 30, 29 }, laneQueuePicks(32, 10, 3, &buf));
    try testing.expectEqualSlices(u32, &.{}, laneQueuePicks(3, 10, 3, &buf));
    try testing.expectEqual(@as(usize, 8), laneQueuePicks(32, 5, 100, &buf).len);
    try testing.expectEqual(@as(u32, 1), fairQueueWant(10, 10, 4));
    try testing.expectEqual(@as(u32, 4), fairQueueWant(16, 2, 4));
    try testing.expectEqual(@as(u32, 1), fairQueueWant(0, 0, 8));
    try testing.expectEqual(@as(u32, 16), reservedSlots(32));
    try testing.expectEqual(@as(u32, 1), reservedSlots(1));
    try testing.expectEqual(Range{ .lo = 112, .hi = 128 }, reservedLocRange(128, 32));
    try testing.expectEqual(Range{ .lo = 0, .hi = 8 }, reservedLocRange(8, 32));
    try testing.expectEqual(Range{ .lo = 0, .hi = 0 }, reservedLocRange(0, 32));
}

test "restricted RSS: lane entries remapped round-robin over the rest, non-lane entries verbatim" {
    const prior = [_]u32{ 0, 1, 2, 3, 0, 1, 2, 3 };
    var out: [8]u32 = undefined;
    const r = restrictedRss(&prior, &.{ 3, 2 }, 4, &out);
    try testing.expectEqualSlices(u32, &.{ 0, 1, 0, 1, 0, 1, 0, 1 }, r);
    for (r) |q| try testing.expect(q != 2 and q != 3);
}

test "phase A applies the RSS restriction and registers; phase B installs reserved rules at the first free slots; restore converges to pristine with zero rules" {
    var reg = Registry.init(testing.allocator);
    defer reg.deinit();
    var nic = MockNic.init();
    const idx = reg.nic(6).?;
    var g = try reg.armRssExclusion(idx, nic.control(), &.{ 31, 30 });
    try testing.expectEqual(@as(u32, 1), nic.rss_writes);
    for (nic.rss[0..nic.rss_len]) |q| try testing.expect(q != 31 and q != 30);
    try reg.armFlowRules(idx, nic.control(), &g, &.{ flow(31, 1000), flow(30, 1001) });
    try testing.expectEqual(@as(u32, 2), nic.ruleCount());
    // The reserved range of a 128-table / 32-channel NIC is [112, 128).
    try testing.expect(nic.rules[112] != null and nic.rules[113] != null);
    reg.restore(nic.control(), &g);
    try testing.expectEqual(@as(u32, 0), nic.ruleCount());
    try testing.expect(nic.rssIsIdentity());
    try testing.expectEqual(@as(u32, 0), reg.state(idx).live_sessions);
    // Idempotent.
    reg.restore(nic.control(), &g);
    try testing.expectEqual(@as(u32, 2), nic.rss_writes);
}

test "arm failure at every step leaves the NIC untouched (the FailAt lattice); HDS off refuses with zero mutations; ntuple off refuses" {
    inline for (.{ .channels, .hds, .rss_read, .rss_write, .ntuple_enabled, .table_size }) |f| {
        var reg = Registry.init(testing.allocator);
        defer reg.deinit();
        var nic = MockNic.init();
        nic.fail_at = f;
        const idx = reg.nic(1).?;
        try testing.expectError(switch (f) {
            .hds => error.NoHds,
            else => error.NicRefused,
        }, reg.armRssExclusion(idx, nic.control(), &.{31}));
        try testing.expect(nic.rssIsIdentity());
        try testing.expectEqual(@as(u32, 0), nic.ruleCount());
        try testing.expectEqual(@as(u32, 0), reg.state(idx).live_sessions);
    }
    var reg = Registry.init(testing.allocator);
    defer reg.deinit();
    var nic = MockNic.init();
    nic.hds = false;
    const idx = reg.nic(2).?;
    try testing.expectError(error.NoHds, reg.armRssExclusion(idx, nic.control(), &.{31}));
    try testing.expectEqual(@as(u32, 0), nic.rss_writes);
    nic.hds = true;
    nic.ntuple = false;
    try testing.expectError(error.NtupleOff, reg.armRssExclusion(idx, nic.control(), &.{31}));
    try testing.expectEqual(@as(u32, 0), nic.rss_writes);
    try testing.expectError(error.BadFlows, reg.armRssExclusion(idx, nic.control(), &.{}));
}

test "crash-residue reap deletes only rules inside the reserved range that no live session owns; a phase-B refusal rolls back exactly its inserts and keeps the guard armed" {
    var reg = Registry.init(testing.allocator);
    defer reg.deinit();
    var nic = MockNic.init();
    nic.rules[5] = flow(1, 5); // operator-owned, outside the range
    nic.rules[120] = flow(9, 9); // residue inside [112, 128)
    const idx = reg.nic(3).?;
    var g = try reg.armRssExclusion(idx, nic.control(), &.{31});
    try reg.armFlowRules(idx, nic.control(), &g, &.{flow(31, 1)});
    try testing.expect(nic.rules[5] != null);
    try testing.expect(nic.rules[120] == null);
    try testing.expectEqual(@as(u32, 2), nic.ruleCount());
    // A second session whose insert fails: its inserts rolled back, the
    // first session's rule and both exclusions intact.
    var g2 = try reg.armRssExclusion(idx, nic.control(), &.{30});
    nic.fail_at = .insert;
    try testing.expectError(error.NicRefused, reg.armFlowRules(idx, nic.control(), &g2, &.{flow(30, 2)}));
    try testing.expectEqual(@as(u32, 2), nic.ruleCount());
    for (nic.rss[0..nic.rss_len]) |q| try testing.expect(q != 31 and q != 30);
    try testing.expect(g2.registered);
    nic.fail_at = .none;
    reg.restore(nic.control(), &g2);
    for (nic.rss[0..nic.rss_len]) |q| try testing.expect(q != 31);
    reg.restore(nic.control(), &g);
    try testing.expect(nic.rssIsIdentity());
    try testing.expectEqual(@as(u32, 1), nic.ruleCount()); // the operator's rule survives
}

test "two sessions on one NIC: the second arm preserves the live session's rules and locs; out-of-order restore converges to pristine" {
    var reg = Registry.init(testing.allocator);
    defer reg.deinit();
    var nic = MockNic.init();
    const idx = reg.nic(4).?;
    var a = try reg.armRssExclusion(idx, nic.control(), &.{31});
    try reg.armFlowRules(idx, nic.control(), &a, &.{flow(31, 1)});
    var b = try reg.armRssExclusion(idx, nic.control(), &.{30});
    try reg.armFlowRules(idx, nic.control(), &b, &.{flow(30, 2)});
    try testing.expect(nic.rules[112] != null and nic.rules[113] != null);
    for (nic.rss[0..nic.rss_len]) |q| try testing.expect(q != 31 and q != 30);
    // Restore A first: B's queue stays excluded.
    reg.restore(nic.control(), &a);
    try testing.expect(nic.rules[112] == null and nic.rules[113] != null);
    var saw31 = false;
    for (nic.rss[0..nic.rss_len]) |q| {
        try testing.expect(q != 30);
        if (q == 31) saw31 = true;
    }
    try testing.expect(saw31);
    reg.restore(nic.control(), &b);
    try testing.expect(nic.rssIsIdentity());
    try testing.expectEqual(@as(u32, 0), nic.ruleCount());
}

test "the mlx5 lattice: a 0-size advertised table arms via kernel-assigned ids (@ANY when supported, else the highest free explicit loc) and restores; @ANY bouncing falls back to explicit" {
    var reg = Registry.init(testing.allocator);
    defer reg.deinit();
    var nic = MockNic.init();
    nic.table_size = 0;
    nic.table_hint = 1024;
    nic.special_loc = true;
    const idx = reg.nic(5).?;
    var g = try reg.armRssExclusion(idx, nic.control(), &.{31});
    try reg.armFlowRules(idx, nic.control(), &g, &.{flow(31, 1)});
    try testing.expect(nic.rules[1023] != null);
    try testing.expectEqual(@as(u32, 1023), reg.state(idx).sessions[g.session].rule_locs[0]);
    reg.restore(nic.control(), &g);
    try testing.expectEqual(@as(u32, 0), nic.ruleCount());
    // mlx5 proper: no special-loc flag, @ANY would bounce — explicit top-down.
    nic.special_loc = false;
    nic.any_bounces = true;
    nic.rules[1023] = flow(0, 0);
    var h = try reg.armRssExclusion(idx, nic.control(), &.{31});
    try reg.armFlowRules(idx, nic.control(), &h, &.{flow(31, 1)});
    try testing.expect(nic.rules[1022] != null);
    reg.restore(nic.control(), &h);
    try testing.expectEqual(@as(u32, 1), nic.ruleCount());
    // A 0 hint refuses.
    nic.table_hint = 0;
    var k = try reg.armRssExclusion(idx, nic.control(), &.{31});
    try testing.expectError(error.NoRuleSlot, reg.armFlowRules(idx, nic.control(), &k, &.{flow(31, 1)}));
    reg.restore(nic.control(), &k);
}

test "the reserved class refuses more lanes than free slots up front" {
    var reg = Registry.init(testing.allocator);
    defer reg.deinit();
    var nic = MockNic.init();
    nic.table_size = 2;
    nic.channels = 4;
    const idx = reg.nic(7).?;
    // reservedSlots(4) = 2, table 2 ⇒ range [0, 2): 3 lanes refuse.
    try testing.expectError(error.ReservedFull, reg.armRssExclusion(idx, nic.control(), &.{ 3, 2, 1 }));
    try testing.expectEqual(@as(u32, 0), nic.rss_writes);
}

test "rxq arbiter: two sessions get distinct queues; exhaustion refuses with the derived numbers; release then re-acquire rotates the pool before reusing a retired index; a narrow NIC has no pool" {
    var reg = Registry.init(testing.allocator);
    defer reg.deinit();
    const idx = reg.nic(10).?;
    const a = try reg.acquire(idx, "nic", 32, 10, 2);
    try testing.expectEqualSlices(u32, &.{ 31, 30 }, a.slice());
    const b = try reg.acquire(idx, "nic", 32, 10, 2);
    try testing.expectEqualSlices(u32, &.{ 29, 28 }, b.slice());
    // The pool [22, 32) has 6 left; a 10-wide ask degrades to 6.
    const c = try reg.acquire(idx, "nic", 32, 10, 10);
    try testing.expectEqual(@as(u8, 6), c.n);
    try testing.expectError(error.Exhausted, reg.acquire(idx, "nic", 32, 10, 1));
    try testing.expectError(error.PoolEmpty, reg.acquire(idx, "nic", 32, 10, 0));
    // Release A: its queues are RETIRED; the next ask prefers them only once
    // nothing fresh remains (fresh = never used); here nothing is fresh, so
    // the least-recently-freed comes back.
    reg.release(&a);
    const d = try reg.acquire(idx, "nic", 32, 10, 1);
    try testing.expectEqualSlices(u32, &.{31}, d.slice());
    // With fresh queues available the retired ones are skipped.
    var reg2 = Registry.init(testing.allocator);
    defer reg2.deinit();
    const nic2 = reg2.nic(11).?;
    const e = try reg2.acquire(nic2, "nic", 32, 10, 1);
    reg2.release(&e);
    const f = try reg2.acquire(nic2, "nic", 32, 10, 1);
    try testing.expectEqualSlices(u32, &.{30}, f.slice());
    try testing.expectError(error.PoolEmpty, reg2.acquire(nic2, "lo", 1, 1, 1));
}

test "the census: structural × 2 ≥ live-armed sweeps the NIC (the voter votes while counted); the field tape" {
    var reg = Registry.init(testing.allocator);
    defer reg.deinit();
    const idx = reg.nic(20).?;
    for (0..10) |_| reg.noteArmed(idx);
    for (0..4) |_| try testing.expect(!reg.noteStructural(idx));
    try testing.expect(reg.noteStructural(idx));
    // The live denominator: released sessions leave it.
    var r2 = Registry.init(testing.allocator);
    defer r2.deinit();
    const j = r2.nic(21).?;
    for (0..4) |_| r2.noteArmed(j);
    r2.noteReleased(j);
    r2.noteReleased(j);
    try testing.expect(r2.noteStructural(j));
    // A sole session is structural at its first vote.
    var r3 = Registry.init(testing.allocator);
    defer r3.deinit();
    const k = r3.nic(22).?;
    r3.noteArmed(k);
    try testing.expect(r3.noteStructural(k));
}
